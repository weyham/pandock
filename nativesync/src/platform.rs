use crate::backend::CloudBackend;
use crate::hydration::HydrationError;
use pandock_core::cloud::RemotePath;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// CF 视角下"是否占位符"的**三态**判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaceholderProbe {
    Placeholder,
    /// **确定**不是占位符（可以安全地就地转换）。
    NotPlaceholder,
    /// **无法确定**（包含一切非"确定不是"的错误）：必须按"当作占位符、不转换"处理。
    Unknown,
}

/// 把 CfGetPlaceholderInfo 的结果映射成三态（纯函数，便于跨平台单测）。
///
/// **只有"确定不是占位符"才返回 NotPlaceholder**；其余一切错误都是 Unknown，
/// 而 Unknown 的处置等价于"当成占位符、**不要转换**"（与"打不开句柄"同一保守策略）。
///
/// 事故（2026-10-12）：此前把**任何**错误都当成"不是占位符"，于是对一个**本来就是占位符的目录**
/// 调用了 CfConvertToPlaceholder → 返回 ERROR_CLOUD_FILE_INVALID_REQUEST (0x8007017C)，
/// 并把该错误冒泡成整条命令失败（UI 出现红框）。
pub(crate) fn classify_placeholder_probe(result: Result<(), u32>) -> PlaceholderProbe {
    match result {
        Ok(()) => PlaceholderProbe::Placeholder,
        Err(hr) => match hr {
            // ERROR_NOT_A_REPARSE_POINT (4390) / ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT (390)
            0x8007_1126 | 0x8007_0186 => PlaceholderProbe::NotPlaceholder,
            // 其余（INVALID_REQUEST / ACCESS_DENIED / NOT_A_CLOUD_FILE …）一律"不确定"。
            _ => PlaceholderProbe::Unknown,
        },
    }
}

/// 已存在路径的占位符修复动作（纯决策，便于跨平台单测）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaceholderRepair {
    /// 本来就是占位符：只维持 in-sync 标记。
    KeepInSync,
    /// 不是占位符（例如被注销转成了普通文件）：就地转回占位符。
    ConvertToPlaceholder,
}

/// **仅供诊断日志用**的属性线索：**不是**权威判定。
///
/// ⚠️ 真机实测（2026-10-12）：健康占位符 `db.sql`/`manifest.json`/`skills.zip` 的
/// `GetFileAttributesW`、`GetFileAttributesExW`、Python `st_file_attributes`
/// **都不含 `0x400 (REPARSE_POINT)`**（实测 0x00100020 / 0x00080020 / 0x00000020），
/// 而 `fsutil reparsepoint query` 明确显示它们是重解析点（`Reparse Tag 0x9000601a`）。
/// 即**该读取路径不返回** `REPARSE_POINT`（同期 PowerShell 属性读取却返回；机制未确定，可能取决于底层 API）。用它判定会让健康占位符被误判、
/// 每次 `sync_now` 重复转换。权威判定见 `cf_is_placeholder`（CF 自身给出的结论）。
///
/// 占位符必然是重解析点（`FILE_ATTRIBUTE_REPARSE_POINT`）；在线/按需占位符还会带
/// `RECALL_ON_OPEN` / `RECALL_ON_DATA_ACCESS`。三者任一命中即视为占位符。
pub(crate) fn is_placeholder_attrs(attrs: u32) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    attrs
        & (FILE_ATTRIBUTE_REPARSE_POINT
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
}

/// 需要"移除同步根"的两种起因。注销是**破坏性**的（会遍历整棵树，见 ADR-110），
/// 因此必须把起因显式建模，并由 [`sync_root_removal_action`] 统一裁决。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncRootRemovalCause {
    /// 用户在资源管理器/卸载流程里**刻意移除**同步根：允许注销。
    Deliberate,
    /// `CfConnectSyncRoot` 失败：**绝不注销**。
    ConnectFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncRootRemovalAction {
    KeepRegistration,
    Unregister,
}

/// 裁决"某起因下是否注销同步根"。
///
/// **只有刻意移除才允许注销**：注销会遍历整棵树——已水合的占位符转回普通文件、
/// **未水合的占位符被从本地永久删除**（ADR-110）。连接失败时保留注册虽然会让该目录在
/// 未连接期间报「云文件提供程序未运行」，但**树与占位符都活着**，可由下一次成功连接后的
/// 自愈（`CfConvertToPlaceholder`）恢复；注销是不可逆的破坏。前者严格优于后者。
pub(crate) fn sync_root_removal_action(cause: SyncRootRemovalCause) -> SyncRootRemovalAction {
    match cause {
        SyncRootRemovalCause::Deliberate => SyncRootRemovalAction::Unregister,
        SyncRootRemovalCause::ConnectFailure => SyncRootRemovalAction::KeepRegistration,
    }
}

pub(crate) fn placeholder_repair_action(is_placeholder: bool) -> PlaceholderRepair {
    if is_placeholder {
        PlaceholderRepair::KeepInSync
    } else {
        PlaceholderRepair::ConvertToPlaceholder
    }
}

/// in-sync 自愈的判定输入（纯数据，便于跨平台单测）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InSyncRepairInput {
    /// 本地路径存在且**是**占位符。
    pub is_placeholder: bool,
    /// 该条目有已记录的本地改动（`pending_op` 非空）。
    pub has_pending_change: bool,
    /// 本地 mtime/size 与快照一致（内容与云端一致）。
    pub snapshot_matches: bool,
    /// 平台侧 in-sync 状态；`None` = 查询不可用。
    pub in_sync: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InSyncRepair {
    /// 条件齐备且当前不是 in-sync → 调用 `CfSetInSyncState(IN_SYNC)`。
    Repair,
    /// 本来就是 in-sync → 什么都不做。
    LeaveAlone,
    /// 前置条件不满足（可能被本地改过 / 读不到状态）→ **绝不动手**。
    Skip,
}

/// 判定"是否可以把占位符恢复为 in-sync"。
///
/// **三个前置条件缺一不可**：是占位符、没有已记录的本地改动、本地内容与快照一致。
/// 在任何"内容可能被本地改过"的情况下强行置为已同步，会把真实改动**伪装成已同步**
/// （S1 没有上传方向，用户会以为改动已同步到云端）——比现状更危险。
pub(crate) fn in_sync_repair_action(input: InSyncRepairInput) -> InSyncRepair {
    if !input.is_placeholder || input.has_pending_change || !input.snapshot_matches {
        return InSyncRepair::Skip;
    }
    match input.in_sync {
        Some(true) => InSyncRepair::LeaveAlone,
        Some(false) => InSyncRepair::Repair,
        None => InSyncRepair::Skip,
    }
}

/// 脱水成功之后对 pin 状态的处理（纯决策，便于跨平台单测）。
///
/// 依据 MS Learn CF_PIN_STATE：**显式 UNPINNED(2) 的语义就是"请提供程序异步脱水"**。
/// 脱水一旦完成，这个标记就成了"永远待办"——平台会因此一直认为该占位符有活没干
/// （资源管理器显示同步挂起类图标、「释放空间」该文件灰显）。所以要把它归回 UNSPECIFIED(0)，
/// 让平台重新"自由决定"。**用户的显式 PINNED(1) 不动**（固定是明确的"保留本地"意图）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DehydratePostAction {
    NormalizeToUnspecified,
    LeaveAlone,
}

pub(crate) fn dehydrate_post_action(pin_state: u32) -> DehydratePostAction {
    match pin_state {
        2 /* CF_PIN_STATE_UNPINNED */ => DehydratePostAction::NormalizeToUnspecified,
        _ => DehydratePostAction::LeaveAlone,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("NativeSync 当前平台不可用: {0}")]
    Unsupported(String),
    #[error("Cloud Files API: {0}")]
    Api(String),
    #[error("路径错误: {0}")]
    Path(String),
}

pub trait PlatformSync: Send + Sync {
    fn supported(&self) -> Result<(), String>;
    fn register(&mut self, root: &Path) -> Result<(), PlatformError>;
    fn connect(&mut self) -> Result<(), PlatformError>;
    fn disconnect(&mut self) -> Result<(), PlatformError>;
    fn unregister(&mut self) -> Result<(), PlatformError>;
    fn set_hydration_backend(&mut self, _backend: Arc<dyn CloudBackend>) {}
    fn set_remote_mapping(&mut self, _relative: &str, _remote: RemotePath, _fs_id: u64) {}
    fn set_state_db(&mut self, _path: &Path) {}
    fn set_event_sink(
        &mut self,
        _sink: Option<Arc<dyn Fn(crate::model::NativeSyncEvent) + Send + Sync>>,
    ) {
    }
    /// 本地变更事件出口（S1 的 `pandock://nativesync/change` 由它驱动）。
    fn set_change_sink(
        &mut self,
        _sink: Option<Arc<dyn Fn(crate::model::NativeSyncChangeEvent) + Send + Sync>>,
    ) {
    }
    /// 紧急开关：关闭后不再注册/处理本地变更通知（保留只读召回与占位符创建）。
    fn set_notifications_enabled(&mut self, _enabled: bool) {}
    /// 取出并清空"已存在路径判定"的一次性统计 `(checked, placeholders, converted)`。
    fn set_run_id(&mut self, _run_id: u64) {}
    /// 只读查询"本地实际占用字节数"（CfGetPlaceholderInfo(STANDARD).OnDiskDataSize）。
    /// 用于在**读取文件内容之前**判断它是否已经脱水——对已脱水文件做读访问会立刻把它水合回来。
    fn on_disk_data_size(&self, _path: &Path) -> Option<i64> {
        None
    }

    fn take_probe_summary(&mut self) -> (usize, usize, usize, usize) {
        (0, 0, 0, 0)
    }
    /// 若占位符被平台标成"未同步"且**内容确实与云端一致**，把它恢复为 in-sync。
    /// 前置条件（`has_pending_change` / `snapshot_matches`）由调用方从索引库给出。
    fn repair_in_sync_if_needed(
        &mut self,
        _path: &Path,
        _fs_id: u64,
        _is_dir: bool,
        _has_pending_change: bool,
        _snapshot_matches: bool,
    ) {
    }
    fn create_placeholder(
        &mut self,
        path: &Path,
        is_dir: bool,
        size: u64,
        identity: &[u8],
    ) -> Result<(), PlatformError>;
    fn pin(&mut self, path: &Path, pinned: bool) -> Result<(), PlatformError>;
    fn dehydrate(&mut self, path: &Path) -> Result<(), PlatformError>;
    fn mark_in_sync(&mut self, path: &Path) -> Result<(), PlatformError>;
    /// 从文件系统回读 pin 状态（None = 未知/非占位符）。
    /// CFAPI 会把「固定到本地」镜像到 FILE_ATTRIBUTE_PINNED 属性位。
    fn pin_state(&self, path: &Path) -> Option<bool>;
    fn remove_placeholder(&mut self, path: &Path, is_dir: bool) -> Result<(), PlatformError>;
}

pub fn new_platform() -> Box<dyn PlatformSync> {
    platform_impl()
}

#[cfg(not(windows))]
fn platform_impl() -> Box<dyn PlatformSync> {
    Box::new(StubPlatform)
}
#[cfg(windows)]
fn platform_impl() -> Box<dyn PlatformSync> {
    Box::new(WindowsPlatform::default())
}

#[cfg(not(windows))]
struct StubPlatform;
#[cfg(not(windows))]
impl PlatformSync for StubPlatform {
    fn supported(&self) -> Result<(), String> {
        Err("NativeSync 仅支持 Windows 10 1903+".into())
    }
    fn register(&mut self, _: &Path) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("Windows Cloud Files API".into()))
    }
    fn connect(&mut self) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("Windows Cloud Files API".into()))
    }
    fn disconnect(&mut self) -> Result<(), PlatformError> {
        Ok(())
    }
    fn unregister(&mut self) -> Result<(), PlatformError> {
        Ok(())
    }
    fn create_placeholder(
        &mut self,
        _: &Path,
        _: bool,
        _: u64,
        _: &[u8],
    ) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("Windows Cloud Files API".into()))
    }
    fn pin(&mut self, _: &Path, _: bool) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("Windows Cloud Files API".into()))
    }
    fn dehydrate(&mut self, _: &Path) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("Windows Cloud Files API".into()))
    }
    fn pin_state(&self, _: &Path) -> Option<bool> {
        None
    }
    fn mark_in_sync(&mut self, _: &Path) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("Windows Cloud Files API".into()))
    }
    fn remove_placeholder(&mut self, _: &Path, _: bool) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("Windows Cloud Files API".into()))
    }
}

#[cfg(windows)]
mod windows_impl {

    /// 同步根注册标识：`<providerId>!<用户SID>!<accountId>`。
    /// 与外壳注册（StorageProviderSyncRootManager）使用同一个 Id，
    /// 且包含 SID 以保证每用户一份、互不干扰。
    const SHELL_PROVIDER_ID: &str = "Pandock";
    const SHELL_ACCOUNT_ID: &str = "Personal";

    /// 当前登录用户的 SID。
    fn current_user_sid() -> Result<String, PlatformError> {
        use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
        use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
                .map_err(|e| PlatformError::Api(e.to_string()))?;
            let mut needed = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
            let mut buf = vec![0u8; needed as usize];
            let result = GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                needed,
                &mut needed,
            );
            let _ = CloseHandle(token);
            result.map_err(|e| PlatformError::Api(e.to_string()))?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid = windows::core::PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut sid)
                .map_err(|e| PlatformError::Api(e.to_string()))?;
            sid.to_string()
                .map_err(|e| PlatformError::Api(e.to_string()))
        }
    }

    /// 同步根的注册标识。
    fn shell_sync_root_id() -> Result<String, PlatformError> {
        let sid = current_user_sid()?;
        Ok(format!("{SHELL_PROVIDER_ID}!{sid}!{SHELL_ACCOUNT_ID}"))
    }

    /// 记住当前注册的同步根路径，供 unregister 使用。
    fn shell_root_set(root: &Path) {
        if let Ok(mut slot) = SHELL_ROOT.get_or_init(|| RwLock::new(None)).write() {
            *slot = Some(root.to_path_buf());
        }
    }

    fn shell_root_take() {
        if let Some(lock) = SHELL_ROOT.get() {
            if let Ok(mut slot) = lock.write() {
                *slot = None;
            }
        }
    }

    static SHELL_ROOT: OnceLock<RwLock<Option<std::path::PathBuf>>> = OnceLock::new();

    /// 外壳层注册：把同步根交给 Windows 外壳，解锁导航栏节点与同步状态角标。
    ///
    /// 依据微软 CloudMirror 官方示例（Samples/CloudMirror）：现代做法是
    /// 「StorageProviderSyncRootManager.Register + CfConnectSyncRoot」，
    /// 不使用 CfRegisterSyncRoot；两者混用会返回 0x8007018B（已拒绝访问云文件）。
    fn register_with_shell(root: &Path) -> Result<(), PlatformError> {
        use windows::core::Interface;
        use windows::Storage::Provider::{
            StorageProviderHydrationPolicy, StorageProviderHydrationPolicyModifier,
            StorageProviderInSyncPolicy, StorageProviderPopulationPolicy,
            StorageProviderSyncRootInfo, StorageProviderSyncRootManager,
        };
        use windows::Storage::StorageFolder;

        let id = shell_sync_root_id()?;
        let info =
            StorageProviderSyncRootInfo::new().map_err(|e| PlatformError::Api(e.to_string()))?;

        let folder = StorageFolder::GetFolderFromPathAsync(&windows::core::HSTRING::from(
            root.to_string_lossy().as_ref(),
        ))
        .and_then(|op| op.get())
        .map_err(|e| PlatformError::Api(e.to_string()))?;

        let folder: windows::Storage::IStorageFolder = folder
            .cast()
            .map_err(|e| PlatformError::Api(e.to_string()))?;

        let set = |r: windows::core::Result<()>| r.map_err(|e| PlatformError::Api(e.to_string()));
        set(info.SetId(&windows::core::HSTRING::from(id.as_str())))?;
        set(info.SetPath(&folder))?;
        set(info.SetDisplayNameResource(&windows::core::HSTRING::from("Pandock")))?;
        // 图标取自本程序内嵌的应用图标（Windows 按 SHLoadIndirectString 解析）。
        if let Ok(exe) = std::env::current_exe() {
            set(info.SetIconResource(&windows::core::HSTRING::from(format!(
                "{},0",
                exe.display()
            ))))?;
        }
        set(info.SetVersion(&windows::core::HSTRING::from(env!("CARGO_PKG_VERSION"))))?;
        // 按需水合：占位符可在线、可固定到本地。
        set(info.SetHydrationPolicy(StorageProviderHydrationPolicy::Full))?;
        set(info.SetHydrationPolicyModifier(StorageProviderHydrationPolicyModifier::None))?;
        set(info.SetPopulationPolicy(StorageProviderPopulationPolicy::Full))?;
        set(info.SetInSyncPolicy(StorageProviderInSyncPolicy::Default))?;
        set(
            info.SetHardlinkPolicy(windows::Storage::Provider::StorageProviderHardlinkPolicy::None)
        )?;
        set(info.SetAllowPinning(true))?;
        set(info.SetShowSiblingsAsGroup(false))?;

        match StorageProviderSyncRootManager::Register(&info) {
            Ok(()) => {}
            // 已注册（重复启用/重启）：视为成功。
            Err(e) if e.code().0 as u32 == 0x800700B7 => {
                log::info!("native-sync shell sync root already registered");
            }
            Err(e) => return Err(PlatformError::Api(e.to_string())),
        }
        shell_root_set(root);
        log::info!("native-sync shell sync root registered: {}", root.display());
        Ok(())
    }

    /// 注销外壳注册（仅卸载/彻底停用同步时调用）。
    fn unregister_from_shell() -> Result<(), PlatformError> {
        use windows::Storage::Provider::StorageProviderSyncRootManager;
        let id = shell_sync_root_id()?;
        match StorageProviderSyncRootManager::Unregister(&windows::core::HSTRING::from(id.as_str()))
        {
            Ok(()) => {}
            Err(e) => {
                let code = e.code().0 as u32;
                // 未注册时视为成功。
                if code != 0x80070490 && code != 0x80070002 {
                    return Err(PlatformError::Api(e.to_string()));
                }
            }
        }
        shell_root_take();
        log::info!("native-sync shell sync root unregistered");
        Ok(())
    }
    use super::*;
    use crate::notify::relative_from_normalized;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex, OnceLock, RwLock};
    use std::thread;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, NTSTATUS};
    use windows::Win32::Storage::CloudFilters::*;
    use windows::Win32::Storage::FileSystem::*;

    #[derive(Default)]
    pub struct WindowsPlatform {
        root: Option<PathBuf>,
        connection: Option<CF_CONNECTION_KEY>,
        runtime: Arc<Runtime>,
        /// 每次 `sync_now` 期间"已存在路径判定"的统计：(检查数, 判为占位符数, 实际转换数)。
        probe_stats: Mutex<(usize, usize, usize, usize)>,
        run_id: std::sync::atomic::AtomicU64,
    }
    type EventSink = Arc<dyn Fn(crate::model::NativeSyncEvent) + Send + Sync>;
    type ChangeSink = Arc<dyn Fn(crate::model::NativeSyncChangeEvent) + Send + Sync>;

    #[derive(Default)]
    struct Runtime {
        backend: RwLock<Option<Arc<dyn CloudBackend>>>,
        root: RwLock<Option<PathBuf>>,
        mappings: RwLock<HashMap<String, (RemotePath, u64)>>,
        by_id: RwLock<HashMap<u64, (RemotePath, String)>>,
        state_db: RwLock<Option<PathBuf>>,
        event_sink: RwLock<Option<EventSink>>,
        change_sink: RwLock<Option<ChangeSink>>,
        /// 本地变更通知：有界队列 + 单消费者线程。用独立的额度，不占 hydration 的
        /// `try_begin_worker` 名额，避免通知把召回挤掉、或反之。
        notify_tx: RwLock<Option<std::sync::mpsc::SyncSender<NotifyTask>>>,
        notify_worker_started: AtomicBool,
        /// 紧急开关（配置字段 + 环境变量覆盖），默认关闭捕获前不生效。
        notifications_enabled: AtomicBool,
        /// 应用自身发起的 CF 操作守卫：由此产生的 NOTIFY_* 是「自杀回调」，
        /// 必须去重，否则会把应用的动作误记成用户的本地变更。
        app_path_ops: Mutex<HashMap<String, std::time::Instant>>,
        app_fs_ops: Mutex<HashMap<u64, std::time::Instant>>,
        cancelled: RwLock<HashMap<i64, Arc<AtomicBool>>>,
        lifecycle: Mutex<RuntimeLifecycle>,
        lifecycle_cv: Condvar,
    }
    #[derive(Default)]
    struct RuntimeLifecycle {
        accepting: bool,
        active_workers: usize,
    }
    struct WorkerLease {
        runtime: Arc<Runtime>,
    }
    impl Runtime {
        fn start_accepting(&self) {
            if let Ok(mut state) = self.lifecycle.lock() {
                state.accepting = true;
            }
        }
        fn try_begin_worker(self: &Arc<Self>) -> Option<WorkerLease> {
            let mut state = self.lifecycle.lock().ok()?;
            if !state.accepting {
                return None;
            }
            state.active_workers += 1;
            Some(WorkerLease {
                runtime: Arc::clone(self),
            })
        }
        fn stop_accepting_and_wait(&self) {
            let mut state = match self.lifecycle.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            state.accepting = false;
            if let Ok(jobs) = self.cancelled.read() {
                for cancelled in jobs.values() {
                    cancelled.store(true, Ordering::SeqCst);
                }
            }
            while state.active_workers != 0 {
                state = match self.lifecycle_cv.wait(state) {
                    Ok(state) => state,
                    Err(_) => return,
                };
            }
        }
    }
    impl Drop for WorkerLease {
        fn drop(&mut self) {
            if let Ok(mut state) = self.runtime.lifecycle.lock() {
                state.active_workers = state.active_workers.saturating_sub(1);
                if state.active_workers == 0 {
                    self.runtime.lifecycle_cv.notify_all();
                }
            }
        }
    }
    // Cloud Files retains CallbackContext across callbacks. Keep one strong
    // reference for each connection until process teardown; never free the
    // context while the OS could still dispatch a callback.
    static PROCESS_CONTEXTS: OnceLock<Mutex<Vec<Arc<Runtime>>>> = OnceLock::new();
    fn retain_process_context(runtime: &Arc<Runtime>) -> Result<(), PlatformError> {
        PROCESS_CONTEXTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .map_err(|_| PlatformError::Api("provider context lock poisoned".into()))?
            .push(Arc::clone(runtime));
        Ok(())
    }
    unsafe fn runtime_from_callback(info: &CF_CALLBACK_INFO) -> Option<Arc<Runtime>> {
        let ptr = info.CallbackContext as *const Runtime;
        if ptr.is_null() {
            return None;
        }
        Arc::increment_strong_count(ptr);
        Some(Arc::from_raw(ptr))
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    fn file_handle(path: &Path, write: bool) -> Result<HANDLE, PlatformError> {
        let full = path.to_string_lossy().replace('/', "\\");
        let w = wide(&full);
        let mut flags = FILE_ATTRIBUTE_NORMAL;
        if path.is_dir() {
            flags |= FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_BACKUP_SEMANTICS.0);
        }
        // **只请求属性访问**（不要 FILE_GENERIC_READ/WRITE）：
        // 对**已脱水**的占位符做读访问会让平台立刻把内容物化回来（重新水合），
        // 于是"释放空间"刚成功就被我们自己的元数据调用撤销（真实事故 D1）。
        // 需要改属性（in-sync / pin / 脱水 / 转换）用 FILE_WRITE_ATTRIBUTES，只读属性用 FILE_READ_ATTRIBUTES。
        // 数据通路不依赖此句柄：fetch worker 走 CfExecute(TRANSFER_DATA)。
        let access = if write {
            FILE_WRITE_ATTRIBUTES
        } else {
            FILE_READ_ATTRIBUTES
        };
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                access.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                flags,
                HANDLE::default(),
            )
        }
        .map_err(|e| PlatformError::Api(e.to_string()))?;
        if h.is_invalid() {
            return Err(PlatformError::Api("打开云文件句柄失败".into()));
        }
        Ok(h)
    }
    fn operation_param_size<T>() -> u32 {
        let params = CF_OPERATION_PARAMETERS::default();
        let base = &params as *const _ as usize;
        let member = &params.Anonymous as *const _ as usize;
        (member - base + std::mem::size_of::<T>()) as u32
    }

    /// 召回映射解析：优先内存 by_id；未命中时回退 SQLite（remote_path + relative_path），
    /// 保证重启后（映射为内存态）无需 sync_now 也能解析。均在 worker 线程内执行。
    fn resolve_mapping(
        runtime: &Runtime,
        db: Option<&crate::db::IndexDb>,
        file_id: u64,
    ) -> Option<(RemotePath, String)> {
        if let Some(hit) = runtime
            .by_id
            .read()
            .ok()
            .and_then(|m| m.get(&file_id).cloned())
        {
            return Some(hit);
        }
        db.and_then(|db| db.mapping_by_fs_id(file_id as i64).ok().flatten())
            .and_then(|(remote_path, relative)| {
                RemotePath::parse(&remote_path)
                    .ok()
                    .map(|remote| (remote, relative))
            })
    }
    fn file_id_from_identity(info: &CF_CALLBACK_INFO) -> Option<u64> {
        let len = info.FileIdentityLength as usize;
        if info.FileIdentity.is_null() || len != std::mem::size_of::<u64>() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(info.FileIdentity.cast::<u8>(), len) };
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    /// 只读探测占位符的 in-sync 状态（**纯观测，不改任何行为**）。
    ///
    /// CFAPI **没有** `CfGetInSyncState`（只有 `CfSetInSyncState`），读状态必须用
    /// `CfGetPlaceholderInfo(CF_PLACEHOLDER_INFO_BASIC)` 取 `CF_PLACEHOLDER_BASIC_INFO.InSyncState`。
    ///
    /// 为什么把它做成常驻告警而不是临时探针：这个状态**没有任何外部只读路径**
    /// （外部进程调用被 0x8007018B 拒、`fsutil` 不暴露、索引库不记录），
    /// 一旦它错了——Explorer 会因此禁用「释放空间」并显示"同步挂起"箭头——我们就是盲的。
    /// 所以：**只在不是 `IN_SYNC` 时 `warn`**，正常与查询失败都走 `debug`，避免刷屏。
    /// 只记 `kind`（file/dir），不记文件名（隐私纪律）。
    /// **CF 原生**判定：`CfGetPlaceholderInfo` 成功即说明该路径确实是本提供程序的占位符。
    ///
    /// 不用通用属性位（`GetFileAttributes*`）——Windows 对云占位符会隐藏 `0x400`，
    /// 用它判定会让健康占位符被反复转换（真实缺陷，见 `is_placeholder_attrs` 的说明）。
    fn placeholder_probe(path: &Path) -> PlaceholderProbe {
        let Ok(handle) = file_handle(path, false) else {
            return PlaceholderProbe::Unknown; // 打不开句柄 ⇒ 不确定 ⇒ 宁可不转换
        };
        let mut buf = vec![0u8; std::mem::size_of::<CF_PLACEHOLDER_BASIC_INFO>() + 64];
        let mut returned = 0u32;
        let r = unsafe {
            CfGetPlaceholderInfo(
                handle,
                CF_PLACEHOLDER_INFO_BASIC,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                Some(&mut returned),
            )
        };
        unsafe {
            let _ = CloseHandle(handle);
        }
        let outcome = r.map_err(|e| e.code().0 as u32);
        let probe = classify_placeholder_probe(outcome);
        if probe == PlaceholderProbe::Unknown {
            if let Err(hr) = outcome {
                log::warn!(
                    "native-sync placeholder probe inconclusive op=CfGetPlaceholderInfo hr=0x{hr:08X}"
                );
            }
        }
        probe
    }

    fn probe_in_sync_state(path: &Path) -> Option<bool> {
        let handle = file_handle(path, false).ok()?;
        let mut buf = vec![0u8; std::mem::size_of::<CF_PLACEHOLDER_BASIC_INFO>() + 64];
        let mut returned = 0u32;
        let r = unsafe {
            CfGetPlaceholderInfo(
                handle,
                CF_PLACEHOLDER_INFO_BASIC,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                Some(&mut returned),
            )
        };
        unsafe {
            let _ = CloseHandle(handle);
        }
        match r {
            Ok(()) => {
                let info = unsafe { &*(buf.as_ptr() as *const CF_PLACEHOLDER_BASIC_INFO) };
                Some(info.InSyncState == CF_IN_SYNC_STATE_IN_SYNC)
            }
            Err(error) => {
                log::debug!(
                    "native-sync in-sync probe unavailable hr=0x{:08X}",
                    error.code().0 as u32
                );
                None
            }
        }
    }

    fn probe_in_sync(path: &Path, kind: &str) {
        let Ok(handle) = file_handle(path, false) else {
            return;
        };
        // `FileIdentity` 是柔性数组，缓冲区留出余量，避免 ERROR_MORE_DATA 而拿不到状态。
        let mut buf = vec![0u8; std::mem::size_of::<CF_PLACEHOLDER_BASIC_INFO>() + 64];
        let mut returned = 0u32;
        let r = unsafe {
            CfGetPlaceholderInfo(
                handle,
                CF_PLACEHOLDER_INFO_BASIC,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                Some(&mut returned),
            )
        };
        unsafe {
            let _ = CloseHandle(handle);
        }
        match r {
            Ok(()) => {
                let info = unsafe { &*(buf.as_ptr() as *const CF_PLACEHOLDER_BASIC_INFO) };
                if info.InSyncState == CF_IN_SYNC_STATE_IN_SYNC {
                    log::debug!("native-sync in-sync probe in_sync=true kind={kind}");
                } else {
                    log::warn!("native-sync in-sync probe not_in_sync=true kind={kind}");
                }
            }
            Err(error) => log::debug!(
                "native-sync in-sync probe unavailable kind={kind} hr=0x{:08X}",
                error.code().0 as u32
            ),
        }
    }

    /// 把单个路径标记为「与云端已同步」。
    ///
    /// 不标记时资源管理器的「状态」列会显示为**同步挂起**（蓝色循环箭头）——
    /// 这是 2026-10-12 实测定位的根因：`CfSetInSyncState` 此前只在「释放空间」
    /// 流程里被调用，占位符创建与水合完成两处都漏了。
    ///
    /// 关于旧注释「完整 hydration 后不得调用 CfSetInSyncState」：该结论来自更早的
    /// `set_len` 满尺寸文件 + MARK_IN_SYNC 组合（那种文件本是普通文件）。当前实现用
    /// `CfCreatePlaceholders` 创建**标准无数据占位符**，2026-10-12 在真机实验验证：
    /// 调用成功、REPARSE 保持不变（attrs 0x420 前后一致），状态列随即从「挂起」变
    /// 为正常图标。旧结论对当前实现不适用。
    fn mark_path_in_sync(path: &Path) -> Result<(), PlatformError> {
        let handle = file_handle(path, true)?;
        let mut usn = 0i64;
        let result = unsafe {
            CfSetInSyncState(
                handle,
                CF_IN_SYNC_STATE_IN_SYNC,
                CF_SET_IN_SYNC_FLAG_NONE,
                Some(&mut usn),
            )
        };
        unsafe {
            let _ = CloseHandle(handle);
        }
        result.map_err(|e| {
            log::warn!(
                "native-sync CfSetInSyncState failed op=mark_in_sync hr=0x{:08X}",
                e.code().0 as u32
            );
            PlatformError::Api(e.to_string())
        })
    }

    fn placeholder_fs_metadata(size: u64) -> CF_FS_METADATA {
        let mut metadata: CF_FS_METADATA = unsafe { std::mem::zeroed() };
        metadata.BasicInfo.FileAttributes = FILE_ATTRIBUTE_NORMAL.0;
        metadata.FileSize = size as i64;
        metadata
    }

    const PLACEHOLDER_CREATE_FLAGS: CF_PLACEHOLDER_CREATE_FLAGS = CF_PLACEHOLDER_CREATE_FLAG_NONE;

    fn create_dataless_file_placeholder(
        path: &Path,
        size: u64,
        identity: &[u8],
    ) -> Result<(), PlatformError> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .ok_or_else(|| PlatformError::Path("占位符缺少文件名".into()))?;
        let name_w = wide(&name);
        let parent = path
            .parent()
            .ok_or_else(|| PlatformError::Path("占位符缺少父目录".into()))?;
        let parent_text = parent.to_string_lossy().replace('/', "\\");
        let parent_full = if parent_text.starts_with("\\\\?\\") {
            parent_text
        } else {
            format!("\\\\?\\{parent_text}")
        };
        let parent_w = wide(&parent_full);
        let mut infos = [CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: PCWSTR(name_w.as_ptr()),
            FsMetadata: placeholder_fs_metadata(size),
            FileIdentity: identity.as_ptr() as *const c_void,
            FileIdentityLength: identity.len() as u32,
            Flags: PLACEHOLDER_CREATE_FLAGS,
            Result: windows::core::HRESULT(0),
            CreateUsn: 0,
        }];
        // 必须在 provider 连接进程内调用；外部进程会被 0x8007018B 拒绝。
        let r = unsafe {
            CfCreatePlaceholders(
                PCWSTR(parent_w.as_ptr()),
                &mut infos,
                CF_CREATE_FLAGS(0),
                None,
            )
        };
        r.map_err(|e| {
            log::warn!(
                "native-sync CfCreatePlaceholders call failed: parent={parent_full} error={e}"
            );
            PlatformError::Api(format!("CfCreatePlaceholders 调用失败: {e}"))
        })?;
        if infos[0].Result.is_ok() {
            // 标记为已同步，否则 Explorer 状态列显示「同步挂起」。
            if let Err(error) = mark_path_in_sync(path) {
                log::warn!(
                    "native-sync mark in-sync failed after placeholder create: {} error={error}",
                    path.display()
                );
            }
        }
        if infos[0].Result.is_err() {
            log::warn!(
                "native-sync file placeholder failed: {} size={size} hr=0x{:08X}",
                path.display(),
                infos[0].Result.0 as u32
            );
            return Err(PlatformError::Api(format!(
                "CfCreatePlaceholders 条目失败: 0x{:08X}",
                infos[0].Result.0 as u32
            )));
        }
        Ok(())
    }
    fn fetch_worker(
        runtime: Arc<Runtime>,
        info: (CF_CONNECTION_KEY, i64, i64),
        file_id: u64,
        offset: u64,
        length: i64,
        cancel: Arc<AtomicBool>,
    ) {
        const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
        // 诊断日志只记录 fs_id/区间/字节数，严禁记录文件名、dlink、token。
        log::info!("native-sync fetch start fs_id={file_id} offset={offset} length={length}");
        // 应用自身发起的召回：登记 fs_id 守卫，开/闭完成通知不得被误判为用户变更。
        note_app_fs_op(&runtime, file_id);
        let db = runtime
            .state_db
            .read()
            .ok()
            .and_then(|v| v.clone())
            .and_then(|path| {
                path.parent()
                    .and_then(|p| p.parent())
                    .map(crate::db::IndexDb::open)
            })
            .and_then(|result| result.ok());
        // P1: 内存映射（重启后为空）未命中时回退 SQLite 按 fs_id 解析。
        let Some((remote, relative)) = resolve_mapping(&runtime, db.as_ref(), file_id) else {
            log::warn!("native-sync fetch rejected: unmapped file fs_id={file_id}");
            unsafe {
                fail_fetch_from_keys(info.0, info.1, info.2, offset as i64, length);
            }
            if let Ok(mut jobs) = runtime.cancelled.write() {
                jobs.remove(&info.2);
            }
            return;
        };
        // 自愈：db 解析结果写回内存，后续回调直接命中。
        if let Ok(mut by_id) = runtime.by_id.write() {
            by_id.insert(file_id, (remote.clone(), relative.clone()));
        }
        // 水合完成后标记 in-sync 需要本地路径；relative 稍后会被 move，这里先留一份。
        let relative_for_mark = relative.clone();
        let job_id = db
            .as_ref()
            .and_then(|db| db.hydration_started(file_id, 0).ok());
        let result = (|| -> Result<crate::hydration::HydrationResult, String> {
            let backend = runtime
                .backend
                .read()
                .map_err(|_| "backend lock poisoned".to_string())?
                .clone()
                .ok_or_else(|| "backend missing".to_string())?;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            // P0-2: dlink 短时有效，FETCH_DATA 必须用 backend.stat 现取新鲜快照，
            // 不得使用 sync_now 时缓存的 dlink/md5/size。
            let snapshot = rt
                .block_on(async {
                    tokio::time::timeout(FETCH_TIMEOUT, backend.stat(&remote)).await
                })
                .map_err(|_| HydrationError::Backend("stat timeout".into()).to_string())
                .and_then(|r| {
                    r.map_err(|e| HydrationError::Backend(format!("{:?}", e.kind)).to_string())
                })?;
            let requested_total = if length < 0 {
                snapshot.size.saturating_sub(offset)
            } else {
                (length as u64).min(snapshot.size.saturating_sub(offset))
            };
            if let (Some(db), Some(job)) = (db.as_ref(), job_id) {
                let _ = db.hydration_set_total(job, requested_total);
            }
            let request = crate::hydration::HydrationRequest {
                file_id,
                remote,
                snapshot,
                offset,
                length,
                cancelled: cancel.clone(),
            };
            let connection = info.0;
            let transfer = info.1;
            let request_key = info.2;
            let mut bytes_done = 0u64;
            rt.block_on(async {
                tokio::time::timeout(
                    FETCH_TIMEOUT,
                    crate::hydration::hydrate_range(backend, request, |file_offset, bytes| {
                        if cancel.load(Ordering::SeqCst) {
                            return Err(HydrationError::Cancelled);
                        }
                        let op = CF_OPERATION_INFO {
                            StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
                            Type: CF_OPERATION_TYPE_TRANSFER_DATA,
                            ConnectionKey: connection,
                            TransferKey: transfer,
                            RequestKey: request_key,
                            ..Default::default()
                        };
                        let mut params = CF_OPERATION_PARAMETERS {
                            ParamSize: operation_param_size::<CF_OPERATION_PARAMETERS_0_6>(),
                            ..Default::default()
                        };
                        params.Anonymous.TransferData = CF_OPERATION_PARAMETERS_0_6 {
                            Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                            CompletionStatus: NTSTATUS(0),
                            Buffer: bytes.as_ptr() as *const c_void,
                            Offset: file_offset as i64,
                            Length: bytes.len() as i64,
                        };
                        let transferred = unsafe {
                            CfExecute(&op, &mut params)
                                .map_err(|e| HydrationError::Transfer(e.to_string()))
                        };
                        transferred?;
                        bytes_done = bytes_done.saturating_add(bytes.len() as u64);
                        if let (Some(db), Some(job_id)) = (db.as_ref(), job_id) {
                            let _ = db.hydration_progress(job_id, bytes_done);
                        }
                        Ok(())
                    }),
                )
                .await
                .map_err(|_| HydrationError::Backend("fetch timeout".into()))
                .and_then(|result| result)
            })
            .map_err(|e| e.to_string())
        })();
        // 完整 hydration 后同样要标记 in-sync，否则已下载文件在 Explorer 里
        // 一直显示「同步挂起」。旧注释所称「会把占位符提升为普通文件」已被
        // 2026-10-12 真机实验推翻（标准无数据占位符上调用的前后 attrs 与 REPARSE 不变）。
        match &result {
            Ok(hydrated) => log::info!(
                "native-sync fetch done fs_id={file_id} bytes={} full_file={}",
                hydrated.bytes,
                hydrated.full_file
            ),
            Err(error) => log::warn!("native-sync fetch failed fs_id={file_id} error={error}"),
        }
        // 完整水合成功后标记 in-sync，使 Explorer 状态列从「同步挂起」变为正常图标。
        if let Ok(hydrated) = &result {
            if hydrated.full_file {
                if let Some(root) = runtime.root.read().ok().and_then(|guard| guard.clone()) {
                    let local =
                        root.join(relative_for_mark.replace('/', std::path::MAIN_SEPARATOR_STR));
                    if let Err(error) = mark_path_in_sync(&local) {
                        log::warn!(
                            "native-sync mark in-sync failed after hydration: fs_id={file_id} error={error}"
                        );
                    }
                }
            }
        }
        // 水合写入会改 mtime：完成后刷新快照，避免把平台自己的写入误判成本地编辑。
        if let (Some(db), Ok(_)) = (db.as_ref(), &result) {
            if let Some(local) = root_path_for(&runtime, &relative_for_mark) {
                if let Ok(meta) = std::fs::metadata(&local) {
                    let _ = db.set_snapshot(file_id as i64, mtime_secs(&meta), meta.len());
                }
            }
        }
        if let (Some(db), Some(job_id)) = (db.as_ref(), job_id) {
            match &result {
                Ok(hydrated) => {
                    let state = if hydrated.full_file {
                        "hydrated"
                    } else {
                        "online_only"
                    };
                    let _ = db.hydration_finished(file_id, job_id, state, None);
                }
                Err(error) => {
                    let _ = db.hydration_finished(file_id, job_id, "error", Some(error));
                }
            }
        }
        if result.is_err() {
            unsafe {
                fail_fetch_from_keys(info.0, info.1, info.2, offset as i64, length);
            }
        }
        // P2: hydration job 完成/失败时推送一次事件（进度类高频变化不推送）。
        if let Ok(sink) = runtime.event_sink.read() {
            if let Some(sink) = sink.as_ref() {
                sink(crate::model::NativeSyncEvent {
                    fs_id: file_id,
                    success: result.is_ok(),
                });
            }
        }
        if let Ok(mut jobs) = runtime.cancelled.write() {
            jobs.remove(&info.2);
        }
    }
    unsafe fn fail_fetch_from_keys(
        connection: CF_CONNECTION_KEY,
        transfer: i64,
        request: i64,
        offset: i64,
        length: i64,
    ) {
        let op = CF_OPERATION_INFO {
            StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
            Type: CF_OPERATION_TYPE_TRANSFER_DATA,
            ConnectionKey: connection,
            TransferKey: transfer,
            RequestKey: request,
            ..Default::default()
        };
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: operation_param_size::<CF_OPERATION_PARAMETERS_0_6>(),
            ..Default::default()
        };
        params.Anonymous.TransferData = CF_OPERATION_PARAMETERS_0_6 {
            Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
            CompletionStatus: windows::Win32::Foundation::STATUS_CLOUD_FILE_UNSUCCESSFUL,
            Buffer: std::ptr::null(),
            Offset: offset,
            Length: length.max(0),
        };
        let _ = CfExecute(&op, &mut params);
    }
    unsafe extern "system" fn on_fetch_data(
        info_ptr: *const CF_CALLBACK_INFO,
        params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        let fetch = (&*params_ptr).Anonymous.FetchData;
        let Some(runtime) = runtime_from_callback(info) else {
            fail_fetch_from_keys(
                info.ConnectionKey,
                info.TransferKey,
                info.RequestKey,
                fetch.RequiredFileOffset,
                fetch.RequiredLength,
            );
            return;
        };
        // P1: 回调线程只做身份解析，映射解析（内存→SQLite 回退）在 worker 线程内完成。
        // P0: NormalizedPath 不含盘符（形如 \Users\someone\...），路径剥离仅作身份回退。
        let file_id = file_id_from_identity(info).or_else(|| {
            let normalized = info
                .NormalizedPath
                .to_string()
                .unwrap_or_default()
                .replace('\\', "/")
                .to_ascii_lowercase();
            let root = runtime
                .root
                .read()
                .ok()
                .and_then(|v| v.clone())
                .map(|p| p.to_string_lossy().replace('\\', "/").to_ascii_lowercase())
                .unwrap_or_default();
            relative_from_normalized(&normalized, &root)
                .and_then(|rel| {
                    runtime
                        .mappings
                        .read()
                        .ok()
                        .and_then(|m| m.get(&rel).cloned())
                })
                .map(|(_, file_id)| file_id)
        });
        let Some(file_id) = file_id else {
            log::warn!(
                "native-sync fetch rejected: unidentified file request_key={}",
                info.RequestKey
            );
            fail_fetch_from_keys(
                info.ConnectionKey,
                info.TransferKey,
                info.RequestKey,
                fetch.RequiredFileOffset,
                fetch.RequiredLength,
            );
            return;
        };
        let connection = info.ConnectionKey;
        let transfer = info.TransferKey;
        let request_key = info.RequestKey;
        let offset = fetch.RequiredFileOffset as u64;
        let length = fetch.RequiredLength;
        let Some(lease) = runtime.try_begin_worker() else {
            fail_fetch_from_keys(connection, transfer, request_key, offset as i64, length);
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        if let Ok(mut jobs) = runtime.cancelled.write() {
            jobs.insert(request_key, Arc::clone(&cancel));
        } else {
            fail_fetch_from_keys(connection, transfer, request_key, offset as i64, length);
            return;
        }
        // If disconnect raced between taking the lease and registering this
        // request, ensure it is cancelled even if it missed the drain scan.
        if runtime
            .lifecycle
            .lock()
            .map(|s| !s.accepting)
            .unwrap_or(true)
        {
            cancel.store(true, Ordering::SeqCst);
        }
        let worker_runtime = Arc::clone(&runtime);
        let started = thread::Builder::new()
            .name("pandock-native-sync-fetch".into())
            .spawn(move || {
                let _lease = lease;
                fetch_worker(
                    worker_runtime,
                    (connection, transfer, request_key),
                    file_id,
                    offset,
                    length,
                    cancel,
                )
            });
        if started.is_err() {
            if let Ok(mut jobs) = runtime.cancelled.write() {
                jobs.remove(&request_key);
            }
            fail_fetch_from_keys(connection, transfer, request_key, offset as i64, length);
        }
    }
    unsafe extern "system" fn on_cancel_fetch_data(
        info_ptr: *const CF_CALLBACK_INFO,
        _params: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        if let Some(runtime) = runtime_from_callback(info) {
            if let Ok(jobs) = runtime.cancelled.read() {
                if let Some(cancel) = jobs.get(&info.RequestKey) {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    unsafe extern "system" fn on_fetch_placeholders(
        info_ptr: *const CF_CALLBACK_INFO,
        _params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        let operation = CF_OPERATION_INFO {
            StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
            Type: CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS,
            ConnectionKey: info.ConnectionKey,
            TransferKey: info.TransferKey,
            RequestKey: info.RequestKey,
            ..Default::default()
        };
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: operation_param_size::<CF_OPERATION_PARAMETERS_0_7>(),
            ..Default::default()
        };
        params.Anonymous.TransferPlaceholders = CF_OPERATION_PARAMETERS_0_7 {
            Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
            CompletionStatus: NTSTATUS(0),
            PlaceholderTotalCount: 0,
            PlaceholderArray: std::ptr::null_mut(),
            PlaceholderCount: 0,
            EntriesProcessed: 0,
        };
        let _ = CfExecute(&operation, &mut params);
    }

    #[cfg(test)]
    mod recall_lookup_tests {
        use super::*;

        #[test]
        fn file_id_from_identity_reads_le_u64() {
            let id = 0x0102_0304_0506_0708u64;
            let mut info: CF_CALLBACK_INFO = unsafe { std::mem::zeroed() };
            let id_bytes = id.to_le_bytes();
            info.FileIdentity = id_bytes.as_ptr() as *const c_void;
            info.FileIdentityLength = 8;
            assert_eq!(file_id_from_identity(&info), Some(id));

            info.FileIdentityLength = 4;
            assert_eq!(file_id_from_identity(&info), None);
            info.FileIdentityLength = 8;
            info.FileIdentity = std::ptr::null();
            assert_eq!(file_id_from_identity(&info), None);
        }

        #[test]
        fn relative_lookup_handles_missing_drive_letter() {
            // CF NormalizedPath 实际形态（无盘符，spike 证据）
            let root = "c:/users/tester/documents/pandock";
            assert_eq!(
                relative_from_normalized("/users/tester/documents/pandock/a.txt", root),
                Some("a.txt".into())
            );
            assert_eq!(
                relative_from_normalized("c:/users/tester/documents/pandock/a.txt", root),
                Some("a.txt".into())
            );
            assert_eq!(
                relative_from_normalized("//?/c:/users/tester/documents/pandock/d/b.txt", root),
                Some("d/b.txt".into())
            );
            // 文档目录重定向到 D: 盘时 root 与 NormalizedPath 同侧比较仍成立
            let root_d = "d:/documents/pandock";
            assert_eq!(
                relative_from_normalized("/documents/pandock/x.bin", root_d),
                Some("x.bin".into())
            );
            assert_eq!(
                relative_from_normalized("/users/tester/other/a.txt", root),
                None
            );
            assert_eq!(relative_from_normalized("/a.txt", ""), None);
        }

        #[test]
        fn resolve_mapping_prefers_memory_and_falls_back_to_db() {
            let dir = tempfile::tempdir().unwrap();
            let db = crate::db::IndexDb::open(dir.path()).unwrap();
            let record = |fs_id: i64, remote: &str, rel: &str| crate::model::EntryRecord {
                fs_id,
                local_only: false,
                parent_fs_id: None,
                remote_path: remote.into(),
                relative_path: rel.into(),
                name: rel.rsplit('/').next().unwrap().into(),
                is_dir: false,
                size: 3,
                md5: None,
                modified_at: None,
                state: crate::model::NativeSyncState::OnlineOnly,
                pin_state: crate::model::PinState::Unpinned,
                error: None,
                has_children: false,

                pending_op: None,
                pending_since: None,
            };
            db.upsert(&record(77, "/a.txt", "a.txt")).unwrap();
            let runtime = Runtime::default();
            // 内存未命中 → SQLite 回退
            let resolved = resolve_mapping(&runtime, Some(&db), 77).unwrap();
            assert_eq!(resolved.0.as_str(), "/a.txt");
            assert_eq!(resolved.1, "a.txt");
            // 内存与 db 都没有 → None
            assert!(resolve_mapping(&runtime, Some(&db), 404).is_none());
            assert!(resolve_mapping(&runtime, None, 77).is_none());
            // 内存命中 → 不需要 db
            runtime
                .by_id
                .write()
                .unwrap()
                .insert(99, (RemotePath::parse("/x").unwrap(), "x".into()));
            assert_eq!(
                resolve_mapping(&runtime, None, 99).unwrap().0.as_str(),
                "/x"
            );
        }
        #[test]
        fn set_remote_mapping_populates_both_indexes() {
            let mut platform = WindowsPlatform::default();
            let remote = RemotePath::parse("/a.txt").unwrap();
            platform.set_remote_mapping("A.TXT", remote.clone(), 42);
            let by_id = platform.runtime.by_id.read().unwrap();
            let (stored_remote, rel) = by_id.get(&42).unwrap();
            assert_eq!(stored_remote.as_str(), "/a.txt");
            assert_eq!(rel, "a.txt");
            drop(by_id);
            assert!(platform
                .runtime
                .mappings
                .read()
                .unwrap()
                .contains_key("a.txt"));
            let _ = remote;
        }
    }
    #[cfg(test)]
    mod placeholder_create_tests {
        use super::*;

        #[test]
        fn windows_platform_stores_and_invokes_event_sink() {
            let mut platform = WindowsPlatform::default();
            let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let captured = hits.clone();
            platform.set_event_sink(Some(Arc::new(move |_| {
                captured.fetch_add(1, Ordering::SeqCst);
            })));
            let sink = platform
                .runtime
                .event_sink
                .read()
                .unwrap()
                .clone()
                .expect("sink stored");
            sink(crate::model::NativeSyncEvent {
                fs_id: 7,
                success: true,
            });
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        }
        #[test]
        fn set_state_db_stores_path_for_worker_persistence() {
            let mut platform = WindowsPlatform::default();
            let path = std::path::Path::new("C:\\data\\native-sync\\sync.db");
            platform.set_state_db(path);
            assert_eq!(
                platform.runtime.state_db.read().unwrap().as_deref(),
                Some(path)
            );
        }
        #[test]
        fn placeholder_metadata_declares_size_without_in_sync() {
            let metadata = placeholder_fs_metadata(103_276);
            assert_eq!(metadata.FileSize, 103_276);
            assert_eq!(metadata.BasicInfo.FileAttributes, FILE_ATTRIBUTE_NORMAL.0);
            // 在线占位符绝不允许 MARK_IN_SYNC：否则 Explorer 认为内容在本地，
            // 双击不触发 FETCH_DATA，消费者读到全零（P0 根因）。
            assert_eq!(
                PLACEHOLDER_CREATE_FLAGS.0,
                CF_PLACEHOLDER_CREATE_FLAG_NONE.0
            );
            assert_eq!(
                PLACEHOLDER_CREATE_FLAGS.0 & CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC.0,
                0
            );
        }
    }
    #[cfg(test)]
    mod notify_capture_tests {
        use super::*;

        fn runtime_with(root: &Path, db_path: &Path) -> Arc<Runtime> {
            let runtime = Arc::new(Runtime::default());
            *runtime.root.write().unwrap() = Some(root.to_path_buf());
            *runtime.state_db.write().unwrap() = Some(db_path.to_path_buf());
            runtime
        }

        fn captured(
            runtime: &Arc<Runtime>,
        ) -> Arc<Mutex<Vec<crate::model::NativeSyncChangeEvent>>> {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&seen);
            *runtime.change_sink.write().unwrap() = Some(Arc::new(move |event| {
                sink.lock().unwrap().push(event);
            }));
            seen
        }

        fn wait_for(pred: impl Fn() -> bool, what: &str) {
            for _ in 0..600 {
                if pred() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("超时等待：{what}");
        }

        fn delete_task(root: &Path, relative: &str, fs_id: u64) -> NotifyTask {
            NotifyTask {
                input: crate::notify::NotifyInput {
                    kind: Some(crate::notify::NotifyKind::Delete),
                    normalized_path: Some(format!(
                        "{}/{}",
                        root.to_string_lossy().replace('\\', "/"),
                        relative
                    )),
                    ..Default::default()
                },
                fs_id: Some(fs_id),
                identity: None,
            }
        }

        fn upsert_file(db: &crate::db::IndexDb, fs_id: i64, relative: &str) {
            db.upsert(&crate::model::EntryRecord {
                fs_id,
                local_only: false,
                parent_fs_id: None,
                remote_path: format!("/{relative}"),
                relative_path: relative.into(),
                name: relative.rsplit('/').next().unwrap().into(),
                is_dir: false,
                size: 3,
                md5: None,
                modified_at: None,
                state: crate::model::NativeSyncState::OnlineOnly,
                pin_state: crate::model::PinState::Unpinned,
                error: None,
                has_children: false,
                pending_op: None,
                pending_since: None,
            })
            .unwrap();
        }

        /// 打开者身份解码：正常结构、空指针、过短 StructSize、空 ImagePath 四种形态。
        /// 全部只用栈上构造的假结构，不依赖真实 Explorer 或云文件驱动。
        #[test]
        fn process_identity_decodes_opener_and_rejects_short_structures() {
            let image: Vec<u16> = "C:\\Windows\\explorer.exe\0".encode_utf16().collect();
            let mut process = CF_PROCESS_INFO {
                StructSize: std::mem::size_of::<CF_PROCESS_INFO>() as u32,
                ProcessId: 4242,
                ImagePath: PCWSTR(image.as_ptr()),
                ..Default::default()
            };
            let mut info: CF_CALLBACK_INFO = unsafe { std::mem::zeroed() };
            info.ProcessInfo = &mut process;
            assert_eq!(
                unsafe { process_identity(&info) },
                Some(crate::notify::ProcessIdentity {
                    pid: Some(4242),
                    image: Some("explorer.exe".into()),
                })
            );

            // 系统没给 ProcessInfo（或连接未要求）⇒ 一律 None，不得解引用。
            let mut bare: CF_CALLBACK_INFO = unsafe { std::mem::zeroed() };
            bare.ProcessInfo = std::ptr::null_mut();
            assert_eq!(unsafe { process_identity(&bare) }, None);

            // StructSize 比我们读的字段还短 ⇒ 不猜，直接放弃。
            let mut short = CF_PROCESS_INFO {
                StructSize: 4,
                ProcessId: 1,
                ..Default::default()
            };
            let mut short_info: CF_CALLBACK_INFO = unsafe { std::mem::zeroed() };
            short_info.ProcessInfo = &mut short;
            assert_eq!(unsafe { process_identity(&short_info) }, None);

            // ImagePath 为空指针 ⇒ 只留 pid，绝不 panic。
            let mut no_image = CF_PROCESS_INFO {
                StructSize: std::mem::size_of::<CF_PROCESS_INFO>() as u32,
                ProcessId: 7,
                ImagePath: PCWSTR(std::ptr::null::<u16>()),
                ..Default::default()
            };
            let mut no_image_info: CF_CALLBACK_INFO = unsafe { std::mem::zeroed() };
            no_image_info.ProcessInfo = &mut no_image;
            assert_eq!(
                unsafe { process_identity(&no_image_info) },
                Some(crate::notify::ProcessIdentity {
                    pid: Some(7),
                    image: None,
                })
            );
        }

        /// 回调表必须自洽：13 个真实回调 + NONE 终止项，类型不得重复。
        #[test]
        fn callback_table_registers_every_s1_callback_and_terminates_with_none() {
            assert_eq!(CALLBACKS.len(), 14);
            assert_eq!(CALLBACKS[13].Type, CF_CALLBACK_TYPE_NONE);
            assert!(CALLBACKS[13].Callback.is_none());
            let mut types: Vec<i32> = CALLBACKS[..13].iter().map(|c| c.Type.0).collect();
            types.sort_unstable();
            let before = types.len();
            types.dedup();
            assert_eq!(types.len(), before, "回调类型不得重复");
            for entry in &CALLBACKS[..13] {
                assert!(entry.Callback.is_some(), "真实回调必须挂上处理函数");
            }
            // ADR-108 记录的三个只读召回必须保留。
            for required in [
                CF_CALLBACK_TYPE_FETCH_DATA,
                CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
                CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
            ] {
                assert!(CALLBACKS[..13].iter().any(|c| c.Type == required));
            }
            // S1 新增的本地变更捕获。
            for required in [
                CF_CALLBACK_TYPE_NOTIFY_DELETE,
                CF_CALLBACK_TYPE_NOTIFY_DELETE_COMPLETION,
                CF_CALLBACK_TYPE_NOTIFY_RENAME,
                CF_CALLBACK_TYPE_NOTIFY_RENAME_COMPLETION,
                CF_CALLBACK_TYPE_NOTIFY_DEHYDRATE,
                CF_CALLBACK_TYPE_NOTIFY_DEHYDRATE_COMPLETION,
                CF_CALLBACK_TYPE_NOTIFY_FILE_OPEN_COMPLETION,
                CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION,
                CF_CALLBACK_TYPE_VALIDATE_DATA,
                CF_CALLBACK_TYPE_CANCEL_FETCH_PLACEHOLDERS,
            ] {
                assert!(CALLBACKS[..13].iter().any(|c| c.Type == required));
            }
        }

        /// 投递绝不阻塞：开关关闭、队列满、队列无消费者都必须安全返回 false。
        #[test]
        fn enqueue_never_blocks_and_respects_the_switch_and_queue_bound() {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("native-sync").join("sync.db");
            let runtime = runtime_with(dir.path(), &db_path);
            let task = || NotifyTask {
                input: crate::notify::NotifyInput {
                    kind: Some(crate::notify::NotifyKind::Delete),
                    ..Default::default()
                },
                fs_id: Some(1),
                identity: None,
            };

            // 未启动消费者：投递失败但不阻塞、不 panic。
            runtime.notifications_enabled.store(true, Ordering::Relaxed);
            assert!(!enqueue_notification(&runtime, task()));

            // 启动后正常入队。
            start_notify_worker(&runtime);
            assert!(enqueue_notification(&runtime, task()));

            // 开关关闭后不再收。
            runtime
                .notifications_enabled
                .store(false, Ordering::Relaxed);
            assert!(!enqueue_notification(&runtime, task()));
            runtime.notifications_enabled.store(true, Ordering::Relaxed);

            // 队列满时丢弃（消费者已被停止，队列没有读者）。
            stop_notify_worker(&runtime);
        }

        /// 自杀回调去重：应用自身的操作守卫必须「一次操作抵消一次通知」。
        #[test]
        fn app_operation_guard_consumes_exactly_one_self_callback() {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("native-sync").join("sync.db");
            let root = dir.path().join("root");
            std::fs::create_dir_all(root.join("d")).unwrap();
            let runtime = runtime_with(&root, &db_path);

            note_app_path_op(&runtime, &root.join("d").join("f.txt"));
            assert!(consume_app_path_op(&runtime, "d/f.txt"));
            assert!(
                !consume_app_path_op(&runtime, "d/f.txt"),
                "守卫只能被消费一次"
            );
            assert!(!consume_app_path_op(&runtime, "d/other.txt"));

            note_app_fs_op(&runtime, 42);
            assert!(consume_app_fs_op(&runtime, 42));
            assert!(!consume_app_fs_op(&runtime, 42));
        }

        /// 回归：stop→start（“停用 NativeSync” → “启用同步”）必须能重新建立通道与线程。
        ///
        /// 历史缺陷：`stop_notify_worker` 只置空 sender、不复位 `notify_worker_started`，
        /// 于是下一次 start 直接提前返回，投递从此静默失败——整个 S1 在用户点过一次
        /// 停用/启用之后永久哑掉，直到重启应用。
        #[test]
        fn notify_worker_is_recreated_after_stop_then_start() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            let db_path = dir.path().join("native-sync").join("sync.db");
            std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            let db = crate::db::IndexDb::open(dir.path()).unwrap();
            upsert_file(&db, 31, "restart.txt");
            let runtime = runtime_with(&root, &db_path);
            runtime.notifications_enabled.store(true, Ordering::Relaxed);
            let seen = captured(&runtime);

            start_notify_worker(&runtime);
            assert!(enqueue_notification(
                &runtime,
                delete_task(&root, "restart.txt", 31)
            ));
            wait_for(|| seen.lock().unwrap().len() == 1, "首次投递被消费");

            // disconnect() 路径：停投递 + 停接收。
            stop_notify_worker(&runtime);
            assert!(
                !enqueue_notification(&runtime, delete_task(&root, "restart.txt", 31)),
                "停止后不得投递"
            );

            // connect() 路径：必须重建通道与消费者线程。
            start_notify_worker(&runtime);
            assert!(
                enqueue_notification(&runtime, delete_task(&root, "restart.txt", 31)),
                "stop→start 之后必须能重新投递（生命周期标记未复位曾导致永久哑掉）"
            );
            wait_for(|| seen.lock().unwrap().len() == 2, "重启后的投递被消费");
        }

        /// 回归：开关关掉再打开（set_notifications_enabled 入口）同样必须重建通道。
        #[test]
        fn notifications_switch_reenable_recreates_the_channel() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            let db_path = dir.path().join("native-sync").join("sync.db");
            std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            let db = crate::db::IndexDb::open(dir.path()).unwrap();
            upsert_file(&db, 32, "switch.txt");
            let mut platform = WindowsPlatform::default();
            platform.register(&root).unwrap();
            platform.set_state_db(&db_path);
            let runtime = Arc::clone(&platform.runtime);
            let seen = captured(&runtime);

            platform.set_notifications_enabled(true);
            assert!(enqueue_notification(
                &runtime,
                delete_task(&root, "switch.txt", 32)
            ));
            wait_for(|| seen.lock().unwrap().len() == 1, "开关打开后投递被消费");

            platform.set_notifications_enabled(false);
            assert!(
                !enqueue_notification(&runtime, delete_task(&root, "switch.txt", 32)),
                "开关关闭后不得投递"
            );

            platform.set_notifications_enabled(true);
            assert!(
                enqueue_notification(&runtime, delete_task(&root, "switch.txt", 32)),
                "重新打开开关必须重建通道"
            );
            wait_for(|| seen.lock().unwrap().len() == 2, "重新打开后的投递被消费");
        }

        /// 只读相关的通知（open/close/validate）**不得**在 local_change 里留下记录：
        /// 它们只是观测，不是本地变更。否则"读一次文件"就会被记成变更（真实事故）。
        /// 同时保证真正的变更（delete intent）仍然照常入账。
        #[test]
        fn read_only_notifications_leave_no_local_change_rows() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            let db_path = dir.path().join("native-sync").join("sync.db");
            std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            let db = crate::db::IndexDb::open(dir.path()).unwrap();
            upsert_file(&db, 61, "read.txt");
            std::fs::write(root.join("read.txt"), b"content").unwrap();
            let runtime = runtime_with(&root, &db_path);
            let _seen = captured(&runtime);
            let normalized = format!("{}/read.txt", root.to_string_lossy().replace('\\', "/"));
            let task = |kind| NotifyTask {
                input: crate::notify::NotifyInput {
                    kind: Some(kind),
                    normalized_path: Some(normalized.clone()),
                    ..Default::default()
                },
                fs_id: Some(61),
                identity: Some(crate::notify::ProcessIdentity {
                    pid: Some(9999),
                    image: Some("explorer.exe".into()),
                }),
            };

            // 打开 → 关闭（只读一遍）：不得留下任何 local_change，也不得产生待同步。
            handle_notification(&runtime, task(crate::notify::NotifyKind::OpenCompleted));
            handle_notification(&runtime, task(crate::notify::NotifyKind::CloseCompleted));
            handle_notification(&runtime, task(crate::notify::NotifyKind::ValidateData));
            assert_eq!(
                db.recent_local_changes(10).unwrap().len(),
                0,
                "观测类通知不得写入 local_change"
            );
            assert_eq!(db.pending_op_of(61).unwrap(), None);

            // 真正的变更（删除意图）仍然入账。
            handle_notification(&runtime, task(crate::notify::NotifyKind::Delete));
            let changes = db.recent_local_changes(10).unwrap();
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].kind, "delete");
            assert_eq!(db.pending_op_of(61).unwrap().as_deref(), Some("delete"));
        }

        /// 删除通知落库：pending_op=delete + local_change + 事件（来源 explorer）。
        #[test]
        fn delete_notification_marks_pending_and_emits_event() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            let db_path = dir.path().join("native-sync").join("sync.db");
            std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            let db = crate::db::IndexDb::open(dir.path()).unwrap();
            upsert_file(&db, 11, "a.txt");
            let runtime = runtime_with(&root, &db_path);
            let seen = captured(&runtime);

            handle_notification(
                &runtime,
                NotifyTask {
                    input: crate::notify::NotifyInput {
                        kind: Some(crate::notify::NotifyKind::Delete),
                        normalized_path: Some(format!(
                            "{}/a.txt",
                            root.to_string_lossy().replace('\\', "/")
                        )),
                        ..Default::default()
                    },
                    fs_id: Some(11),
                    identity: None,
                },
            );

            assert_eq!(db.pending_op_of(11).unwrap().as_deref(), Some("delete"));
            let events = seen.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].kind, "delete");
            assert_eq!(events[0].source, "explorer");
            assert_eq!(events[0].from_relative.as_deref(), Some("a.txt"));
            let changes = db.recent_local_changes(10).unwrap();
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].kind, "delete");
            assert!(!changes[0].applied_to_cloud, "S1 不上传");
        }

        /// 应用自身的回调（命中守卫）不得被记成用户的本地变更。
        #[test]
        fn self_callback_is_recorded_as_app_and_does_not_mark_pending() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            let db_path = dir.path().join("native-sync").join("sync.db");
            std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            let db = crate::db::IndexDb::open(dir.path()).unwrap();
            upsert_file(&db, 12, "b.txt");
            let runtime = runtime_with(&root, &db_path);
            let seen = captured(&runtime);
            note_app_path_op(&runtime, &root.join("b.txt"));

            handle_notification(
                &runtime,
                NotifyTask {
                    input: crate::notify::NotifyInput {
                        kind: Some(crate::notify::NotifyKind::Delete),
                        normalized_path: Some(format!(
                            "{}/b.txt",
                            root.to_string_lossy().replace('\\', "/")
                        )),
                        ..Default::default()
                    },
                    fs_id: Some(12),
                    identity: None,
                },
            );

            assert_eq!(
                db.pending_op_of(12).unwrap(),
                None,
                "自杀回调不得产生待同步标记"
            );
            let events = seen.lock().unwrap();
            assert_eq!(events[0].source, "app");
        }

        /// 本地编辑：关闭完成后快照不一致 -> pending_op=upload。
        #[test]
        fn close_completion_detects_local_edit_via_snapshot_diff() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            let db_path = dir.path().join("native-sync").join("sync.db");
            std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            let db = crate::db::IndexDb::open(dir.path()).unwrap();
            upsert_file(&db, 21, "edit.txt");
            // 占位符创建时的快照：3 字节。
            db.set_snapshot(21, Some(1_700_000_000), 3).unwrap();
            std::fs::write(root.join("edit.txt"), b"0123456789").unwrap();
            let runtime = runtime_with(&root, &db_path);
            let seen = captured(&runtime);

            detect_local_write(
                &runtime,
                &db,
                &crate::model::NativeSyncChangeEvent {
                    kind: "close".into(),
                    phase: "completion".into(),
                    fs_id: Some(21),
                    from_relative: Some("edit.txt".into()),
                    to_relative: None,
                    is_dir: false,
                    reason: None,
                    source: "system".into(),
                    detected_at: "1700000100".into(),
                    pending_cloud: true,
                },
            );

            assert_eq!(db.pending_op_of(21).unwrap().as_deref(), Some("upload"));
            let events = seen.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].kind, "upload");
            assert_eq!(events[0].fs_id, Some(21));
        }

        /// 事故回归：**没有可用基线**（老库行 `snapshot_* IS NULL`）时，
        /// 关闭完成通知不得把"只读打开"判成"本地编辑"。
        ///
        /// 真实事故：S1 之前水合的条目没有快照列，`snapshot_for` 把 NULL 折叠成 0，
        /// 于是仅用 `Get-FileHash` 读一遍三个文件，就给它们全写了 `pending_op='upload'`。
        /// 正确行为：第一次见到无基线的条目时**只补记基线、不判定**；此后真正的修改仍要能捕获。
        #[test]
        fn close_completion_without_baseline_does_not_flag_read_as_edit() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            let db_path = dir.path().join("native-sync").join("sync.db");
            std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            let db = crate::db::IndexDb::open(dir.path()).unwrap();
            upsert_file(&db, 41, "legacy.txt");
            // 故意不写快照：模拟 S1 之前水合的老库行（snapshot_mtime/size 为 NULL）。
            assert_eq!(
                db.snapshot_for(41).unwrap().and_then(|s| s.size),
                None,
                "未记录快照时应视为没有可用基线"
            );
            std::fs::write(root.join("legacy.txt"), b"read-only-open").unwrap();
            let runtime = runtime_with(&root, &db_path);
            let seen = captured(&runtime);
            let close_event = |at: &str| crate::model::NativeSyncChangeEvent {
                kind: "close".into(),
                phase: "completion".into(),
                fs_id: Some(41),
                from_relative: Some("legacy.txt".into()),
                to_relative: None,
                is_dir: false,
                reason: None,
                source: "system".into(),
                detected_at: at.into(),
                pending_cloud: true,
            };

            // 只读打开后关闭：不得产生待同步。
            detect_local_write(&runtime, &db, &close_event("1791773770"));
            assert_eq!(
                db.pending_op_of(41).unwrap(),
                None,
                "没有基线时不得把读取判成本地编辑"
            );
            assert!(seen.lock().unwrap().is_empty());
            // 但必须把当前状态补记为基线，供后续判定使用。
            let actual_len = std::fs::metadata(root.join("legacy.txt")).unwrap().len();
            let size = db.snapshot_for(41).unwrap().and_then(|s| s.size);
            assert_eq!(size, Some(actual_len), "无基线时应补记基线");

            // 之后真正的本地编辑仍必须被捕获。
            std::fs::write(root.join("legacy.txt"), b"read-only-open-and-then-edited").unwrap();
            detect_local_write(&runtime, &db, &close_event("1791773800"));
            assert_eq!(db.pending_op_of(41).unwrap().as_deref(), Some("upload"));
            assert_eq!(seen.lock().unwrap().len(), 1);
        }

        /// 快照一致（未被改动）不得产生任何待同步标记。
        #[test]
        fn close_completion_without_snapshot_drift_is_ignored() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            let db_path = dir.path().join("native-sync").join("sync.db");
            std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            let db = crate::db::IndexDb::open(dir.path()).unwrap();
            upsert_file(&db, 22, "same.txt");
            std::fs::write(root.join("same.txt"), b"abc").unwrap();
            let meta = std::fs::metadata(root.join("same.txt")).unwrap();
            let mtime = mtime_secs(&meta).unwrap();
            db.set_snapshot(22, Some(mtime), 3).unwrap();
            let runtime = runtime_with(&root, &db_path);
            let seen = captured(&runtime);

            detect_local_write(
                &runtime,
                &db,
                &crate::model::NativeSyncChangeEvent {
                    kind: "close".into(),
                    phase: "completion".into(),
                    fs_id: Some(22),
                    from_relative: Some("same.txt".into()),
                    to_relative: None,
                    is_dir: false,
                    reason: None,
                    source: "system".into(),
                    detected_at: "1700000100".into(),
                    pending_cloud: true,
                },
            );

            assert_eq!(db.pending_op_of(22).unwrap(), None);
            assert!(seen.lock().unwrap().is_empty());
        }

        /// 本地新建真实文件：登记为 local_only 条目，fs_id 不下发负数。
        #[test]
        fn close_completion_registers_locally_created_file() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            let db_path = dir.path().join("native-sync").join("sync.db");
            std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            let db = crate::db::IndexDb::open(dir.path()).unwrap();
            std::fs::write(root.join("new.txt"), b"hello").unwrap();
            let runtime = runtime_with(&root, &db_path);
            let seen = captured(&runtime);

            detect_local_write(
                &runtime,
                &db,
                &crate::model::NativeSyncChangeEvent {
                    kind: "close".into(),
                    phase: "completion".into(),
                    fs_id: None,
                    from_relative: Some("new.txt".into()),
                    to_relative: None,
                    is_dir: false,
                    reason: None,
                    source: "system".into(),
                    detected_at: "1700000100".into(),
                    pending_cloud: true,
                },
            );

            let row = db
                .entry_by_relative("new.txt")
                .unwrap()
                .expect("本地新建条目");
            assert!(row.5, "必须标记为 local_only");
            assert_eq!(db.pending_op_of(row.0).unwrap().as_deref(), Some("upload"));
            {
                let events = seen.lock().unwrap();
                assert_eq!(events[0].kind, "create");
                assert_eq!(events[0].fs_id, None, "本地新建不得暴露负数 fs_id");
            }

            // 再次触发不会重复记账。
            detect_local_write(
                &runtime,
                &db,
                &crate::model::NativeSyncChangeEvent {
                    kind: "close".into(),
                    phase: "completion".into(),
                    fs_id: None,
                    from_relative: Some("new.txt".into()),
                    to_relative: None,
                    is_dir: false,
                    reason: None,
                    source: "system".into(),
                    detected_at: "1700000200".into(),
                    pending_cloud: true,
                },
            );
            assert_eq!(seen.lock().unwrap().len(), 1);
        }
    }

    #[cfg(test)]
    mod lifecycle_tests {
        use super::*;
        use std::sync::mpsc;
        use std::time::Duration;

        #[test]
        fn disconnect_drain_rejects_new_workers_and_waits_for_active_lease() {
            let runtime = Arc::new(Runtime::default());
            runtime.start_accepting();
            let lease = runtime.try_begin_worker().expect("accepting workers");
            let cancelled = Arc::new(AtomicBool::new(false));
            runtime
                .cancelled
                .write()
                .unwrap()
                .insert(42, Arc::clone(&cancelled));
            let (tx, rx) = mpsc::channel();
            let draining = Arc::clone(&runtime);
            let thread = thread::spawn(move || {
                draining.stop_accepting_and_wait();
                tx.send(()).unwrap();
            });
            for _ in 0..100 {
                if cancelled.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(cancelled.load(Ordering::SeqCst));
            assert!(runtime.try_begin_worker().is_none());
            assert!(rx.recv_timeout(Duration::from_millis(10)).is_err());
            drop(lease);
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
            thread.join().unwrap();
        }
    }
    // ======================= 本地变更通知（S1） =======================
    //
    // 设计约束（来自 CFAPI 纪律，违反会导致 Explorer 卡死或误删本地文件）：
    //   A. 阻塞型回调（NOTIFY_DEHYDRATE / NOTIFY_DELETE / NOTIFY_RENAME）必须先
    //      **同步内联 ACK**；ACK 之前不得做任何 I/O、加锁或日志落盘。
    //      「ACK = 确认并执行」——平台收到成功 ACK 才真正落地删除/改名/脱水。
    //   B. 非阻塞通知必须快速投递到 worker，不能在回调线程里查库/联网。
    //   C. 应用自身发起的 CF 操作会产生「自杀回调」，必须按来源去重。
    //   D. 紧急开关（配置字段 + 环境变量）可整体关掉捕获。
    const APP_OP_TTL: std::time::Duration = std::time::Duration::from_secs(30);
    const NOTIFY_QUEUE_DEPTH: usize = 256;

    /// 打开者身份日志的记账表（进程级，键是 `(fs_id, image)`）：同一个文件被同一个程序
    /// 反复打开时只留一条 info 证据，不刷日志。
    static IDENTITY_LOG: OnceLock<Mutex<crate::notify::IdentityLogLimiter>> = OnceLock::new();

    struct NotifyTask {
        input: crate::notify::NotifyInput,
        fs_id: Option<u64>,
        /// 打开者身份。`CF_PROCESS_INFO` 只在回调期间有效，回调线程已把它拷成
        /// 拥有所有权的值，worker 线程拿到的绝不是裸指针。
        identity: Option<crate::notify::ProcessIdentity>,
    }

    fn now_iso() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{secs}")
    }

    /// 回调线程内只用它做「字符串 → 相对路径」的便宜计算（无 I/O、无 DB）。
    unsafe fn normalized_text(info: &CF_CALLBACK_INFO) -> Option<String> {
        info.NormalizedPath.to_string().ok()
    }

    /// 读取 `CF_CALLBACK_INFO.ProcessInfo`（连接已要求 `CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO`）。
    ///
    /// 安全前提：`ProcessInfo` 指向的结构体与其字符串**只在回调期间有效**，因此这里立刻
    /// 把需要的两项拷成拥有所有权的值，绝不把指针带出回调。结构体短于我们要读的字段
    /// 一律拒绝（先校验 `StructSize`，再读字段）。
    unsafe fn process_identity(info: &CF_CALLBACK_INFO) -> Option<crate::notify::ProcessIdentity> {
        let process = info.ProcessInfo;
        if process.is_null() {
            return None;
        }
        let minimal =
            std::mem::offset_of!(CF_PROCESS_INFO, ImagePath) + std::mem::size_of::<PCWSTR>();
        if ((*process).StructSize as usize) < minimal {
            return None;
        }
        let image = pcwstr_text((*process).ImagePath).and_then(|p| crate::notify::image_label(&p));
        Some(crate::notify::ProcessIdentity {
            pid: Some((*process).ProcessId),
            image,
        })
    }

    /// `PCWSTR` → `String`：显式判空 + 有界扫描，不依赖「字符串一定已终止」。
    unsafe fn pcwstr_text(p: PCWSTR) -> Option<String> {
        if p.0.is_null() {
            return None;
        }
        const MAX_CHARS: usize = 4096;
        let mut len = 0usize;
        while len < MAX_CHARS && *p.0.add(len) != 0 {
            len += 1;
        }
        if len == 0 {
            return None;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(p.0, len));
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// 记录一次「应用自身发起」的操作，用于去重随之而来的自杀回调。
    fn note_app_path_op(runtime: &Runtime, path: &Path) {
        let Some(root) = runtime
            .root
            .read()
            .ok()
            .and_then(|v| v.clone())
            .map(|p| p.to_string_lossy().to_string())
        else {
            return;
        };
        let key = path
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        if let Some(rel) = crate::notify::relative_from_normalized(&key, &root) {
            if let Ok(mut ops) = runtime.app_path_ops.lock() {
                let now = std::time::Instant::now();
                ops.retain(|_, at| now.duration_since(*at) < APP_OP_TTL);
                ops.insert(rel.replace('\\', "/").to_ascii_lowercase(), now);
            }
        }
    }

    fn note_app_fs_op(runtime: &Runtime, fs_id: u64) {
        if let Ok(mut ops) = runtime.app_fs_ops.lock() {
            let now = std::time::Instant::now();
            ops.retain(|_, at| now.duration_since(*at) < APP_OP_TTL);
            ops.insert(fs_id, now);
        }
    }

    /// 消费一次守卫（命中即移除：一次应用操作只抵消一次自杀回调）。
    fn consume_app_path_op(runtime: &Runtime, relative: &str) -> bool {
        let key = relative.replace('\\', "/").to_ascii_lowercase();
        runtime
            .app_path_ops
            .lock()
            .ok()
            .and_then(|mut ops| {
                let hit = ops.remove(&key)?;
                Some(std::time::Instant::now().duration_since(hit) < APP_OP_TTL)
            })
            .unwrap_or(false)
    }

    fn consume_app_fs_op(runtime: &Runtime, fs_id: u64) -> bool {
        runtime
            .app_fs_ops
            .lock()
            .ok()
            .and_then(|mut ops| {
                let hit = ops.remove(&fs_id)?;
                Some(std::time::Instant::now().duration_since(hit) < APP_OP_TTL)
            })
            .unwrap_or(false)
    }

    /// 通道缺失 / 已断开的告警限频：这类失效必须在日志里可见，否则
    /// 「通知捕获静默哑掉」在日志里完全查不到，但也不能刷爆日志。
    static LAST_CHANNEL_WARN_MS: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    const CHANNEL_WARN_INTERVAL_MS: u64 = 60_000;

    fn warn_notify_channel_unavailable() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = LAST_CHANNEL_WARN_MS.load(Ordering::Relaxed);
        if now.saturating_sub(last) < CHANNEL_WARN_INTERVAL_MS {
            return;
        }
        LAST_CHANNEL_WARN_MS.store(now, Ordering::Relaxed);
        log::warn!(
            "native-sync notify channel unavailable; local change event dropped \
             (provider disconnected, or capture disabled?)"
        );
    }

    /// 非阻塞投递：队列满即丢弃并告警（绝不阻塞 Explorer 回调线程）。
    fn enqueue_notification(runtime: &Arc<Runtime>, task: NotifyTask) -> bool {
        if !runtime.notifications_enabled.load(Ordering::Relaxed) {
            return false;
        }
        let sender = runtime.notify_tx.read().ok().and_then(|v| v.clone());
        let Some(sender) = sender else {
            warn_notify_channel_unavailable();
            return false;
        };
        match sender.try_send(task) {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                log::warn!("native-sync notify queue full; dropped one event");
                false
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                warn_notify_channel_unavailable();
                false
            }
        }
    }

    /// 记录「谁打开了这个占位符」。
    ///
    /// 目的：回答「释放空间之后是谁把文件读了回来」。只在 info 级、按 `(fs_id, image)`
    /// 60 秒限频，且**绝不记录文件名**（只记录可执行文件基名与 pid）。
    /// `identity` 缺失时照记 `image=unknown`——「拿不到身份」本身也是证据。
    fn log_opener_identity(
        fs_id: Option<u64>,
        kind: crate::notify::NotifyKind,
        identity: Option<&crate::notify::ProcessIdentity>,
    ) {
        let key_id = fs_id.unwrap_or(0);
        let image = identity
            .and_then(|i| i.image.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let pid = identity.and_then(|i| i.pid);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let allowed = IDENTITY_LOG
            .get_or_init(|| {
                Mutex::new(crate::notify::IdentityLogLimiter::new(
                    crate::notify::IDENTITY_LOG_WINDOW_MS,
                ))
            })
            .lock()
            .map(|mut limiter| limiter.should_log(key_id, &image, now_ms))
            .unwrap_or(false);
        if allowed {
            log::info!(
                "native-sync placeholder opener kind={} fs_id={} image={} pid={:?}",
                kind.as_str(),
                key_id,
                image,
                pid
            );
        }
    }

    /// 非阻塞通知的统一入口：**不得**在这里做 I/O / 查库 / 落盘。
    fn dispatch_notification(info: &CF_CALLBACK_INFO, input: crate::notify::NotifyInput) {
        let Some(runtime) = (unsafe { runtime_from_callback(info) }) else {
            return;
        };
        // 只在打开/关闭通知上取发起者身份：这两条正是「谁把文件读了回来」的证据面；
        // 其余回调不取，避免无谓的字符串拷贝。
        let identity = match input.kind {
            Some(
                crate::notify::NotifyKind::OpenCompleted
                | crate::notify::NotifyKind::CloseCompleted,
            ) => unsafe { process_identity(info) },
            _ => None,
        };
        let task = NotifyTask {
            input,
            fs_id: file_id_from_identity(info),
            identity,
        };
        enqueue_notification(&runtime, task);
    }

    /// 单消费者：所有落库/发事件都在这条线程上，回调线程只投递。
    fn notify_worker_loop(runtime: Arc<Runtime>, rx: std::sync::mpsc::Receiver<NotifyTask>) {
        while let Ok(task) = rx.recv() {
            handle_notification(&runtime, task);
        }
    }

    fn handle_notification(runtime: &Arc<Runtime>, task: NotifyTask) {
        let root_text = runtime
            .root
            .read()
            .ok()
            .and_then(|v| v.clone())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        // 来源判定：命中应用守卫 => App（自杀回调，不得记为用户的本地变更）。
        let kind = task.input.kind;
        let relative_key = task
            .input
            .normalized_path
            .as_deref()
            .and_then(|p| crate::notify::relative_from_normalized(p, &root_text));
        let is_app_op = relative_key
            .as_deref()
            .map(|rel| consume_app_path_op(runtime, rel))
            .unwrap_or(false)
            || task
                .fs_id
                .map(|id| consume_app_fs_op(runtime, id))
                .unwrap_or(false);
        let source = match (kind, is_app_op) {
            (Some(_), true) => crate::notify::NotifySource::App,
            (Some(k), false) if k.is_blocking() => crate::notify::NotifySource::Explorer,
            // 完成型通知由平台侧发出，无法区分具体发起者；S1 只观测。
            (Some(_), false) => crate::notify::NotifySource::System,
            (None, _) => crate::notify::NotifySource::Unknown,
        };
        // A：打开者身份（打开/关闭通知）——落库/落日志都在 worker 线程，回调线程只投递。
        if matches!(
            task.input.kind,
            Some(
                crate::notify::NotifyKind::OpenCompleted
                    | crate::notify::NotifyKind::CloseCompleted,
            )
        ) {
            if let Some(kind) = task.input.kind {
                log_opener_identity(task.fs_id, kind, task.identity.as_ref());
            }
        }
        let scope = crate::notify::NotifyScope {
            root: &root_text,
            fs_id: task.fs_id,
            source,
            detected_at: &now_iso(),
        };
        let event = match crate::notify::classify(&task.input, &scope) {
            crate::notify::NotifyDecision::Event(event) => *event,
            crate::notify::NotifyDecision::Ignore(reason) => {
                log::debug!("native-sync notify ignored: {reason}");
                return;
            }
            crate::notify::NotifyDecision::OutOfScope => {
                log::debug!("native-sync notify out of scope");
                return;
            }
        };
        // 诊断日志只记录 fs_id / kind / 来源，绝不记录文件名。
        log::info!(
            "native-sync notify kind={} phase={} source={} fs_id={:?} dir={} reason={:?}",
            event.kind,
            event.phase,
            event.source,
            event.fs_id,
            event.is_dir,
            event.reason
        );

        let db = runtime
            .state_db
            .read()
            .ok()
            .and_then(|v| v.clone())
            .and_then(|path| {
                path.parent()
                    .and_then(|p| p.parent())
                    .map(crate::db::IndexDb::open)
            })
            .and_then(|r| r.ok());
        if let Some(db) = db.as_ref() {
            // 应用自身操作只留痕，不产生「待同步」标记。
            let is_app = event.source == "app";
            let fs_id = event.fs_id.map(|v| v as i64);
            if !is_app {
                match event.kind.as_str() {
                    "delete" => {
                        if let Some(id) = fs_id {
                            let _ = db.set_pending_op(id, Some("delete"), Some(&event.detected_at));
                        } else if let Some(rel) = event.from_relative.as_deref() {
                            if let Some((id, _, _, _, _, _)) =
                                db.entry_by_relative(rel).ok().flatten()
                            {
                                let _ =
                                    db.set_pending_op(id, Some("delete"), Some(&event.detected_at));
                            }
                        }
                    }
                    "rename" => {
                        if let Some(id) = fs_id {
                            if let Some(to) = event.to_relative.as_deref() {
                                let _ = db.apply_local_rename(id, to);
                            } else {
                                // 移出同步根：从同步根视角等同「本地已删除」。
                                let _ =
                                    db.set_pending_op(id, Some("delete"), Some(&event.detected_at));
                            }
                        }
                    }
                    "dehydrate" if event.phase == "intent" => {
                        if let Some(id) = fs_id {
                            // 内容已不在本地：hydration 轴回到 online_only。
                            let _ = db.set_state_by_fs_id(id as u64, "online_only", None);
                        }
                    }
                    _ => {}
                }
            }
            // 只有"本地确实发生了变化、且是意图事件"才入账。
            // open/close/validate 属**观测**（日志里已有），不是变更；
            // delete/rename/dehydrate 的 completion 只是平台回执，变更在 intent 时已记账。
            // 否则「读一次文件」就会在 local_change 里留下记录（真实事故）。
            let is_change = matches!(
                event.kind.as_str(),
                "delete" | "rename" | "dehydrate" | "upload" | "create"
            );
            if is_change && event.phase == "intent" {
                let _ = db.record_local_change(&crate::db::LocalChangeRecord {
                    id: None,
                    fs_id,
                    kind: event.kind.clone(),
                    phase: event.phase.clone(),
                    from_relative: event.from_relative.clone(),
                    to_relative: event.to_relative.clone(),
                    is_dir: event.is_dir,
                    reason: event.reason.clone(),
                    source: event.source.clone(),
                    detected_at: event.detected_at.clone(),
                    applied_to_cloud: false,
                    error: None,
                });
            }
        }
        if let Ok(sink) = runtime.change_sink.read() {
            if let Some(sink) = sink.as_ref() {
                sink(event.clone());
            }
        }
        // 关闭完成：本地可能已被写入（编辑）或新建，做快照比对。
        if event.kind == "close" && event.phase == "completion" {
            if let Some(db) = db.as_ref() {
                detect_local_write(runtime, db, &event);
            }
        }
    }

    fn mtime_secs(meta: &std::fs::Metadata) -> Option<i64> {
        use std::time::UNIX_EPOCH;
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
    }

    /// 是否为云占位符（在线/脱水/已召回）。用来把「本地新建的真实文件」与
    /// 「本来就存在的占位符」区分开——只有前者才算本地新建。
    fn is_cloud_placeholder(meta: &std::fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
        const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
        meta.file_attributes()
            & (FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
    }

    fn root_path_for(runtime: &Arc<Runtime>, relative: &str) -> Option<std::path::PathBuf> {
        runtime
            .root
            .read()
            .ok()
            .and_then(|v| v.clone())
            .map(|root| root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)))
    }

    fn emit_change(runtime: &Arc<Runtime>, event: crate::model::NativeSyncChangeEvent) {
        if let Ok(sink) = runtime.change_sink.read() {
            if let Some(sink) = sink.as_ref() {
                sink(event);
            }
        }
    }

    /// 关闭完成通知是「本地可能已被写入」的唯一可靠信号：把磁盘上的 mtime/size
    /// 与占位符创建 / 上次水合时记录的快照比对，识别本地编辑与本地新建。
    fn detect_local_write(
        runtime: &Arc<Runtime>,
        db: &crate::db::IndexDb,
        event: &crate::model::NativeSyncChangeEvent,
    ) {
        let Some(relative) = event
            .from_relative
            .as_deref()
            .or(event.to_relative.as_deref())
        else {
            return;
        };
        let Some(local) = root_path_for(runtime, relative) else {
            return;
        };
        let Ok(meta) = std::fs::metadata(&local) else {
            return;
        };
        let size = meta.len();
        let mtime = mtime_secs(&meta);
        let (fs_id, kind) = match db.entry_by_relative(relative) {
            Ok(Some((fs_id, _, _, _, _, local_only))) => {
                // 已有待同步标记（本地新建/已识别的编辑）不重复记账。
                if local_only || db.pending_op_of(fs_id).ok().flatten().is_some() {
                    return;
                }
                // 没有可用基线（老库行 / 尚未记录快照）时**不判定为修改**，
                // 只把当前状态补记为基线。否则「读取」会被误记成「本地编辑」。
                let baseline = db
                    .snapshot_for(fs_id)
                    .ok()
                    .flatten()
                    .and_then(|snap| snap.size.map(|size| (snap.mtime, size)));
                let (prev_mtime, prev_size) = match baseline {
                    Some(baseline) => baseline,
                    None => {
                        let _ = db.set_snapshot(fs_id, mtime, size);
                        return;
                    }
                };
                // 纯读取不改变 mtime/size ⇒ 不产生待同步（放行）。
                let changed =
                    prev_size != size || matches!(prev_mtime, Some(prev) if Some(prev) != mtime);
                if !changed {
                    return;
                }
                // 本地编辑：云端仍是旧内容，标记待上传（S1 只记录，不上传）。
                if db
                    .set_pending_op(fs_id, Some("upload"), Some(&event.detected_at))
                    .is_err()
                {
                    return;
                }
                (Some(fs_id), "upload")
            }
            Ok(None) => {
                if is_cloud_placeholder(&meta) {
                    return;
                }
                let Ok(next_id) = db.next_local_fs_id() else {
                    return;
                };
                let name = local
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| relative.to_string());
                if db
                    .insert_local_only_entry(next_id, relative, &name, meta.is_dir(), size)
                    .is_err()
                {
                    return;
                }
                (None, "create")
            }
            Err(_) => return,
        };
        let change = crate::model::NativeSyncChangeEvent {
            kind: kind.to_string(),
            phase: "intent".to_string(),
            // 本地新建条目没有云端 fs_id，绝不下发负数内部 id。
            fs_id: fs_id.map(|v| v as u64),
            from_relative: Some(relative.to_string()),
            to_relative: None,
            is_dir: meta.is_dir(),
            reason: None,
            source: event.source.clone(),
            detected_at: event.detected_at.clone(),
            pending_cloud: true,
        };
        log::info!(
            "native-sync local change captured kind={kind} fs_id={:?} dir={} bytes={size}",
            change.fs_id,
            meta.is_dir()
        );
        let _ = db.record_local_change(&crate::db::LocalChangeRecord {
            id: None,
            fs_id,
            kind: change.kind.clone(),
            phase: change.phase.clone(),
            from_relative: change.from_relative.clone(),
            to_relative: None,
            is_dir: change.is_dir,
            reason: None,
            source: change.source.clone(),
            detected_at: change.detected_at.clone(),
            applied_to_cloud: false,
            error: None,
        });
        emit_change(runtime, change);
    }

    /// 启动通知消费者。`connect()` 成功与「重新打开开关」都会调用，因此必须可重复建立。
    ///
    /// 以**通道是否存在**作为权威判据：`notify_worker_started` 只用来挡住并发重复启动。
    /// 这样即使某条路径漏了复位（历史缺陷），也不会让捕获永久哑掉。
    fn start_notify_worker(runtime: &Arc<Runtime>) {
        if runtime.notify_worker_started.swap(true, Ordering::SeqCst) {
            if runtime
                .notify_tx
                .read()
                .map(|slot| slot.is_some())
                .unwrap_or(false)
            {
                return;
            }
            // 标记为真但通道缺失 => 陈旧标记，继续重建而不是直接返回。
            log::debug!("native-sync notify worker rebuilt after a stale lifecycle flag");
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<NotifyTask>(NOTIFY_QUEUE_DEPTH);
        if let Ok(mut slot) = runtime.notify_tx.write() {
            *slot = Some(tx);
        } else {
            runtime.notify_worker_started.store(false, Ordering::SeqCst);
            return;
        }
        let worker_runtime = Arc::clone(runtime);
        let spawned = thread::Builder::new()
            .name("pandock-native-sync-notify".into())
            .spawn(move || notify_worker_loop(worker_runtime, rx));
        if spawned.is_err() {
            if let Ok(mut slot) = runtime.notify_tx.write() {
                *slot = None;
            }
            runtime.notify_worker_started.store(false, Ordering::SeqCst);
            log::warn!("native-sync notify worker spawn failed");
        }
    }

    /// 停止投递并复位生命周期标记。
    ///
    /// 置空与复位必须在**同一临界区顺序内**完成：否则下一次 `connect()` 或
    /// 「重新打开开关」时 `start_notify_worker` 会因标记仍为 true 而提前返回，
    /// 既不建通道也不起线程——本地变更捕获会在「停用 → 启用」之后永久哑掉
    /// （直到重启应用），而调用方只会看到投递静默失败。
    fn stop_notify_worker(runtime: &Runtime) {
        if let Ok(mut slot) = runtime.notify_tx.write() {
            *slot = None;
            runtime.notify_worker_started.store(false, Ordering::SeqCst);
        }
    }

    // ===================== 阻塞型回调：先内联 ACK =====================
    unsafe fn ack_operation(
        info: &CF_CALLBACK_INFO,
        op_type: CF_OPERATION_TYPE,
        params: &mut CF_OPERATION_PARAMETERS,
    ) {
        let op = CF_OPERATION_INFO {
            StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
            Type: op_type,
            ConnectionKey: info.ConnectionKey,
            TransferKey: info.TransferKey,
            RequestKey: info.RequestKey,
            ..Default::default()
        };
        if let Err(error) = CfExecute(&op, params) {
            // **不许静默**：ACK 失败会让调用方（Explorer）一直等下去，必须有痕迹。
            log::warn!(
                "native-sync CfExecute(ack) failed hr=0x{:08X}",
                error.code().0 as u32
            );
        }
    }

    /// ACK 前不得有任何 I/O、加锁或日志落盘：Explorer 正阻塞等待这个响应。
    unsafe fn ack_delete(info: &CF_CALLBACK_INFO) {
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: operation_param_size::<CF_OPERATION_PARAMETERS_0_2>(),
            ..Default::default()
        };
        params.Anonymous.AckDelete = CF_OPERATION_PARAMETERS_0_2 {
            Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
            CompletionStatus: NTSTATUS(0),
        };
        ack_operation(info, CF_OPERATION_TYPE_ACK_DELETE, &mut params);
    }

    unsafe fn ack_rename(info: &CF_CALLBACK_INFO) {
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: operation_param_size::<CF_OPERATION_PARAMETERS_0_3>(),
            ..Default::default()
        };
        params.Anonymous.AckRename = CF_OPERATION_PARAMETERS_0_3 {
            Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
            CompletionStatus: NTSTATUS(0),
        };
        ack_operation(info, CF_OPERATION_TYPE_ACK_RENAME, &mut params);
    }

    unsafe fn ack_dehydrate(info: &CF_CALLBACK_INFO) {
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: operation_param_size::<CF_OPERATION_PARAMETERS_0_1>(),
            ..Default::default()
        };
        params.Anonymous.AckDehydrate = CF_OPERATION_PARAMETERS_0_1 {
            Flags: CF_OPERATION_ACK_DEHYDRATE_FLAG_NONE,
            CompletionStatus: NTSTATUS(0),
            // **不传身份**：ACK 里的 identity 应是提供程序自备的数据，而 info.FileIdentity
            // 是平台自己的缓冲区指针；ParamSize 也不含内联 blob。实测（D2）：回传该指针会让
            // 外部发起的 CfDehydratePlaceholder **永远不返回**（Explorer「释放空间」卡在半路）。
            FileIdentity: std::ptr::null(),
            FileIdentityLength: 0,
        };
        ack_operation(info, CF_OPERATION_TYPE_ACK_DEHYDRATE, &mut params);
    }

    /// VALIDATE_DATA：S1 只观测。成功 ACK 表示「数据可用」，不触发重新下载链路；
    /// 任何异常都在日志里留痕，S2 再决定是否改为要求重新水合。
    unsafe fn ack_validate_data(info: &CF_CALLBACK_INFO, offset: i64, length: i64) {
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: operation_param_size::<CF_OPERATION_PARAMETERS_0_0>(),
            ..Default::default()
        };
        params.Anonymous.AckData = CF_OPERATION_PARAMETERS_0_0 {
            Flags: CF_OPERATION_ACK_DATA_FLAG_NONE,
            CompletionStatus: NTSTATUS(0),
            Offset: offset,
            Length: length,
        };
        ack_operation(info, CF_OPERATION_TYPE_ACK_DATA, &mut params);
    }

    unsafe extern "system" fn on_notify_delete(
        info_ptr: *const CF_CALLBACK_INFO,
        params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        let flags = (&*params_ptr).Anonymous.Delete.Flags.0 as u32;
        // 阻塞型：先 ACK，再做别的。
        ack_delete(info);
        dispatch_notification(
            info,
            crate::notify::NotifyInput {
                kind: Some(crate::notify::NotifyKind::Delete),
                flags,
                normalized_path: normalized_text(info),
                ..Default::default()
            },
        );
    }

    unsafe extern "system" fn on_notify_delete_completion(
        info_ptr: *const CF_CALLBACK_INFO,
        params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        let flags = (&*params_ptr).Anonymous.DeleteCompletion.Flags.0 as u32;
        dispatch_notification(
            info,
            crate::notify::NotifyInput {
                kind: Some(crate::notify::NotifyKind::DeleteCompleted),
                flags,
                normalized_path: normalized_text(info),
                ..Default::default()
            },
        );
    }

    unsafe extern "system" fn on_notify_rename(
        info_ptr: *const CF_CALLBACK_INFO,
        params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        let params = &*params_ptr;
        let flags = params.Anonymous.Rename.Flags.0 as u32;
        // 先 ACK：Explorer 正在阻塞等待响应，TargetPath 的字符串转换（堆分配）
        // 必须放在 ACK 之后，把阻塞窗口压到最小。
        ack_rename(info);
        // 新名字只由 TargetPath 给出（不是 FileIdentity）。
        let target = params.Anonymous.Rename.TargetPath.to_string().ok();
        dispatch_notification(
            info,
            crate::notify::NotifyInput {
                kind: Some(crate::notify::NotifyKind::Rename),
                flags,
                target_path: target,
                normalized_path: normalized_text(info),
                ..Default::default()
            },
        );
    }

    unsafe extern "system" fn on_notify_rename_completion(
        info_ptr: *const CF_CALLBACK_INFO,
        params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        let params = &*params_ptr;
        let flags = params.Anonymous.RenameCompletion.Flags.0 as u32;
        let source = params
            .Anonymous
            .RenameCompletion
            .SourcePath
            .to_string()
            .ok();
        dispatch_notification(
            info,
            crate::notify::NotifyInput {
                kind: Some(crate::notify::NotifyKind::RenameCompleted),
                flags,
                source_path: source,
                normalized_path: normalized_text(info),
                ..Default::default()
            },
        );
    }

    unsafe extern "system" fn on_notify_dehydrate(
        info_ptr: *const CF_CALLBACK_INFO,
        params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        let params = &*params_ptr;
        let flags = params.Anonymous.Dehydrate.Flags.0 as u32;
        let reason = params.Anonymous.Dehydrate.Reason.0 as u32;
        ack_dehydrate(info);
        dispatch_notification(
            info,
            crate::notify::NotifyInput {
                kind: Some(crate::notify::NotifyKind::Dehydrate),
                flags,
                reason: Some(reason),
                normalized_path: normalized_text(info),
                ..Default::default()
            },
        );
    }

    unsafe extern "system" fn on_notify_dehydrate_completion(
        info_ptr: *const CF_CALLBACK_INFO,
        params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        let params = &*params_ptr;
        let flags = params.Anonymous.DehydrateCompletion.Flags.0 as u32;
        let reason = params.Anonymous.DehydrateCompletion.Reason.0 as u32;
        dispatch_notification(
            info,
            crate::notify::NotifyInput {
                kind: Some(crate::notify::NotifyKind::DehydrateCompleted),
                flags,
                reason: Some(reason),
                normalized_path: normalized_text(info),
                ..Default::default()
            },
        );
    }

    unsafe extern "system" fn on_notify_open_completion(
        info_ptr: *const CF_CALLBACK_INFO,
        params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        let flags = (&*params_ptr).Anonymous.OpenCompletion.Flags.0 as u32;
        dispatch_notification(
            info,
            crate::notify::NotifyInput {
                kind: Some(crate::notify::NotifyKind::OpenCompleted),
                flags,
                normalized_path: normalized_text(info),
                ..Default::default()
            },
        );
    }

    unsafe extern "system" fn on_notify_close_completion(
        info_ptr: *const CF_CALLBACK_INFO,
        params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        let flags = (&*params_ptr).Anonymous.CloseCompletion.Flags.0 as u32;
        dispatch_notification(
            info,
            crate::notify::NotifyInput {
                kind: Some(crate::notify::NotifyKind::CloseCompleted),
                flags,
                normalized_path: normalized_text(info),
                ..Default::default()
            },
        );
    }

    unsafe extern "system" fn on_validate_data(
        info_ptr: *const CF_CALLBACK_INFO,
        params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        let info = &*info_ptr;
        let validate = (&*params_ptr).Anonymous.ValidateData;
        // S1：只观测 + 安全 ACK，不改动用户数据、不触发重新下载。
        log::warn!(
            "native-sync validate data observed offset={} length={} request_key={}",
            validate.RequiredFileOffset,
            validate.RequiredLength,
            info.RequestKey
        );
        ack_validate_data(info, validate.RequiredFileOffset, validate.RequiredLength);
        // 只观测：投递一条事件供诊断，worker 对 validate 不写任何待同步状态。
        dispatch_notification(
            info,
            crate::notify::NotifyInput {
                kind: Some(crate::notify::NotifyKind::ValidateData),
                flags: validate.Flags.0 as u32,
                normalized_path: normalized_text(info),
                ..Default::default()
            },
        );
    }

    unsafe extern "system" fn on_cancel_fetch_placeholders(
        info_ptr: *const CF_CALLBACK_INFO,
        _params_ptr: *const CF_CALLBACK_PARAMETERS,
    ) {
        // S1 不启用按需填充（FETCH_PLACEHOLDERS 恒返回空），这里只观察。
        let info = &*info_ptr;
        log::debug!(
            "native-sync cancel fetch placeholders observed request_key={}",
            info.RequestKey
        );
        dispatch_notification(
            info,
            crate::notify::NotifyInput {
                kind: Some(crate::notify::NotifyKind::PlaceholdersCancelled),
                normalized_path: normalized_text(info),
                ..Default::default()
            },
        );
    }

    /// S1 注册 13 个回调（3 个只读召回 + 10 个本地变更捕获）+ 终止项 NONE。
    ///
    /// 与 ADR-108 的关系：ADR-108 记录的「只注册 FETCH_DATA / CANCEL_FETCH_DATA /
    /// FETCH_PLACEHOLDERS」是 1.2.x 的事实；本切片补上删除/改名/脱水/开闭完成，
    /// 使本地变更被实时捕获（仍然**不传播到云端**）。
    static CALLBACKS: [CF_CALLBACK_REGISTRATION; 14] = [
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(on_fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
            Callback: Some(on_cancel_fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
            Callback: Some(on_fetch_placeholders),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_CANCEL_FETCH_PLACEHOLDERS,
            Callback: Some(on_cancel_fetch_placeholders),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_VALIDATE_DATA,
            Callback: Some(on_validate_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_FILE_OPEN_COMPLETION,
            Callback: Some(on_notify_open_completion),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION,
            Callback: Some(on_notify_close_completion),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DEHYDRATE,
            Callback: Some(on_notify_dehydrate),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DEHYDRATE_COMPLETION,
            Callback: Some(on_notify_dehydrate_completion),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
            Callback: Some(on_notify_delete),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE_COMPLETION,
            Callback: Some(on_notify_delete_completion),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
            Callback: Some(on_notify_rename),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_RENAME_COMPLETION,
            Callback: Some(on_notify_rename_completion),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        },
    ];
    impl Drop for WindowsPlatform {
        fn drop(&mut self) {
            let _ = self.disconnect();
        }
    }
    impl PlatformSync for WindowsPlatform {
        fn set_hydration_backend(&mut self, backend: Arc<dyn CloudBackend>) {
            if let Ok(mut slot) = self.runtime.backend.write() {
                *slot = Some(backend);
            }
        }
        fn set_state_db(&mut self, path: &Path) {
            if let Ok(mut slot) = self.runtime.state_db.write() {
                *slot = Some(path.to_path_buf());
            }
        }
        fn set_event_sink(
            &mut self,
            sink: Option<Arc<dyn Fn(crate::model::NativeSyncEvent) + Send + Sync>>,
        ) {
            if let Ok(mut slot) = self.runtime.event_sink.write() {
                *slot = sink;
            }
        }
        fn set_change_sink(
            &mut self,
            sink: Option<Arc<dyn Fn(crate::model::NativeSyncChangeEvent) + Send + Sync>>,
        ) {
            if let Ok(mut slot) = self.runtime.change_sink.write() {
                *slot = sink;
            }
        }
        fn set_notifications_enabled(&mut self, enabled: bool) {
            self.runtime
                .notifications_enabled
                .store(enabled, Ordering::Relaxed);
            if enabled {
                start_notify_worker(&self.runtime);
            } else {
                stop_notify_worker(&self.runtime);
            }
        }
        fn set_remote_mapping(&mut self, relative: &str, remote: RemotePath, fs_id: u64) {
            let key = relative.to_ascii_lowercase();
            if let Ok(mut map) = self.runtime.mappings.write() {
                map.insert(key.clone(), (remote.clone(), fs_id));
            }
            if let Ok(mut by_id) = self.runtime.by_id.write() {
                by_id.insert(fs_id, (remote, key));
            }
        }
        fn supported(&self) -> Result<(), String> {
            Ok(())
        }
        fn register(&mut self, root: &Path) -> Result<(), PlatformError> {
            // 只准备目录与记录根路径；外壳注册放在 connect() 里，与引擎连接同生命周期。
            std::fs::create_dir_all(root).map_err(|e| PlatformError::Path(e.to_string()))?;
            if let Ok(mut slot) = self.runtime.root.write() {
                *slot = Some(root.to_path_buf());
            }
            self.root = Some(root.to_path_buf());
            Ok(())
        }
        fn connect(&mut self) -> Result<(), PlatformError> {
            if self.connection.is_some() {
                return Ok(());
            }
            let root = self
                .root
                .clone()
                .ok_or_else(|| PlatformError::Path("未注册同步根".into()))?;
            // 外壳层注册：导航栏节点、水合状态角标、状态列都由它带来。
            // 依据微软 CloudMirror 官方示例，正确顺序是「外壳注册 → CfConnectSyncRoot」；
            // 不使用 CfRegisterSyncRoot（旧的低层路径，混用会报 0x8007018B）。
            // 之所以放在 connect 而非 register：外壳注册一旦存在，引擎未连接时资源管理器
            // 访问该目录会直接报「云文件提供程序未运行」，因此两者必须同生命周期。
            register_with_shell(&root)?;
            let rw = wide(&root.to_string_lossy());
            retain_process_context(&self.runtime)?;
            let connected = unsafe {
                CfConnectSyncRoot(
                    PCWSTR(rw.as_ptr()),
                    CALLBACKS.as_ptr(),
                    Some(Arc::as_ptr(&self.runtime) as *const c_void),
                    CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
                )
            };
            let k = match connected {
                Ok(k) => k,
                Err(e) => {
                    // **不再用注销做回滚**（ADR-110）：注销会遍历整棵树，
                    // 已水合的占位符转回普通文件、未水合的**从本地永久删除**。
                    // 保留注册 → 该目录在未连接期间报「云文件提供程序未运行」，但树与占位符都活着，
                    // 可被下一次成功 connect 后的自愈恢复。注销是不可逆的破坏，严格更差。
                    match sync_root_removal_action(SyncRootRemovalCause::ConnectFailure) {
                        SyncRootRemovalAction::KeepRegistration => log::warn!(
                            "native-sync CfConnectSyncRoot failed (hr=0x{:08X}) for {}: \
                             keeping the shell registration (NOT unregistering — that would \
                             revert hydrated placeholders to normal files and permanently delete \
                             un-hydrated ones)",
                            e.code().0 as u32,
                            root.display()
                        ),
                        SyncRootRemovalAction::Unregister => {
                            if let Err(rollback) = unregister_from_shell() {
                                log::warn!("native-sync shell rollback failed: {rollback}");
                            }
                        }
                    }
                    return Err(PlatformError::Api(e.to_string()));
                }
            };
            self.connection = Some(k);
            if self.runtime.notifications_enabled.load(Ordering::Relaxed) {
                start_notify_worker(&self.runtime);
            }
            self.runtime.start_accepting();
            Ok(())
        }
        fn disconnect(&mut self) -> Result<(), PlatformError> {
            // Stop new workers and drain existing ones before disconnecting CFAPI.
            // self.runtime remains owned by WindowsPlatform until drop, so the
            // raw CallbackContext pointer stays valid throughout teardown.
            // 先停投递与接收，再等 worker 排空，最后才断开 CFAPI。
            stop_notify_worker(&self.runtime);
            self.runtime.stop_accepting_and_wait();
            if let Some(k) = self.connection {
                unsafe {
                    CfDisconnectSyncRoot(k).map_err(|e| {
                        log::warn!(
                            "native-sync CfDisconnectSyncRoot failed hr=0x{:08X}",
                            e.code().0 as u32
                        );
                        PlatformError::Api(e.to_string())
                    })?;
                }
                self.connection = None;
            }
            // **故意不注销外壳注册**（ADR-110）：
            // 注销（`StorageProviderSyncRootManager.Unregister` / `CfUnregisterSyncRoot`）会
            // **遍历整棵树**——已水合的占位符被转回普通文件、**未水合的占位符被从本地永久删除**。
            // 干净退出时注销，等于用户每关一次应用就把整棵同步树脱云（真实事故）。
            // 保持注册与 ADR-108 的明文意图一致（"应用退出后同步根保留、与 OneDrive 一致"），
            // 代价是应用关闭期间该目录报「云文件提供程序未运行」（ADR-110 已记录）。
            Ok(())
        }
        fn unregister(&mut self) -> Result<(), PlatformError> {
            // 只有**刻意的移除**（卸载 / 从资源管理器移除同步根）才允许注销，
            // 且调用方必须告知用户风险：未水合（仅在线）的占位符会被从本地永久删除。
            self.disconnect()?;
            if sync_root_removal_action(SyncRootRemovalCause::Deliberate)
                == SyncRootRemovalAction::Unregister
            {
                unregister_from_shell()?;
            }
            self.root.take();
            Ok(())
        }
        fn on_disk_data_size(&self, path: &Path) -> Option<i64> {
            let handle = file_handle(path, false).ok()?;
            let mut buf = vec![0u8; std::mem::size_of::<CF_PLACEHOLDER_STANDARD_INFO>() + 64];
            let mut returned = 0u32;
            let ok = unsafe {
                CfGetPlaceholderInfo(
                    handle,
                    CF_PLACEHOLDER_INFO_STANDARD,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as u32,
                    Some(&mut returned),
                )
            }
            .is_ok();
            unsafe {
                let _ = CloseHandle(handle);
            }
            if ok {
                Some(unsafe {
                    (*(buf.as_ptr() as *const CF_PLACEHOLDER_STANDARD_INFO)).OnDiskDataSize
                })
            } else {
                None
            }
        }

        fn set_run_id(&mut self, run_id: u64) {
            self.run_id
                .store(run_id, std::sync::atomic::Ordering::Relaxed);
        }
        fn take_probe_summary(&mut self) -> (usize, usize, usize, usize) {
            self.probe_stats
                .lock()
                .map(|mut s| {
                    let out = *s;
                    *s = (0, 0, 0, 0);
                    out
                })
                .unwrap_or((0, 0, 0, 0))
        }
        fn repair_in_sync_if_needed(
            &mut self,
            path: &Path,
            fs_id: u64,
            is_dir: bool,
            has_pending_change: bool,
            snapshot_matches: bool,
        ) {
            let attrs = std::fs::metadata(path)
                .map(|m| {
                    use std::os::windows::fs::MetadataExt;
                    m.file_attributes()
                })
                .unwrap_or(0);
            let input = InSyncRepairInput {
                is_placeholder: placeholder_probe(path) == PlaceholderProbe::Placeholder,
                has_pending_change,
                snapshot_matches,
                in_sync: probe_in_sync_state(path),
            };
            match in_sync_repair_action(input) {
                InSyncRepair::Repair => {
                    let Ok(handle) = file_handle(path, true) else {
                        return;
                    };
                    let mut usn = 0i64;
                    let r = unsafe {
                        CfSetInSyncState(
                            handle,
                            CF_IN_SYNC_STATE_IN_SYNC,
                            CF_SET_IN_SYNC_FLAG_NONE,
                            Some(&mut usn),
                        )
                    };
                    unsafe {
                        let _ = CloseHandle(handle);
                    }
                    match r {
                        Ok(()) => log::info!(
                            "native-sync in-sync repaired fs_id={fs_id} kind={}",
                            if is_dir { "dir" } else { "file" }
                        ),
                        Err(error) => log::warn!(
                            "native-sync in-sync repair failed fs_id={fs_id} hr=0x{:08X}",
                            error.code().0 as u32
                        ),
                    }
                }
                InSyncRepair::LeaveAlone => log::debug!(
                    "native-sync in-sync ok fs_id={fs_id} kind={}",
                    if is_dir { "dir" } else { "file" }
                ),
                // "看到了未同步却没动手"必须**可见**（info）；其余情形（读不到状态 / 不是占位符）保持 debug。
                InSyncRepair::Skip => {
                    if input.in_sync == Some(false) {
                        log::info!(
                            "native-sync in-sync repair skipped fs_id={fs_id} kind={} pending={has_pending_change} snapshot_ok={snapshot_matches} attrs=0x{attrs:08X} cf_placeholder={}",
                            if is_dir { "dir" } else { "file" },
                            input.is_placeholder
                        );
                    } else {
                        log::debug!(
                            "native-sync in-sync repair skipped fs_id={fs_id} (probe unavailable or not a placeholder)"
                        );
                    }
                }
            }
        }
        fn create_placeholder(
            &mut self,
            path: &Path,
            is_dir: bool,
            size: u64,
            identity: &[u8],
        ) -> Result<(), PlatformError> {
            if path.exists() {
                // 已存在：按属性区分两种情形（决策函数见 `placeholder_repair_action`，
                // 纯函数、有单测）。
                let attrs = std::fs::metadata(path)
                    .map(|m| {
                        use std::os::windows::fs::MetadataExt;
                        m.file_attributes()
                    })
                    .unwrap_or(0);
                let probe = placeholder_probe(path);
                let cf_placeholder = probe == PlaceholderProbe::Placeholder;
                log::debug!(
                    "native-sync existing-path probe attrs=0x{attrs:08X} attrs_hint={} cf_placeholder={cf_placeholder}",
                    is_placeholder_attrs(attrs)
                );
                if let Ok(mut stats) = self.probe_stats.lock() {
                    stats.0 += 1;
                    if cf_placeholder {
                        stats.1 += 1;
                    }
                }
                if probe == PlaceholderProbe::Unknown {
                    // 「不确定」必须保守：不转换、也不标记，避免把健康占位符改坏。
                    log::debug!(
                        "native-sync existing path probe inconclusive; leaving path untouched"
                    );
                    return Ok(());
                }
                match placeholder_repair_action(cf_placeholder) {
                    // 本来就是占位符：确保被标记为「与云端已同步」，
                    // 否则 Explorer 状态列会一直显示「同步挂起」。
                    PlaceholderRepair::KeepInSync => {
                        if let Err(error) = mark_path_in_sync(path) {
                            log::debug!(
                                "native-sync mark in-sync skipped for existing path: {} error={error}",
                                path.display()
                            );
                        }
                        probe_in_sync(path, if is_dir { "dir" } else { "file" });
                    }
                    // 已存在但**不是占位符**（例如曾被注销转成普通文件）：就地转回占位符。
                    // 这是唯一非破坏性的自愈手段——保留内容与 PINNED 位。
                    PlaceholderRepair::ConvertToPlaceholder => {
                        // **止血（2026-10-12）**：自动转换当前被**闸门**关闭。
                        // 缘由：探测结论尚未与 CF 层真值逐条对齐——曾因判定传递错误，
                        // 对**健康的占位符**反复发起 CfConvertToPlaceholder 并全部失败（0x8007017C）。
                        // 恢复条件：用诊断模式证明"探测结论 == CF 真值"后，才允许重新启用
                        // （届时单独提交 + 失败测试）。
                        log::warn!(
                            "native-sync would convert existing path back to a placeholder, but auto-conversion is gated pending probe validation run={} (dir={is_dir}) attrs=0x{attrs:08X}",
                            self.run_id.load(std::sync::atomic::Ordering::Relaxed)
                        );
                    }
                }
                return Ok(());
            }
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).map_err(|e| PlatformError::Path(e.to_string()))?;
            }
            if is_dir {
                // 目录无文件数据，沿用真实目录 + 转换，保持 MARK_IN_SYNC。
                std::fs::create_dir(path).map_err(|e| PlatformError::Path(e.to_string()))?;
                let h = file_handle(path, true)?;
                let mut usn = 0;
                let r = unsafe {
                    CfConvertToPlaceholder(
                        h,
                        Some(identity.as_ptr() as *const c_void),
                        identity.len() as u32,
                        CF_CONVERT_FLAG_MARK_IN_SYNC,
                        Some(&mut usn),
                        None,
                    )
                };
                unsafe {
                    let _ = CloseHandle(h);
                }
                r.map_err(|e| {
                    log::warn!(
                        "native-sync dir placeholder convert failed: {} error={e}",
                        path.display()
                    );
                    PlatformError::Api(e.to_string())
                })?;
                probe_in_sync(path, "dir");
                return Ok(());
            }
            // P0-1: 在线文件占位符必须无磁盘数据。MARK_IN_SYNC + set_len 的满尺寸文件
            // 会被 Explorer 视为内容已在本地，双击不触发 FETCH_DATA，消费者读到全零。
            // 必须用 CfCreatePlaceholders 通过 CF_FS_METADATA 声明 FileSize。
            create_dataless_file_placeholder(path, size, identity)?;
            probe_in_sync(path, "file");
            Ok(())
        }
        fn pin(&mut self, path: &Path, pinned: bool) -> Result<(), PlatformError> {
            // **写操作**（CfSetPinState）必须用可写属性的句柄：此前传 false（只读属性）
            // 是"管理台点击固定没有反应"的高度可疑根因；文档只保证"句柄可为仅属性/无访问 +
            // 调用者具备 READ_DATA 或 WRITE_DAC"，两种读法必须实测判定（见材料里的权限验证表）。
            let h = file_handle(path, true)?;
            let state = if pinned {
                CF_PIN_STATE_PINNED
            } else {
                CF_PIN_STATE_UNPINNED
            };
            let r = unsafe { CfSetPinState(h, state, CF_SET_PIN_FLAG_NONE, None) };
            unsafe {
                let _ = CloseHandle(h);
            }
            r.map_err(|e| {
                log::warn!(
                    "native-sync CfSetPinState failed hr=0x{:08X}",
                    e.code().0 as u32
                );
                PlatformError::Api(e.to_string())
            })
        }
        fn dehydrate(&mut self, path: &Path) -> Result<(), PlatformError> {
            // 应用自身发起的脱水会产生 NOTIFY_DEHYDRATE「自杀回调」，先立守卫。
            note_app_path_op(&self.runtime, path);
            let h = file_handle(path, true)?;
            // 脱水前读 pin：显式 UNPINNED = "请脱水"，脱水完成后必须归一回 UNSPECIFIED，
            // 否则平台会永远认为该占位符"待办未完成"（图标挂起、「释放空间」灰显）。
            let mut pin_before: u32 = 0;
            {
                let mut buf = vec![0u8; std::mem::size_of::<CF_PLACEHOLDER_BASIC_INFO>() + 64];
                let mut returned = 0u32;
                if unsafe {
                    CfGetPlaceholderInfo(
                        h,
                        CF_PLACEHOLDER_INFO_BASIC,
                        buf.as_mut_ptr() as *mut c_void,
                        buf.len() as u32,
                        Some(&mut returned),
                    )
                }
                .is_ok()
                {
                    pin_before = unsafe {
                        (*(buf.as_ptr() as *const CF_PLACEHOLDER_BASIC_INFO))
                            .PinState
                            .0 as u32
                    };
                }
            }
            let r = unsafe { CfDehydratePlaceholder(h, 0, -1, CF_DEHYDRATE_FLAG_NONE, None) };

            // **顺序（事故后修正）**：脱水 → **先验真** `OnDiskDataSize == 0` → **再**归一 pin。
            // 若先归一、而文件其实又被水合回来，会出现「归一 → 又水合 → pin 又回 UNPINNED」的来回抖动；
            // 更严重的是「看起来成功、其实没释放」。所以验证不过 ⇒ 保留 pin + warn + **返回可感知失败**。
            if r.is_ok() {
                let mut std_buf =
                    vec![0u8; std::mem::size_of::<CF_PLACEHOLDER_STANDARD_INFO>() + 64];
                let mut std_ret = 0u32;
                let on_disk = if unsafe {
                    CfGetPlaceholderInfo(
                        h,
                        CF_PLACEHOLDER_INFO_STANDARD,
                        std_buf.as_mut_ptr() as *mut c_void,
                        std_buf.len() as u32,
                        Some(&mut std_ret),
                    )
                }
                .is_ok()
                {
                    Some(unsafe {
                        (*(std_buf.as_ptr() as *const CF_PLACEHOLDER_STANDARD_INFO)).OnDiskDataSize
                    })
                } else {
                    None
                };
                if on_disk != Some(0) {
                    log::warn!(
                        "native-sync dehydrate verification failed op=CfDehydratePlaceholder OnDiskDataSize={on_disk:?} (expected 0) -- pin NOT normalized"
                    );
                    unsafe {
                        let _ = CloseHandle(h);
                    }
                    return Err(PlatformError::Api(format!(
                        "脱水后本地数据未清空（OnDiskDataSize={on_disk:?}）；已保留 pin 状态，未归一"
                    )));
                }
            }
            if r.is_ok()
                && dehydrate_post_action(pin_before) == DehydratePostAction::NormalizeToUnspecified
            {
                let set = unsafe {
                    CfSetPinState(h, CF_PIN_STATE_UNSPECIFIED, CF_SET_PIN_FLAG_NONE, None)
                };
                match set {
                    Ok(()) => log::info!(
                        "native-sync pin normalized to UNSPECIFIED after dehydration (was UNPINNED)"
                    ),
                    Err(e) => log::warn!(
                        "native-sync CfSetPinState(UNSPECIFIED) after dehydrate failed hr=0x{:08X}",
                        e.code().0 as u32
                    ),
                }
            }
            unsafe {
                let _ = CloseHandle(h);
            }
            r.map_err(|e| {
                log::warn!(
                    "native-sync CfDehydratePlaceholder failed hr=0x{:08X}",
                    e.code().0 as u32
                );
                PlatformError::Api(e.to_string())
            })
        }
        fn pin_state(&self, path: &Path) -> Option<bool> {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_PINNED: u32 = 0x0008_0000;
            const FILE_ATTRIBUTE_UNPINNED: u32 = 0x0010_0000;
            let attrs = std::fs::metadata(path).ok()?.file_attributes();
            if attrs & FILE_ATTRIBUTE_PINNED != 0 {
                Some(true)
            } else if attrs & FILE_ATTRIBUTE_UNPINNED != 0 {
                Some(false)
            } else {
                None
            }
        }
        fn mark_in_sync(&mut self, path: &Path) -> Result<(), PlatformError> {
            let h = file_handle(path, true)?;
            let mut usn = 0;
            let r = unsafe {
                CfSetInSyncState(
                    h,
                    CF_IN_SYNC_STATE_IN_SYNC,
                    CF_SET_IN_SYNC_FLAG_NONE,
                    Some(&mut usn),
                )
            };
            unsafe {
                let _ = CloseHandle(h);
            }
            r.map_err(|e| PlatformError::Api(e.to_string()))
        }
        fn remove_placeholder(&mut self, path: &Path, is_dir: bool) -> Result<(), PlatformError> {
            // 远端删除对账时应用会删本地占位符，随之而来的 NOTIFY_DELETE 不是用户操作。
            note_app_path_op(&self.runtime, path);
            let result = if is_dir {
                std::fs::remove_dir(path)
            } else {
                std::fs::remove_file(path)
            };
            result.map_err(|e| PlatformError::Api(e.to_string()))
        }
    }
}
#[cfg(windows)]
use windows_impl::WindowsPlatform;

#[cfg(test)]
mod placeholder_repair_tests {
    use super::*;

    /// 已是占位符 → 维持 in-sync（**不**走 convert 分支）。
    #[test]
    fn existing_placeholders_keep_in_sync_and_are_not_converted() {
        // 判定输入现在是 **CF 原生结论**（三态里的 Placeholder），不再是通用属性位。
        assert_eq!(
            placeholder_repair_action(true),
            PlaceholderRepair::KeepInSync,
            "CF 判定为占位符时不得再转换"
        );
    }

    /// **唯一**允许注销同步根的起因是"刻意移除"；连接失败必须保留注册。
    /// （注销会遍历整棵树：已水合转普通文件、未水合的从本地永久删除。）
    #[test]
    fn only_deliberate_removal_is_allowed_to_unregister_the_sync_root() {
        assert_eq!(
            sync_root_removal_action(SyncRootRemovalCause::ConnectFailure),
            SyncRootRemovalAction::KeepRegistration,
            "连接失败绝不能注销同步根"
        );
        assert_eq!(
            sync_root_removal_action(SyncRootRemovalCause::Deliberate),
            SyncRootRemovalAction::Unregister
        );
        // 穷举所有起因：除 Deliberate 外都不得注销。
        let unregister_causes: Vec<_> = [
            SyncRootRemovalCause::Deliberate,
            SyncRootRemovalCause::ConnectFailure,
        ]
        .into_iter()
        .filter(|c| sync_root_removal_action(*c) == SyncRootRemovalAction::Unregister)
        .collect();
        assert_eq!(
            unregister_causes,
            vec![SyncRootRemovalCause::Deliberate],
            "唯一会注销的起因只能是刻意移除"
        );
    }

    /// in-sync 自愈：三条前置条件缺一不可；已是 in-sync 不调 set；读不到状态不动手。
    #[test]
    fn in_sync_repair_requires_all_preconditions() {
        let ok = InSyncRepairInput {
            is_placeholder: true,
            has_pending_change: false,
            snapshot_matches: true,
            in_sync: Some(false),
        };
        assert_eq!(in_sync_repair_action(ok), InSyncRepair::Repair);
        // 已是 in-sync → 不调 set
        assert_eq!(
            in_sync_repair_action(InSyncRepairInput {
                in_sync: Some(true),
                ..ok
            }),
            InSyncRepair::LeaveAlone
        );
        // 读不到状态 → 不动手
        assert_eq!(
            in_sync_repair_action(InSyncRepairInput {
                in_sync: None,
                ..ok
            }),
            InSyncRepair::Skip
        );
        // 负例三条：不是占位符 / 有本地改动 / 快照不一致 —— 都不得修复
        for bad in [
            InSyncRepairInput {
                is_placeholder: false,
                ..ok
            },
            InSyncRepairInput {
                has_pending_change: true,
                ..ok
            },
            InSyncRepairInput {
                snapshot_matches: false,
                ..ok
            },
        ] {
            assert_eq!(
                in_sync_repair_action(bad),
                InSyncRepair::Skip,
                "内容可能被本地改过时绝不能置为已同步: {bad:?}"
            );
        }
    }

    /// 三态映射：只有"确定不是"才允许转换；其余错误一律"不确定"（保守不转换）。
    #[test]
    fn placeholder_probe_classification_is_conservative() {
        assert_eq!(
            classify_placeholder_probe(Ok(())),
            PlaceholderProbe::Placeholder
        );
        // 确定不是占位符的两类
        for hr in [0x8007_1126u32, 0x8007_0186] {
            assert_eq!(
                classify_placeholder_probe(Err(hr)),
                PlaceholderProbe::NotPlaceholder,
                "hr=0x{hr:08X} 应判为确定不是占位符"
            );
        }
        // 其余错误（含事故里的 INVALID_REQUEST）一律"不确定" ⇒ 不转换
        for hr in [
            0x8007_017C,
            0x8007_0005,
            0x8007_018B,
            0x8007_0164,
            0x8000_4005,
        ] {
            assert_eq!(
                classify_placeholder_probe(Err(hr)),
                PlaceholderProbe::Unknown,
                "hr=0x{hr:08X} 必须保守处理，不得当作\"不是占位符\""
            );
        }
    }

    /// 脱水后的 pin 归一：UNPINNED → 归一；PINNED / UNSPECIFIED → 不动。
    #[test]
    fn dehydrate_post_action_normalizes_only_explicit_unpinned() {
        assert_eq!(
            dehydrate_post_action(2),
            DehydratePostAction::NormalizeToUnspecified,
            "显式 UNPINNED 的语义就是'请脱水'，脱水完成后必须归一回 UNSPECIFIED"
        );
        for pin in [0u32, 1, 3, 4] {
            assert_eq!(
                dehydrate_post_action(pin),
                DehydratePostAction::LeaveAlone,
                "pin={pin} 不得被动（尤其用户的显式 PINNED）"
            );
        }
    }

    /// 事故回归：**通用属性位不能用来判断占位符**。
    /// 真机实测这三种值正是健康占位符的 `GetFileAttributes*` 返回（都不含 `0x400`）；
    /// 若据此判定，每次 `sync_now` 都会把健康占位符重新转换一遍。
    /// 权威判定是 CF 自身（`CfGetPlaceholderInfo` 成功 = 占位符）。
    #[test]
    fn generic_attribute_bits_are_not_a_reliable_placeholder_signal() {
        for attrs in [0x0010_0020u32, 0x0008_0020, 0x0000_0020] {
            assert!(
                !is_placeholder_attrs(attrs),
                "attrs=0x{attrs:X} 不含重解析点/召回位 —— 它其实是健康占位符（陷阱）"
            );
        }
    }

    /// 非占位符（事故现场实测的脱云属性）→ 走 convert 就地修复分支。
    #[test]
    fn existing_normal_files_are_converted_back_to_placeholders() {
        assert_eq!(
            placeholder_repair_action(false),
            PlaceholderRepair::ConvertToPlaceholder,
            "CF 判定不是占位符时才允许走 convert 修复"
        );
    }
}

#[cfg(all(test, not(windows)))]
mod stub_tests {
    use super::*;

    #[test]
    fn stub_platform_reports_unsupported_but_disconnects_cleanly() {
        let mut platform = new_platform();
        assert!(platform.supported().is_err());
        assert!(platform.register(Path::new("x")).is_err());
        assert!(platform.connect().is_err());
        assert!(platform
            .create_placeholder(Path::new("x"), false, 0, &[])
            .is_err());
        assert!(platform.pin(Path::new("x"), true).is_err());
        assert!(platform.dehydrate(Path::new("x")).is_err());
        assert!(platform.mark_in_sync(Path::new("x")).is_err());
        assert!(platform.remove_placeholder(Path::new("x"), false).is_err());
        assert!(platform.disconnect().is_ok());
        assert!(platform.unregister().is_ok());
    }
}
