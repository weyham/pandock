use crate::{client_from_state, AppState};
use pandock_core::cloud::{BaiduCloudFs, CloudFs};
use pandock_nativesync::{
    next_delay, NativeSyncAction, NativeSyncError, NativeSyncListView, NativeSyncManager,
    NativeSyncStatusView, SnapshotBackend,
};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Manager};

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
// ---------- ADR-111：自动对账调度器（无感同步 S1.5） ----------
//
// 纯运行循环；参数与退避/暂停判定在 pandock_nativesync::scheduler（定稿值，纯函数可测）。
// 纪律：网络调用不持 native_sync 锁（take-replace，与 sync_now 相同）；层终态才推状态事件；
// 任何时刻最多一层对账在跑（native_sync_engine 互斥）；手动对账经 auto_sync_cancel 抢占。

static AUTO_SYNC_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 启动自动对账调度器（幂等）。挂点：启动重连完成后 + native_sync_enable 成功后。
pub fn spawn_auto_sync(app: &AppHandle) {
    use std::sync::atomic::Ordering;
    if AUTO_SYNC_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    log::info!("native-sync auto-sync scheduler spawned");
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        auto_sync_loop(app).await;
    });
}

async fn sleep_or_wake(duration: std::time::Duration, wake: &tokio::sync::Notify) {
    tokio::select! {
        _ = tokio::time::sleep(duration) => {},
        _ = wake.notified() => {},
    }
}

/// 取当前状态并推送给 UI（层终态专用；锁被占或状态不可读时静默跳过）。
async fn emit_current_state(app: &AppHandle) {
    let state = app.state::<AppState>();
    let guard = state.native_sync.lock().await;
    let Some(manager) = guard.as_ref() else {
        return;
    };
    let Ok(status) = manager.status() else {
        return;
    };
    emit_state(app, &status);
}

async fn auto_sync_loop(app: AppHandle) {
    use std::sync::atomic::Ordering;
    let params = pandock_nativesync::AutoSyncParams::default();
    let wake = app.state::<AppState>().auto_sync_wake.clone();
    sleep_or_wake(params.first_delay, &wake).await;
    loop {
        // 开关：`native_sync_auto=false` ⇒ 纯手动（ADR-111 §2.5 回滚路径）。
        let auto_on = app
            .state::<AppState>()
            .config
            .lock()
            .map(|config| config.native_sync_auto)
            .unwrap_or(false);
        if !auto_on {
            sleep_or_wake(std::time::Duration::from_secs(60), &wake).await;
            continue;
        }
        // 门控：未启用 / 已暂停 ⇒ 低频等待。暂停的恢复信号由 db 驱动
        // （手动「立即对账」成功或授权恢复都会清 paused），无需额外通道。
        let gate = {
            let state = app.state::<AppState>();
            let guard = state.native_sync.lock().await;
            guard
                .as_ref()
                .and_then(|manager| manager.status().ok())
                .map(|status| (status.enabled, status.auto_sync_state))
        };
        match gate {
            Some((true, ref auto_state)) if auto_state != "paused" => {}
            _ => {
                sleep_or_wake(std::time::Duration::from_secs(60), &wake).await;
                continue;
            }
        }
        // 单飞：手动对账在跑 ⇒ 让位到下一层（不是失败）。
        let engine = app.state::<AppState>().native_sync_engine.clone();
        let Ok(engine_guard) = engine.try_lock() else {
            sleep_or_wake(std::time::Duration::from_secs(30), &wake).await;
            continue;
        };
        // backend：未授权算作一次失败（退避），但不骚扰（ADR-111 §2.3）。
        let backend = match client_from_state(&app) {
            Ok(client) => {
                let cloud: Arc<dyn CloudFs> = Arc::new(BaiduCloudFs::new(client));
                Some(Arc::new(SnapshotBackend::new(cloud))
                    as Arc<dyn pandock_nativesync::CloudBackend>)
            }
            Err(error) => {
                log::info!("native-sync auto layer skipped: no authorized client ({error})");
                None
            }
        };
        let mut manager = {
            let state = app.state::<AppState>();
            let mut guard = state.native_sync.lock().await;
            if let Err(error) = ensure_manager_guard(&app, &mut guard).await {
                log::warn!("native-sync auto layer aborted: {error}");
                drop(guard);
                drop(engine_guard);
                sleep_or_wake(std::time::Duration::from_secs(60), &wake).await;
                continue;
            }
            guard.take().expect("manager initialized")
        };
        let backend_missing = backend.is_none();
        let result = match backend {
            Some(backend) => {
                manager.set_backend(backend);
                let cancel = app.state::<AppState>().auto_sync_cancel.clone();
                cancel.store(false, Ordering::SeqCst);
                manager
                    .reconcile_incremental(&params, &cancel)
                    .await
                    .map(Some)
            }
            None => Err(NativeSyncError::Message("请先完成百度网盘授权".into())),
        };
        // 失败记账只许一次：reconcile 内部的层失败已自行记账；
        // 仅 backend 缺失（reconcile 未执行）才由这里代记。
        let fails_after = match &result {
            Ok(_) => None,
            Err(error) if backend_missing => Some(
                manager
                    .note_auto_failure(&error.to_string(), params.pause_threshold)
                    .unwrap_or(1),
            ),
            Err(_) => Some(
                manager
                    .status()
                    .map(|status| status.consecutive_failures as u32)
                    .unwrap_or(1),
            ),
        };
        let interval_override = manager.auto_interval_override_secs();
        {
            let state = app.state::<AppState>();
            let mut guard = state.native_sync.lock().await;
            *guard = Some(manager);
        }
        drop(engine_guard);
        match result {
            Ok(Some(report)) if report.preempted => {
                // 被手动「立即对账」抢占：不算失败，短等让手动先跑。
                emit_current_state(&app).await;
                sleep_or_wake(std::time::Duration::from_secs(5), &wake).await;
            }
            Ok(report) => {
                let _ = report; // 汇总日志已在 reconcile 内落盘。
                emit_current_state(&app).await;
                let interval = interval_override
                    .map(std::time::Duration::from_secs)
                    .unwrap_or(params.interval);
                sleep_or_wake(interval, &wake).await;
            }
            Err(error) => {
                let fails = fails_after.unwrap_or(1);
                let delay = next_delay(false, fails, &params);
                let backoff_until = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| (d.as_secs() + delay.as_secs()).to_string())
                    .unwrap_or_default();
                let state = app.state::<AppState>();
                let mut guard = state.native_sync.lock().await;
                if let Some(manager) = guard.as_mut() {
                    let _ = manager.set_backoff_until(Some(backoff_until));
                }
                drop(guard);
                log::info!(
                    "native-sync auto layer backoff delay={}s consecutive={fails} error={error}",
                    delay.as_secs()
                );
                emit_current_state(&app).await;
                sleep_or_wake(delay, &wake).await;
            }
        }
    }
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
    // 启用成功 ⇒ 调度器必须在场（不能只在启动重连一处挂）；唤醒立即重估。
    spawn_auto_sync(&app);
    app.state::<AppState>().auto_sync_wake.notify_one();
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
    // 抢占自动层（ADR-111 §2.4）：置取消标记，自动层在目录列举边界让位；
    // 引擎互斥锁保证任何时刻最多一层对账在跑。
    let queued_gen = state
        .native_sync_runs
        .load(std::sync::atomic::Ordering::Relaxed);
    state
        .auto_sync_cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let _engine_guard = state.native_sync_engine.lock().await;
    if state
        .native_sync_runs
        .load(std::sync::atomic::Ordering::Relaxed)
        != queued_gen
    {
        // 合并（ADR-111 §2.4）：排队期间已有一轮手动对账完成 ⇒ 不重跑，直接回当前状态。
        log::info!("native-sync sync_now coalesced into a just-completed manual run");
        let guard = state.native_sync.lock().await;
        let status = guard
            .as_ref()
            .expect("manager initialized")
            .status()
            .map_err(|e| fail("native_sync_sync_now", e))?;
        emit_state(&app, &status);
        return Ok(status);
    }
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
    state
        .native_sync_runs
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // 手动对账成功已清 paused：立即唤醒调度器重估，不必等满 60 秒低频轮询（PR-1 终审附议项）。
    state.auto_sync_wake.notify_one();
    emit_state(&app, &status);
    Ok(status)
}
#[tauri::command]
pub async fn native_sync_list(
    app: AppHandle,
    parent_path: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
    filter: Option<String>,
) -> Result<NativeSyncListView, String> {
    ensure_manager(&app).await?;
    let state = app.state::<AppState>();
    let mut guard = state.native_sync.lock().await;
    ensure_manager_guard(&app, &mut guard).await?;
    let manager = guard.as_ref().expect("manager initialized");
    // `filter` 是 additive 参数：`None`（旧调用）→ 原按父目录列举；`Some("actionable")` → 可操作视图；
    // 其它值显式报错（不能把笔误悄悄当成"无过滤"）。
    match filter.as_deref() {
        None => manager
            .list(parent_path, cursor, limit.unwrap_or(100))
            .map_err(|e| fail("native_sync_list", e)),
        Some("actionable") => manager
            .list_actionable(cursor, limit.unwrap_or(100))
            .map_err(|e| fail("native_sync_list", e)),
        Some(other) => {
            let msg = format!("native_sync_list: unknown filter '{other}'");
            log::warn!("{msg}");
            Err(msg)
        }
    }
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
