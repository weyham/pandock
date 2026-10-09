use crate::backend::CloudBackend;
use crate::hydration::HydrationError;
use pandock_core::cloud::RemotePath;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    fn mark_in_sync(&mut self, _: &Path) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("Windows Cloud Files API".into()))
    }
    fn remove_placeholder(&mut self, _: &Path, _: bool) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("Windows Cloud Files API".into()))
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex, OnceLock, RwLock};
    use std::thread;
    use windows::core::{GUID, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, NTSTATUS};
    use windows::Win32::Storage::CloudFilters::*;
    use windows::Win32::Storage::FileSystem::*;

    #[derive(Default)]
    pub struct WindowsPlatform {
        root: Option<PathBuf>,
        connection: Option<CF_CONNECTION_KEY>,
        runtime: Arc<Runtime>,
    }
    type EventSink = Arc<dyn Fn(crate::model::NativeSyncEvent) + Send + Sync>;

    #[derive(Default)]
    struct Runtime {
        backend: RwLock<Option<Arc<dyn CloudBackend>>>,
        root: RwLock<Option<PathBuf>>,
        mappings: RwLock<HashMap<String, (RemotePath, u64)>>,
        by_id: RwLock<HashMap<u64, (RemotePath, String)>>,
        state_db: RwLock<Option<PathBuf>>,
        event_sink: RwLock<Option<EventSink>>,
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
    fn identity_guid() -> GUID {
        GUID::from_u128(0x2f6f3a10_7c1b_4c6a_9a1d_55f1a2b3c4d5)
    }
    fn file_handle(path: &Path, write: bool) -> Result<HANDLE, PlatformError> {
        let full = path.to_string_lossy().replace('/', "\\");
        let w = wide(&full);
        let mut flags = FILE_ATTRIBUTE_NORMAL;
        if path.is_dir() {
            flags |= FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_BACKUP_SEMANTICS.0);
        }
        let access = if write {
            FILE_GENERIC_READ | FILE_GENERIC_WRITE
        } else {
            FILE_GENERIC_READ
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
        db.and_then(|db| db.mapping_by_fs_id(file_id).ok().flatten())
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

    fn path_tail(path: &str) -> String {
        let p = path.trim_start_matches("//?/").trim_start_matches('/');
        let p = if p.len() > 2 && p.as_bytes()[1] == b':' {
            &p[2..]
        } else {
            p
        };
        p.trim_start_matches('/').to_string()
    }

    fn relative_from_normalized(normalized: &str, root: &str) -> Option<String> {
        let root_tail = path_tail(root);
        if root_tail.is_empty() {
            return None;
        }
        path_tail(normalized)
            .strip_prefix(&root_tail)
            .map(|rest| rest.trim_start_matches('/').to_string())
            .filter(|rel| !rel.is_empty())
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
        r.map_err(|e| PlatformError::Api(format!("CfCreatePlaceholders 调用失败: {e}")))?;
        if infos[0].Result.is_err() {
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
            by_id.insert(file_id, (remote.clone(), relative));
        }
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
        // 注意：完整 hydration 后不得调用 CfSetInSyncState。真实机取证 + spike 对照
        // （POST-READ ATTR 证据）证明：对已充满数据的占位符设置 in-sync 会把占位符
        // 提升为普通文件（丢失 REPARSE，dehydrate/远端删除策略随之失效）。
        // 保持占位符原状即可：数据已在本地，后续读取由 cldflt 直接服务。
        match &result {
            Ok(hydrated) => log::info!(
                "native-sync fetch done fs_id={file_id} bytes={} full_file={}",
                hydrated.bytes,
                hydrated.full_file
            ),
            Err(error) => log::warn!("native-sync fetch failed fs_id={file_id} error={error}"),
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
            let record = |fs_id: u64, remote: &str, rel: &str| crate::model::EntryRecord {
                fs_id,
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
    static CALLBACKS: [CF_CALLBACK_REGISTRATION; 4] = [
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
            std::fs::create_dir_all(root).map_err(|e| PlatformError::Path(e.to_string()))?;
            let rw = wide(&root.to_string_lossy());
            let pn = wide("Pandock");
            let pv = wide(env!("CARGO_PKG_VERSION"));
            let ident = *b"CLOUDDOCK-S1-000";
            let reg = CF_SYNC_REGISTRATION {
                StructSize: std::mem::size_of::<CF_SYNC_REGISTRATION>() as u32,
                ProviderName: PCWSTR(pn.as_ptr()),
                ProviderVersion: PCWSTR(pv.as_ptr()),
                SyncRootIdentity: ident.as_ptr() as *const c_void,
                SyncRootIdentityLength: ident.len() as u32,
                FileIdentity: std::ptr::null(),
                FileIdentityLength: 0,
                ProviderId: identity_guid(),
            };
            let policies = CF_SYNC_POLICIES {
                StructSize: std::mem::size_of::<CF_SYNC_POLICIES>() as u32,
                Hydration: CF_HYDRATION_POLICY {
                    Primary: CF_HYDRATION_POLICY_FULL,
                    Modifier: CF_HYDRATION_POLICY_MODIFIER_NONE,
                },
                Population: CF_POPULATION_POLICY {
                    Primary: CF_POPULATION_POLICY_FULL,
                    Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
                },
                InSync: CF_INSYNC_POLICY_NONE,
                HardLink: CF_HARDLINK_POLICY_NONE,
                PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
            };
            unsafe {
                match CfRegisterSyncRoot(
                    PCWSTR(rw.as_ptr()),
                    &reg,
                    &policies,
                    CF_REGISTER_FLAG_NONE,
                ) {
                    Ok(()) => {}
                    // 0x800700B7 ERROR_ALREADY_EXISTS：重启后注册仍在，视为成功。
                    Err(e) if e.code().0 as u32 == 0x800700B7 => {
                        log::info!("native-sync sync root already registered");
                    }
                    Err(e) => return Err(PlatformError::Api(e.to_string())),
                }
            }
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
            let rw = wide(&root.to_string_lossy());
            retain_process_context(&self.runtime)?;
            let k = unsafe {
                CfConnectSyncRoot(
                    PCWSTR(rw.as_ptr()),
                    CALLBACKS.as_ptr(),
                    Some(Arc::as_ptr(&self.runtime) as *const c_void),
                    CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
                )
            }
            .map_err(|e| PlatformError::Api(e.to_string()))?;
            self.connection = Some(k);
            self.runtime.start_accepting();
            Ok(())
        }
        fn disconnect(&mut self) -> Result<(), PlatformError> {
            // Stop new workers and drain existing ones before disconnecting CFAPI.
            // self.runtime remains owned by WindowsPlatform until drop, so the
            // raw CallbackContext pointer stays valid throughout teardown.
            self.runtime.stop_accepting_and_wait();
            if let Some(k) = self.connection {
                unsafe {
                    CfDisconnectSyncRoot(k).map_err(|e| PlatformError::Api(e.to_string()))?;
                }
                self.connection = None;
            }
            Ok(())
        }
        fn unregister(&mut self) -> Result<(), PlatformError> {
            self.disconnect()?;
            if let Some(root) = self.root.take() {
                let rw = wide(&root.to_string_lossy());
                unsafe {
                    CfUnregisterSyncRoot(PCWSTR(rw.as_ptr()))
                        .map_err(|e| PlatformError::Api(e.to_string()))?;
                }
            }
            Ok(())
        }
        fn create_placeholder(
            &mut self,
            path: &Path,
            is_dir: bool,
            size: u64,
            identity: &[u8],
        ) -> Result<(), PlatformError> {
            if path.exists() {
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
                return r.map_err(|e| PlatformError::Api(e.to_string()));
            }
            // P0-1: 在线文件占位符必须无磁盘数据。MARK_IN_SYNC + set_len 的满尺寸文件
            // 会被 Explorer 视为内容已在本地，双击不触发 FETCH_DATA，消费者读到全零。
            // 必须用 CfCreatePlaceholders 通过 CF_FS_METADATA 声明 FileSize。
            create_dataless_file_placeholder(path, size, identity)
        }
        fn pin(&mut self, path: &Path, pinned: bool) -> Result<(), PlatformError> {
            let h = file_handle(path, false)?;
            let state = if pinned {
                CF_PIN_STATE_PINNED
            } else {
                CF_PIN_STATE_UNPINNED
            };
            let r = unsafe { CfSetPinState(h, state, CF_SET_PIN_FLAG_NONE, None) };
            unsafe {
                let _ = CloseHandle(h);
            }
            r.map_err(|e| PlatformError::Api(e.to_string()))
        }
        fn dehydrate(&mut self, path: &Path) -> Result<(), PlatformError> {
            let h = file_handle(path, true)?;
            let r = unsafe { CfDehydratePlaceholder(h, 0, -1, CF_DEHYDRATE_FLAG_NONE, None) };
            unsafe {
                let _ = CloseHandle(h);
            }
            r.map_err(|e| PlatformError::Api(e.to_string()))
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
