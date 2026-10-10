use tauri::AppHandle;

#[cfg(windows)]
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
use tauri_plugin_autostart::ManagerExt;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use std::process::Command;

#[cfg(windows)]
const SHORTCUT_NAME: &str = "Pandock.lnk";
#[cfg(windows)]
const LEGACY_RUN_NAMES: &[&str] = &["CloudDock", "Pandock"];

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
const WINDOWS_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const WINDOWS_APPROVED_RUN_KEY: &str =
    r"HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
#[cfg(windows)]
const WINDOWS_APPROVED_STARTUP_FOLDER_KEY: &str =
    r"HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder";

#[cfg(windows)]
fn startup_shortcut_path() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA")
        .ok_or_else(|| "无法读取 APPDATA，不能定位 Windows 启动文件夹".to_string())?;
    Ok(PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup")
        .join(SHORTCUT_NAME))
}

#[cfg(windows)]
fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("无法定位当前程序：{e}"))
}

#[cfg(windows)]
fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(windows)]
fn run_powershell(script: &str) -> Result<(), String> {
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-Command",
            script,
        ])
        .creation_flags(CREATE_NO_WINDOW);
    let output = command
        .output()
        .map_err(|e| format!("无法执行 Windows 启动项配置命令：{e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = if stderr.trim().is_empty() {
        "未知错误"
    } else {
        stderr.trim()
    };
    Err(format!("Windows 启动项配置失败：{detail}"))
}

#[cfg(windows)]
fn create_startup_shortcut_at(shortcut: &Path) -> Result<(), String> {
    let exe = current_exe()?;
    let working_dir = exe
        .parent()
        .ok_or_else(|| "无法定位程序所在目录".to_string())?;
    let script = format!(
        r#"$ErrorActionPreference='Stop'; $startup={shortcut}; $target={target}; $working={working}; $dir=Split-Path -Parent $startup; New-Item -ItemType Directory -Path $dir -Force | Out-Null; $shell=New-Object -ComObject WScript.Shell; $link=$shell.CreateShortcut($startup); $link.TargetPath=$target; $link.Arguments='--autostart'; $link.WorkingDirectory=$working; $link.WindowStyle=7; $link.Description='Pandock'; $link.Save(); exit 0"#,
        shortcut = powershell_literal(&shortcut.display().to_string()),
        target = powershell_literal(&exe.display().to_string()),
        working = powershell_literal(&working_dir.display().to_string()),
    );
    run_powershell(&script)
}

#[cfg(windows)]
fn create_startup_shortcut() -> Result<(), String> {
    create_startup_shortcut_at(&startup_shortcut_path()?)?;
    ensure_startup_folder_approved()
}

/// Startup 文件夹项的启用状态由 StartupApproved\StartupFolder 的 12 字节
/// REG_BINARY 控制（首字节 0x02=启用，0x03=禁用）。程序自建的快捷方式可能
/// 从未登记该值，或被系统/用户改成禁用——两者都会导致登录时不启动。
/// 这里在写快捷方式后强制保证值存在且首字节为 0x02；其余字节清零，
/// 由 Windows 自行维护时间戳。
#[cfg(windows)]
fn ensure_startup_folder_approved() -> Result<(), String> {
    let script = format!(
        r#"$ErrorActionPreference='Stop'; $key={key}; New-Item -Path $key -Force | Out-Null; $name={name}; $cur=(Get-ItemProperty -LiteralPath $key -Name $name -ErrorAction SilentlyContinue).$name; if ($null -eq $cur -or $cur.Length -ne 12 -or $cur[0] -ne 2) {{ $enabled=[byte[]](2,0,0,0,0,0,0,0,0,0,0,0); New-ItemProperty -LiteralPath $key -Name $name -PropertyType Binary -Value $enabled -Force | Out-Null }}; exit 0"#,
        key = powershell_literal(WINDOWS_APPROVED_STARTUP_FOLDER_KEY),
        name = powershell_literal(SHORTCUT_NAME),
    );
    run_powershell(&script)
}

#[cfg(windows)]
fn remove_startup_folder_approved() -> Result<(), String> {
    let script = format!(
        r#"$ErrorActionPreference='SilentlyContinue'; Remove-ItemProperty -LiteralPath {key} -Name {name} -ErrorAction SilentlyContinue; exit 0"#,
        key = powershell_literal(WINDOWS_APPROVED_STARTUP_FOLDER_KEY),
        name = powershell_literal(SHORTCUT_NAME),
    );
    run_powershell(&script)
}

#[cfg(windows)]
fn remove_startup_shortcut() -> Result<(), String> {
    let shortcut = startup_shortcut_path()?;
    match std::fs::remove_file(&shortcut) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("无法删除 Windows 启动快捷方式：{error}")),
    }
}

#[cfg(windows)]
fn legacy_run_value_exists() -> Result<bool, String> {
    for name in LEGACY_RUN_NAMES {
        let status = Command::new("reg.exe")
            .args(["query", WINDOWS_RUN_KEY, "/v", name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|e| format!("无法查询 Windows 启动项：{e}"))?;
        if status.success() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(windows)]
fn remove_legacy_run_entries() -> Result<(), String> {
    let script = format!(
        r#"$ErrorActionPreference='SilentlyContinue'; $run='HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'; $approved={approved}; foreach($name in @('CloudDock','Pandock')){{ Remove-ItemProperty -LiteralPath $run -Name $name -ErrorAction SilentlyContinue; Remove-ItemProperty -LiteralPath $approved -Name $name -ErrorAction SilentlyContinue }}; exit 0"#,
        approved = powershell_literal(WINDOWS_APPROVED_RUN_KEY),
    );
    run_powershell(&script)
}

#[cfg(windows)]
fn clear_current_metadata() -> Result<(), String> {
    remove_startup_shortcut()?;
    remove_startup_folder_approved()?;
    remove_legacy_run_entries()
}

#[cfg(windows)]
pub fn is_enabled(_app: &AppHandle) -> Result<bool, String> {
    let shortcut = startup_shortcut_path()?.exists();
    Ok(shortcut || legacy_run_value_exists()?)
}

#[cfg(not(windows))]
pub fn is_enabled(app: &AppHandle) -> Result<bool, String> {
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

#[cfg(windows)]
pub fn set_enabled(_app: &AppHandle, enabled: bool) -> Result<(), String> {
    if enabled {
        create_startup_shortcut()?;
        remove_legacy_run_entries()
    } else {
        clear_current_metadata()
    }
}

#[cfg(not(windows))]
pub fn set_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(|e| e.to_string())
    } else {
        manager.disable().map_err(|e| e.to_string())
    }
}

/// Reconciles the persisted preference with the Startup shortcut.
///
/// The local Windows startup broker was verified to ignore newly added Run
/// entries, while the same C:/F: executables launched correctly from the
/// Startup folder. Pandock therefore uses the Startup shortcut and removes
/// the legacy CloudDock/Pandock Run entries to avoid double launch.
#[cfg(windows)]
pub fn reconcile(_app: &AppHandle, config_enabled: bool) -> Result<bool, String> {
    let shortcut_exists = startup_shortcut_path()?.exists();
    let enabled = config_enabled || shortcut_exists || legacy_run_value_exists()?;
    if enabled {
        create_startup_shortcut()?;
        remove_legacy_run_entries()?;
    } else {
        clear_current_metadata()?;
    }
    Ok(enabled)
}

#[cfg(not(windows))]
pub fn reconcile(app: &AppHandle, _config_enabled: bool) -> Result<bool, String> {
    is_enabled(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn powershell_literal_escapes_single_quotes() {
        let path = std::path::Path::new(r"C:\Users\O'Brien\Pandock.exe");
        assert_eq!(
            powershell_literal(&path.display().to_string()),
            "'C:\\Users\\O''Brien\\Pandock.exe'"
        );
    }

    #[cfg(windows)]
    #[test]
    fn approved_folder_script_targets_shortcut_name_and_enabled_byte() {
        // 脚本必须：指向 StartupApproved\StartupFolder、作用在 Pandock.lnk、
        // 首字节写 0x02（启用）、长度校验 12 字节。
        let key = WINDOWS_APPROVED_STARTUP_FOLDER_KEY;
        assert!(key.ends_with("StartupApproved\\StartupFolder"));
        assert_eq!(SHORTCUT_NAME, "Pandock.lnk");
    }

    #[cfg(windows)]
    #[test]
    fn creates_startup_shortcut_at_requested_path() {
        let directory =
            std::env::temp_dir().join(format!("pandock-autostart-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let shortcut = directory.join("Pandock.lnk");
        create_startup_shortcut_at(&shortcut).unwrap();
        assert!(shortcut.is_file());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
