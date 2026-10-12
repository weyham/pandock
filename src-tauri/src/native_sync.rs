use crate::{client_from_state, AppState};
use pandock_core::cloud::{BaiduCloudFs, CloudFs};
use pandock_nativesync::{
    NativeSyncAction, NativeSyncError, NativeSyncListView, NativeSyncManager, NativeSyncStatusView,
    SnapshotBackend,
};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// 命令边界的失败留痕：任何返回给 UI 的错误都在这里 warn 一次（命令名 + 错误内容，不含文件名）。
fn fail(command: &str, error: NativeSyncError) -> String {
    log::warn!("native-sync command failed cmd={command} error={error}");
    error.to_string()
}

/// P2: NativeSync 状态事件（payload 与 native_sync_status 返回同构）。
pub const NATIVE_SYNC_STATE_EVENT: &str = "pandock://nativesync/state";
/// S1: 本地变更事件，payload 为 NativeSyncChangeEvent。
pub const NATIVE_SYNC_CHANGE_EVENT: &str = "pandock://nativesync/change";

/// 状态重推的最小间隔：一次批量操作（例如删除一个目录）会连续产生多个变更，
/// 用 300ms last-wins 节流，避免把设置页刷爆。
const STATE_EMIT_THROTTLE_MS: u64 = 300;
static LAST_STATE_EMIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn emit_state(app: &AppHandle, status: &NativeSyncStatusView) {
    use tauri::Emitter;
    let _ = app.emit(NATIVE_SYNC_STATE_EVENT, status);
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 节流后的状态重推：状态变化点用；锁被占或未到间隔时静默跳过。
fn emit_state_throttled(app: &AppHandle) {
    use std::sync::atomic::Ordering;
    let now = now_ms();
    let last = LAST_STATE_EMIT_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < STATE_EMIT_THROTTLE_MS {
        return;
    }
    LAST_STATE_EMIT_MS.store(now, Ordering::Relaxed);
    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app_handle.state::<AppState>();
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
        let _ = app_handle.emit(NATIVE_SYNC_STATE_EVENT, &status);
    });
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
        log::warn!("native-sync manager was absent; recreated (state rebuilt from SQLite)");
        let data_dir = app
            .state::<AppState>()
            .data_dir
            .lock()
            .map_err(|_| "无法读取应用数据目录".to_string())?
            .clone();
        let mut manager =
            NativeSyncManager::new(&data_dir, None).map_err(|e| fail("unknown", e))?;
        // 配置文件里的紧急开关；环境变量优先级更高（在 manager 内部解析）。
        if let Ok(config) = app.state::<AppState>().config.lock() {
            manager.set_notifications_config(config.native_sync_notifications);
        }
        attach_event_sink(app, &mut manager);
        **guard = Some(manager);
    }
    Ok(())
}

/// hydration job 完成/失败时经事件总线推送当前状态（payload 与 status 同构）。
/// 推送是非高频的（job 终态才触发）；状态锁被占时静默跳过，避免刷频与阻塞。
fn attach_event_sink(app: &AppHandle, manager: &mut NativeSyncManager) {
    let app_handle = app.clone();
    let change_handle = app.clone();
    manager.set_change_sink(Some(std::sync::Arc::new(move |event| {
        use tauri::Emitter;
        // 变更事件立即推送（UI 按 source 过滤）；状态统计做节流重推。
        let _ = change_handle.emit(NATIVE_SYNC_CHANGE_EVENT, &event);
        emit_state_throttled(&change_handle);
    })));
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
        .map_err(|e| fail("native_sync_status", e))
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
            .map_err(|e| fail("native_sync_enable", e))?
    };
    emit_state(&app, &status);
    Ok(status)
}
/// 停用 NativeSync。
///
/// **默认（`unregister` 缺省或 `false`）只是断开引擎**，同步根注册保留——这不会改动
/// 本地文件，且关闭应用期间的不可访问属预期（ADR-110）。
///
/// `unregister = true` 是**破坏性**操作：注销同步根会让平台遍历整棵树，
/// **已水合的占位符被转回普通文件、未水合（仅在线）的占位符被从本地永久删除**
/// （云端数据仍在）。只应由"从资源管理器移除同步根 / 卸载"这类显式路径使用；
/// 设置页的"停止同步"不得传 `true`。
#[tauri::command]
pub async fn native_sync_disable(
    app: AppHandle,
    unregister: Option<bool>,
) -> Result<NativeSyncStatusView, String> {
    if unregister == Some(true) {
        log::warn!(
            "native-sync disable requested WITH unregister=true: this traverses the sync root and \
             reverts hydrated placeholders to normal files while permanently deleting un-hydrated ones"
        );
    }
    ensure_manager(&app).await?;
    let state = app.state::<AppState>();
    let status = {
        let mut guard = state.native_sync.lock().await;
        guard
            .as_mut()
            .expect("manager initialized")
            .disable(unregister.unwrap_or(false))
            .map_err(|e| fail("native_sync_disable", e))?
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
        result.map_err(|e| fail("native_sync_sync_now", e))?
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
        .map_err(|e| fail("native_sync_list", e))
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
            .map_err(|e| fail("native_sync_action", e))?
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
        .map_err(|e| fail("native_sync_open_in_explorer", e))?;
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
