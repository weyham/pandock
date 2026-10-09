use fs2::FileExt;
use pandock_core::update::install::{
    apply_staging_files, prepare_rollback, readiness_path, restore_rollback,
};
use pandock_core::update::journal::{JournalState, UpdateJournal};
use pandock_core::update::verify::default_install_allowlist;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const READINESS_TIMEOUT: Duration = Duration::from_secs(60);
const ROLLBACK_LAUNCH_GRACE: Duration = Duration::from_secs(2);
const PARENT_EXIT_TIMEOUT: Duration = Duration::from_secs(60);

fn main() {
    if let Err(error) = run() {
        eprintln!("pandock-updater: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse()?;
    if args.protocol != 1 {
        return Err(format!("unsupported protocol {}", args.protocol));
    }

    let journal_path = PathBuf::from(&args.journal);
    let mut journal = UpdateJournal::load(&journal_path).map_err(|error| error.to_string())?;
    validate_args(&args, &journal)?;

    if matches!(journal.state, JournalState::Completed) {
        return Ok(());
    }

    let app_dir = PathBuf::from(&journal.app_dir);
    let staging_dir = PathBuf::from(&journal.staging_dir);
    let rollback_dir = PathBuf::from(&journal.rollback_dir);
    let lock_path = app_dir.join(".update.lock");
    let _lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("无法创建更新锁: {error}"))?;
    _lock
        .try_lock_exclusive()
        .map_err(|error| format!("无法获取更新锁: {error}"))?;
    journal
        .transition(JournalState::LockAcquired)
        .map_err(|error| error.to_string())?;
    journal
        .save(&journal_path)
        .map_err(|error| error.to_string())?;

    if process_is_running(args.parent_pid) {
        wait_for_parent_exit(args.parent_pid)?;
    }
    journal
        .transition(JournalState::ParentExited)
        .map_err(|error| error.to_string())?;
    journal
        .save(&journal_path)
        .map_err(|error| error.to_string())?;

    let apply_allowlist = default_install_allowlist(None);
    let backed_up = prepare_rollback(&app_dir, &rollback_dir, &apply_allowlist)
        .map_err(|error| error.to_string())?;
    journal
        .transition(JournalState::BackupComplete)
        .map_err(|error| error.to_string())?;
    journal
        .save(&journal_path)
        .map_err(|error| error.to_string())?;

    journal
        .transition(JournalState::ReplaceStarted)
        .map_err(|error| error.to_string())?;
    journal
        .save(&journal_path)
        .map_err(|error| error.to_string())?;
    let replaced = apply_staging_files(&staging_dir, &app_dir, &rollback_dir, &apply_allowlist)
        .map_err(|error| error.to_string())?;
    journal
        .transition(JournalState::ReplaceComplete)
        .map_err(|error| error.to_string())?;
    journal
        .save(&journal_path)
        .map_err(|error| error.to_string())?;

    journal
        .transition(JournalState::LaunchStarted)
        .map_err(|error| error.to_string())?;
    journal
        .save(&journal_path)
        .map_err(|error| error.to_string())?;

    let mut child = match launch_new_version(&args, &journal) {
        Ok(child) => child,
        Err(error) => {
            return rollback_and_launch_old(
                &args,
                &mut journal,
                &journal_path,
                &app_dir,
                &rollback_dir,
                &backed_up,
                &format!("新版本启动失败: {error}"),
            );
        }
    };

    let marker = readiness_path(&app_dir, &journal.readiness_token);
    if !wait_for_readiness(&marker, readiness_timeout()) {
        let _ = child.kill();
        let _ = child.wait();
        return rollback_and_launch_old(
            &args,
            &mut journal,
            &journal_path,
            &app_dir,
            &rollback_dir,
            &backed_up,
            "新版本未在超时时间内写入 readiness",
        );
    }

    journal
        .transition(JournalState::Ready)
        .map_err(|error| error.to_string())?;
    journal
        .save(&journal_path)
        .map_err(|error| error.to_string())?;
    let _ = fs::remove_dir_all(&staging_dir);
    journal
        .transition(JournalState::Completed)
        .map_err(|error| error.to_string())?;
    journal
        .save(&journal_path)
        .map_err(|error| error.to_string())?;
    let _ = replaced;
    Ok(())
}

fn validate_args(args: &Args, journal: &UpdateJournal) -> Result<(), String> {
    if journal.to_version != args.expected_version {
        return Err("journal 版本与参数不匹配".into());
    }
    if journal.parent_pid != args.parent_pid {
        return Err("journal parent pid 与参数不匹配".into());
    }
    if journal.readiness_token != args.readiness_token {
        return Err("journal readiness token 与参数不匹配".into());
    }
    let app_dir = Path::new(&journal.app_dir);
    if canonical_or_original(app_dir) != canonical_or_original(Path::new(&args.target_app)) {
        return Err("journal app 目录与参数不匹配".into());
    }
    let expected_helper = PathBuf::from(&journal.helper_path);
    let current_exe = std::env::current_exe().map_err(|error| error.to_string())?;
    if canonical_or_original(&expected_helper) != canonical_or_original(&current_exe) {
        return Err("helper 运行路径与 journal 不匹配".into());
    }
    Ok(())
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn process_is_running(pid: u32) -> bool {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    system.process(sysinfo::Pid::from_u32(pid)).is_some()
}

fn wait_for_parent_exit(pid: u32) -> Result<(), String> {
    let deadline = Instant::now() + PARENT_EXIT_TIMEOUT;
    while process_is_running(pid) {
        if Instant::now() >= deadline {
            return Err("等待主程序退出超时".into());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn launch_new_version(args: &Args, journal: &UpdateJournal) -> Result<std::process::Child, String> {
    let launch = PathBuf::from(&args.launch);
    Command::new(launch)
        .arg("--readiness-token")
        .arg(&journal.readiness_token)
        .current_dir(Path::new(&journal.app_dir))
        .spawn()
        .map_err(|error| error.to_string())
}

fn rollback_and_launch_old(
    args: &Args,
    journal: &mut UpdateJournal,
    journal_path: &Path,
    app_dir: &Path,
    rollback_dir: &Path,
    backed_up: &[String],
    reason: &str,
) -> Result<(), String> {
    journal
        .transition(JournalState::RollbackStarted)
        .map_err(|error| error.to_string())?;
    journal.error = Some(reason.to_string());
    journal
        .save(journal_path)
        .map_err(|error| error.to_string())?;

    if let Err(error) = restore_rollback(app_dir, rollback_dir, backed_up) {
        journal
            .fail(format!(
                "{reason}; 旧文件恢复失败: {error}; manual repair required"
            ))
            .map_err(|error| error.to_string())?;
        journal
            .save(journal_path)
            .map_err(|error| error.to_string())?;
        return Err(format!("旧文件恢复失败，需要人工修复: {error}"));
    }
    journal
        .transition(JournalState::RolledBack)
        .map_err(|error| error.to_string())?;
    journal.error = Some(reason.to_string());
    journal
        .save(journal_path)
        .map_err(|error| error.to_string())?;

    match launch_old_version(args, journal) {
        Ok(mut child) => {
            let deadline = Instant::now() + ROLLBACK_LAUNCH_GRACE;
            loop {
                match child.try_wait() {
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Ok(None) => {
                        journal.error = Some(format!("{reason}; 旧版本已重新启动"));
                        journal
                            .save(journal_path)
                            .map_err(|error| error.to_string())?;
                        return Err(format!("{reason}; 已回滚并重新启动旧版本"));
                    }
                    Ok(Some(status)) => {
                        return mark_manual_repair(
                            journal,
                            journal_path,
                            format!(
                                "{reason}; old version exited immediately (status={status:?}); manual repair required"
                            ),
                        );
                    }
                    Err(error) => {
                        return mark_manual_repair(
                            journal,
                            journal_path,
                            format!(
                                "{reason}; 无法确认旧版本运行状态: {error}；manual repair required"
                            ),
                        );
                    }
                }
            }
        }
        Err(error) => mark_manual_repair(
            journal,
            journal_path,
            format!("{reason}; 旧版本启动失败: {error}; manual repair required"),
        ),
    }
}

fn mark_manual_repair(
    journal: &mut UpdateJournal,
    journal_path: &Path,
    message: impl Into<String>,
) -> Result<(), String> {
    let message = message.into();
    journal
        .fail(message.clone())
        .map_err(|error| error.to_string())?;
    journal
        .save(journal_path)
        .map_err(|error| error.to_string())?;
    Err(message)
}

fn launch_old_version(args: &Args, journal: &UpdateJournal) -> Result<std::process::Child, String> {
    Command::new(&args.launch)
        .arg("--readiness-token")
        .arg(&journal.readiness_token)
        .current_dir(Path::new(&journal.app_dir))
        .spawn()
        .map_err(|error| error.to_string())
}

fn readiness_timeout() -> Duration {
    std::env::var("PANDOCK_UPDATER_READINESS_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(READINESS_TIMEOUT)
}

fn wait_for_readiness(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.is_file() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

struct Args {
    protocol: u32,
    journal: String,
    target_app: String,
    launch: String,
    expected_version: String,
    parent_pid: u32,
    readiness_token: String,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut protocol = None;
        let mut journal = None;
        let mut target_app = None;
        let mut launch = None;
        let mut expected_version = None;
        let mut parent_pid = None;
        let mut readiness_token = None;
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let value = args.next().ok_or_else(|| format!("{flag} 缺少参数"))?;
            match flag.as_str() {
                "--protocol" => protocol = Some(value.parse().map_err(|_| "protocol 无效")?),
                "--journal" => journal = Some(value),
                "--target-app" => target_app = Some(value),
                "--launch" => launch = Some(value),
                "--expected-version" => expected_version = Some(value),
                "--parent-pid" => parent_pid = Some(value.parse().map_err(|_| "parent pid 无效")?),
                "--readiness-token" => readiness_token = Some(value),
                _ => return Err(format!("未知参数 {flag}")),
            }
        }
        Ok(Self {
            protocol: protocol.ok_or("缺少 --protocol")?,
            journal: journal.ok_or("缺少 --journal")?,
            target_app: target_app.ok_or("缺少 --target-app")?,
            launch: launch.ok_or("缺少 --launch")?,
            expected_version: expected_version.ok_or("缺少 --expected-version")?,
            parent_pid: parent_pid.ok_or("缺少 --parent-pid")?,
            readiness_token: readiness_token.ok_or("缺少 --readiness-token")?,
        })
    }
}
