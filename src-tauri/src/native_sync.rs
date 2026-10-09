use crate::{client_from_state, AppState};
use pandock_core::cloud::{BaiduCloudFs, CloudFs};
use pandock_nativesync::{
    NativeSyncAction, NativeSyncError, NativeSyncListView, NativeSyncManager, NativeSyncStatusView,
    SnapshotBackend,
};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

fn map_error(error: NativeSyncError) -> String {
    error.to_string()
}

/// P2: NativeSync 状态事件（payload 与 native_sync_status 返回同构）。
pub const NATIVE_SYNC_STATE_EVENT: &str = "pandock://nativesync/state";

fn emit_state(app: &AppHandle, status: &NativeSyncStatusView) {
    use tauri::Emitter;
    let _ = app.emit(NATIVE_SYNC_STATE_EVENT, status);
}

async fn ensure_manager(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut guard = state.native_sync.lock().await;
    ensure_manager_guard(app, &mut guard).await
}

/// 在已持有的锁内确保 manager 存在。sync_now 采用 take-replace 无锁网络阶段，
/// 期间其他命令可能看到 None，这里就地重建（SQLite/注册表均为权威状态）。
async fn ensure_manager_guard(
    app: &AppHandle,
    guard: &mut tokio::sync::MutexGuard<'_, Option<NativeSyncManager>>,
) -> Result<(), String> {
    if guard.is_none() {
        let data_dir = app
            .state::<AppState>()
            .data_dir
            .lock()
            .map_err(|_| "无法读取应用数据目录".to_string())?
            .clone();
        let mut manager = NativeSyncManager::new(&data_dir, None).map_err(map_error)?;
        attach_event_sink(app, &mut manager);
        **guard = Some(manager);
    }
    Ok(())
}

/// hydration job 完成/失败时经事件总线推送当前状态（payload 与 status 同构）。
/// 推送是非高频的（job 终态才触发）；状态锁被占时静默跳过，避免刷频与阻塞。
fn attach_event_sink(app: &AppHandle, manager: &mut NativeSyncManager) {
    let app_handle = app.clone();
    manager.set_event_sink(Some(std::sync::Arc::new(move |_event| {
        let app = app_handle.clone();
        tauri::async_runtime::spawn(async move {
            let state = app.state::<AppState>();
            let Ok(guard) = state.native_sync.try_lock() else {
                return;
            };
            let Some(manager) = guard.as_ref() else {
                return;
            };
            let Ok(status) = manager.status() else {
                return;
            };
            use tauri::Emitter;
            let _ = app.emit(NATIVE_SYNC_STATE_EVENT, &status);
        });
    })));
}
#[tauri::command]
pub async fn native_sync_status(app: AppHandle) -> Result<NativeSyncStatusView, String> {
    ensure_manager(&app).await?;
    let state = app.state::<AppState>();
    let mut guard = state.native_sync.lock().await;
    ensure_manager_guard(&app, &mut guard).await?;
    guard
        .as_ref()
        .expect("manager initialized")
        .status()
        .map_err(map_error)
}
#[tauri::command]
pub async fn native_sync_enable(
    app: AppHandle,
    root_path: Option<String>,
) -> Result<NativeSyncStatusView, String> {
    ensure_manager(&app).await?;
    let state = app.state::<AppState>();
    let status = {
        let mut guard = state.native_sync.lock().await;
        guard
            .as_mut()
            .expect("manager initialized")
            .enable(root_path.map(PathBuf::from))
            .map_err(map_error)?
    };
    emit_state(&app, &status);
    Ok(status)
}
#[tauri::command]
pub async fn native_sync_disable(
    app: AppHandle,
    unregister: Option<bool>,
) -> Result<NativeSyncStatusView, String> {
    ensure_manager(&app).await?;
    let state = app.state::<AppState>();
    let status = {
        let mut guard = state.native_sync.lock().await;
        guard
            .as_mut()
            .expect("manager initialized")
            .disable(unregister.unwrap_or(false))
            .map_err(map_error)?
    };
    emit_state(&app, &status);
    Ok(status)
}
#[tauri::command]
pub async fn native_sync_sync_now(app: AppHandle) -> Result<NativeSyncStatusView, String> {
    ensure_manager(&app).await?;
    let client = client_from_state(&app)?;
    let cloud: Arc<dyn CloudFs> = Arc::new(BaiduCloudFs::new(client));
    let backend = Arc::new(SnapshotBackend::new(cloud));
    let state = app.state::<AppState>();
    // P2: sync_now 的远端枚举是长网络操作，不得持 native_sync 锁跨 await：
    // 短暂持锁取出 manager/set_backend，网络阶段无锁，结束后放回并推送状态事件。
    let mut manager = {
        let mut guard = state.native_sync.lock().await;
        let mut manager = guard.take().expect("manager initialized");
        manager.set_backend(backend);
        manager
    };
    let result = manager.sync_now().await;
    let status = {
        let mut guard = state.native_sync.lock().await;
        *guard = Some(manager);
        result.map_err(map_error)?
    };
    emit_state(&app, &status);
    Ok(status)
}
#[tauri::command]
pub async fn native_sync_list(
    app: AppHandle,
    parent_path: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
) -> Result<NativeSyncListView, String> {
    ensure_manager(&app).await?;
    let state = app.state::<AppState>();
    let mut guard = state.native_sync.lock().await;
    ensure_manager_guard(&app, &mut guard).await?;
    guard
        .as_ref()
        .expect("manager initialized")
        .list(parent_path, cursor, limit.unwrap_or(100))
        .map_err(map_error)
}
#[tauri::command]
pub async fn native_sync_action(
    app: AppHandle,
    relative_path: String,
    action: pandock_nativesync::EntryAction,
) -> Result<NativeSyncStatusView, String> {
    ensure_manager(&app).await?;
    let state = app.state::<AppState>();
    let status = {
        let mut guard = state.native_sync.lock().await;
        guard
            .as_mut()
            .expect("manager initialized")
            .action(NativeSyncAction {
                relative_path,
                action,
            })
            .map_err(map_error)?
    };
    emit_state(&app, &status);
    Ok(status)
}
/// 同步资源浏览窗口的创建/聚焦逻辑；托盘菜单与 Tauri 命令共用。
pub fn open_manager_window(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("native-sync-manager") {
        window.unminimize().map_err(|e| e.to_string())?;
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }
    WebviewWindowBuilder::new(
        app,
        "native-sync-manager",
        WebviewUrl::App("index.html".into()),
    )
    .title("同步资源浏览")
    .inner_size(1100.0, 720.0)
    .resizable(true)
    .build()
    .map(|_| ())
    .map_err(|e| format!("创建同步资源浏览窗口失败: {e}"))
}

#[tauri::command]
pub async fn native_sync_open_manager(app: AppHandle) -> Result<(), String> {
    open_manager_window(&app)
}
#[tauri::command]
pub async fn native_sync_open_in_explorer(
    app: AppHandle,
    relative_path: String,
) -> Result<(), String> {
    ensure_manager(&app).await?;
    let state = app.state::<AppState>();
    let mut guard = state.native_sync.lock().await;
    ensure_manager_guard(&app, &mut guard).await?;
    let path = guard
        .as_ref()
        .expect("manager initialized")
        .open_path(&relative_path)
        .map_err(map_error)?;
    let target = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(&path).to_path_buf()
    };
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(target)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开资源管理器失败: {e}"))
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        Err("NativeSync 资源管理器打开仅支持 Windows".into())
    }
}
