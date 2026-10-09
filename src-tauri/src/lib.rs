mod autostart;
mod native_sync;
mod update_runtime;

use keyring::Entry;
use pandock_core::update::install::{write_readiness_after_startup, LAUNCH_EXE_NAME};
use pandock_core::update::journal::{journal_path, JournalState, UpdateJournal};
use pandock_core::{
    auth::DeviceAuth,
    auth_flow::{
        AuthErrorView, AuthFlowCoordinator, AuthSession, AuthStateView, AuthorizationChallenge,
        CancelOutcome,
    },
    baidu::{BaiduClient, TokenResponse},
    config::Config,
    server::ServerHandle,
    status::ConnectionStatus,
};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{
    image::Image,
    menu::{CheckMenuItem, CheckMenuItemBuilder, MenuBuilder, MenuItem, MenuItemBuilder},
    tray::{TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, Wry,
};

const KEYRING_SERVICE: &str = "com.weyham.pandock";
/// 更名前（CloudDock 时代）的 keyring 服务名，仅用于一次性迁移读取（ADR-104 §4）。
const LEGACY_KEYRING_SERVICE: &str = "com.weyham.clouddock";
const TOKEN_KEY: &str = "refresh-token";
const SECRET_KEY: &str = "app-secret";
const WEBDAV_PASSWORD_KEY: &str = "webdav-password";
const LEGACY_RUNTIME_DIR: &str = "prod";

pub struct AppState {
    pub auth: Mutex<AuthFlowCoordinator>,
    pub update: update_runtime::UpdateRuntime,
    pub config: Mutex<Config>,
    pub status: Mutex<ConnectionStatus>,
    pub token: Mutex<Option<TokenResponse>>,
    pub server: Mutex<Option<ServerHandle>>,
    pub lifecycle: tokio::sync::Mutex<()>,
    pub data_dir: Mutex<PathBuf>,
    pub(crate) tray_icon_kind: Mutex<Option<TrayIconKind>>,
    pub status_item: Mutex<Option<MenuItem<Wry>>>,
    pub autostart_item: Mutex<Option<CheckMenuItem<Wry>>>,
    pub native_sync: tokio::sync::Mutex<Option<pandock_nativesync::NativeSyncManager>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthInput {
    app_key: String,
    app_name: String,
    app_secret: String,
}

fn keyring_entry_for(service: &str, name: &str) -> Result<Entry, String> {
    Entry::new(service, name).map_err(|e| e.to_string())
}
fn keyring_entry(name: &str) -> Result<Entry, String> {
    keyring_entry_for(KEYRING_SERVICE, name)
}
fn load_secret(name: &str) -> Option<String> {
    keyring_entry(name)
        .ok()
        .and_then(|entry| entry.get_password().ok())
}
fn save_secret(name: &str, value: &str) -> Result<(), String> {
    keyring_entry(name)?
        .set_password(value)
        .map_err(|e| e.to_string())
}
fn delete_secret(name: &str) {
    if let Ok(entry) = keyring_entry(name) {
        let _ = entry.delete_credential();
    }
}

/// ADR-104 §4：keyring 服务名迁移。读回退旧服务名；首次命中即复制到新服务名
/// 并删除旧条目。迁移窗口内崩溃由"下次启动再迁"兜底，凭据不会丢失。
fn migrate_legacy_keyring() {
    for name in [TOKEN_KEY, SECRET_KEY, WEBDAV_PASSWORD_KEY] {
        let Ok(new_entry) = keyring_entry(name) else {
            continue;
        };
        if new_entry.get_password().is_ok() {
            continue;
        }
        let Ok(legacy_entry) = keyring_entry_for(LEGACY_KEYRING_SERVICE, name) else {
            continue;
        };
        if let Ok(value) = legacy_entry.get_password() {
            if new_entry.set_password(&value).is_ok() {
                let _ = legacy_entry.delete_credential();
                log::info!(target: "pandock::migrate", "keyring '{name}' migrated to pandock service");
            }
        }
    }
}

/// ADR-104 §3.2：更名后首次运行清理同目录残留的 clouddock.exe。
/// 仅在自身为 pandock.exe 时触发；旧 exe 被占用时删除失败，下次启动重试。
#[cfg(target_os = "windows")]
fn cleanup_legacy_exe() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if exe.file_name().and_then(|n| n.to_str()) != Some("pandock.exe") {
        return;
    }
    let Some(dir) = exe.parent() else { return };
    let legacy = dir.join("clouddock.exe");
    if legacy.exists() {
        match std::fs::remove_file(&legacy) {
            Ok(()) => log::info!(target: "pandock::migrate", "removed legacy clouddock.exe"),
            Err(error) => {
                log::warn!(target: "pandock::migrate", "cannot remove legacy clouddock.exe: {error}")
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn cleanup_legacy_exe() {}

fn readiness_token_from_args() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--readiness-token" {
            return args.next();
        }
    }
    None
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn emit_auth_state(app: &AppHandle, state: &AuthFlowCoordinator) {
    let snapshot = state.view();
    let _ = app.emit("pandock://auth/state", snapshot);
}

#[cfg(target_os = "windows")]
fn migrate_legacy_runtime(_app: &AppHandle) -> Result<(), String> {
    let runtime_root = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or_else(|| "无法定位应用目录".to_string())?
        .to_path_buf();
    let data = runtime_root.join("data");
    let legacy = runtime_root
        .parent()
        .ok_or_else(|| "无法定位上级目录".to_string())?
        .join(LEGACY_RUNTIME_DIR)
        .join("data")
        .join("config.json");
    let current = data.join("config.json");
    pandock_core::config::migrate_config_file(&legacy, &current).map(|_| ())
}

#[cfg(not(target_os = "windows"))]
fn migrate_legacy_runtime(_app: &AppHandle) -> Result<(), String> {
    Ok(())
}

fn auth_failure(message: impl Into<String>) -> AuthErrorView {
    AuthErrorView::from_message(message)
}

fn data_dir(_app: &AppHandle) -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    let dir = pandock_core::data_dir::resolve()?;

    // macOS .app/Contents/MacOS is part of a signed, normally read-only bundle.
    // Runtime state must not be written next to the executable there.
    #[cfg(not(target_os = "windows"))]
    let dir = _app.path().app_data_dir().map_err(|e| e.to_string())?;

    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(data_dir(app)?.join("config.json"))
}

fn load_config(app: &AppHandle) -> Config {
    let path = config_path(app).ok();
    let mut config = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Config>(&s).ok())
        .unwrap_or_default();
    // Secrets stay in the OS credential store. They are never copied into app
    // state or serialized to the settings frontend.
    config.app_secret.clear();
    config.webdav_password.clear();
    config
}

fn save_config_file(app: &AppHandle, config: &Config) -> Result<(), String> {
    let path = config_path(app)?;
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    // Keep every overwritten user configuration recoverable. On Windows rename
    // does not replace an existing destination, so move the old file aside first.
    let backup = if path.exists() {
        let backup = path.with_file_name(format!(
            "config-{}.bak.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_millis()
        ));
        std::fs::rename(&path, &backup).map_err(|e| e.to_string())?;
        Some(backup)
    } else {
        None
    };
    if let Err(error) = std::fs::rename(&tmp, &path) {
        if let Some(backup) = backup {
            let _ = std::fs::rename(backup, &path);
        }
        return Err(error.to_string());
    }
    Ok(())
}

fn client_from_state(app: &AppHandle) -> Result<Arc<BaiduClient>, String> {
    let state = app.state::<AppState>();
    let config = state.config.lock().unwrap().clone();
    let token = state
        .token
        .lock()
        .unwrap()
        .as_ref()
        .map(|v| v.access_token.clone())
        .unwrap_or_default();
    let secret = load_secret(SECRET_KEY).unwrap_or_default();
    if config.app_key.is_empty() || config.app_name.is_empty() || secret.is_empty() {
        return Err("请先完成百度网盘连接".into());
    }
    Ok(Arc::new(BaiduClient::new(
        config.app_key,
        secret,
        config.app_name,
        token,
    )))
}

async fn start_webdav(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let _lifecycle = state.lifecycle.lock().await;
    let config = state.config.lock().unwrap().clone();
    let token = state
        .token
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "尚未授权".to_string())?;
    let secret = load_secret(SECRET_KEY).unwrap_or_default();
    let client = Arc::new(BaiduClient::new(
        config.app_key.clone(),
        secret,
        config.app_name.clone(),
        token.access_token,
    ));
    client
        .ensure_app_root()
        .await
        .map_err(|e| format!("百度网盘连接失败：{e}"))?;
    let temp_dir = state.data_dir.lock().unwrap().join("uploads");
    let password = if config.basic_auth {
        load_secret(WEBDAV_PASSWORD_KEY)
    } else {
        None
    };
    config.validate(password.as_ref().is_some_and(|p| !p.is_empty()))?;
    let old = { state.server.lock().unwrap().take() };
    if let Some(old) = old {
        old.shutdown_with_timeout(std::time::Duration::from_secs(30))
            .await?;
    }
    let handle = pandock_core::server::start(config, client, temp_dir, password).await?;
    *state.server.lock().unwrap() = Some(handle);
    *state.status.lock().unwrap() = ConnectionStatus::Connected;
    refresh_tray(app);
    Ok(())
}

fn report_start_failure(app: &AppHandle, error: &str) {
    let status = if error.starts_with("百度网盘连接失败") {
        ConnectionStatus::Disconnected {
            reason: "百度网盘暂时不可达".to_string(),
        }
    } else {
        ConnectionStatus::Error {
            reason: error.to_string(),
        }
    };
    *app.state::<AppState>().status.lock().unwrap() = status;
    refresh_tray(app);
}

async fn start_webdav_and_report(app: &AppHandle) -> Result<(), String> {
    *app.state::<AppState>().status.lock().unwrap() = ConnectionStatus::Connecting;
    refresh_tray(app);
    if let Err(error) = start_webdav(app).await {
        report_start_failure(app, &error);
        return Err(error);
    }
    Ok(())
}

fn spawn_health_monitor(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let should_check = {
                let state = app.state::<AppState>();
                state.server.lock().unwrap().is_some() && state.token.lock().unwrap().is_some()
            };
            if !should_check {
                continue;
            }
            let result = match client_from_state(&app) {
                Ok(client) => client.list("/").await.map(|_| ()),
                Err(error) => Err(error),
            };
            let state = app.state::<AppState>();
            match result {
                Ok(()) => {
                    *state.status.lock().unwrap() = ConnectionStatus::Connected;
                }
                Err(error) => {
                    let is_auth = error.contains("access_token")
                        || error.contains("31045")
                        || error.contains("401");
                    if is_auth {
                        let refreshed = if let Some(refresh) = load_secret(TOKEN_KEY) {
                            if let Ok(client) = client_from_state(&app) {
                                client.refresh_token(&refresh).await.ok()
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        if let Some(token) = refreshed {
                            let Some(rotated) = token.refresh_token.as_ref() else {
                                report_start_failure(&app, "百度刷新响应缺少 refresh token");
                                continue;
                            };
                            if let Err(error) = save_secret(TOKEN_KEY, rotated) {
                                report_start_failure(&app, &format!("无法保存新授权: {error}"));
                                continue;
                            }
                            *state.token.lock().unwrap() = Some(token);
                            let _ = start_webdav_and_report(&app).await;
                            continue;
                        }
                        *state.status.lock().unwrap() = ConnectionStatus::Error {
                            reason: "百度授权已失效，请重新授权".to_string(),
                        };
                    } else {
                        *state.status.lock().unwrap() = ConnectionStatus::Disconnected {
                            reason: "百度网盘暂时不可达".to_string(),
                        };
                    }
                }
            }
            refresh_tray(&app);
        }
    });
}

async fn stop_webdav(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let _lifecycle = state.lifecycle.lock().await;
    let handle = { state.server.lock().unwrap().take() };
    if let Some(handle) = handle {
        handle
            .shutdown_with_timeout(std::time::Duration::from_secs(30))
            .await?;
    }
    *state.status.lock().unwrap() = ConnectionStatus::Stopped;
    refresh_tray(app);
    Ok(())
}

async fn wait_for_port_released(addr: std::net::SocketAddr) -> Result<(), String> {
    for _ in 0..50 {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                drop(listener);
                return Ok(());
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    Err(format!("WebDAV 端口未释放：{addr}"))
}

fn refresh_tray(app: &AppHandle) {
    let state = app.state::<AppState>();
    let status = state.status.lock().unwrap().clone();
    let text = match &status {
        ConnectionStatus::Connected => "Pandock · 已连接",
        ConnectionStatus::WaitingForAuthorization => "Pandock · 等待授权",
        ConnectionStatus::Connecting => "Pandock · 连接中",
        ConnectionStatus::Error { reason } => {
            return set_tray_text(app, &format!("Pandock · {reason}"))
        }
        ConnectionStatus::Disconnected { reason } => {
            return set_tray_text(app, &format!("Pandock · {reason}"))
        }
        ConnectionStatus::Stopped => "Pandock · 已停止",
        ConnectionStatus::NotConfigured => "Pandock · 未配置",
    };
    set_tray_text(app, text);
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TrayIconKind {
    Green,
    Yellow,
    Red,
}

/// 托盘状态色映射（参照 shim tray.rs 语义）：
/// 已连接→绿；连接中/等待授权/断开/未配置/已停止→黄；异常→红。
fn tray_icon_kind(status: &ConnectionStatus) -> TrayIconKind {
    match status {
        ConnectionStatus::Connected => TrayIconKind::Green,
        ConnectionStatus::Error { .. } => TrayIconKind::Red,
        ConnectionStatus::Connecting
        | ConnectionStatus::WaitingForAuthorization
        | ConnectionStatus::Disconnected { .. }
        | ConnectionStatus::Stopped
        | ConnectionStatus::NotConfigured => TrayIconKind::Yellow,
    }
}

fn tray_icon_bytes(kind: TrayIconKind) -> &'static [u8] {
    match kind {
        TrayIconKind::Green => include_bytes!("../icons/status-green.png"),
        TrayIconKind::Yellow => include_bytes!("../icons/status-yellow.png"),
        TrayIconKind::Red => include_bytes!("../icons/status-red.png"),
    }
}

fn set_tray_text(app: &AppHandle, text: &str) {
    if let Some(item) = app.state::<AppState>().status_item.lock().unwrap().as_ref() {
        let _ = item.set_text(text);
    }
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(text));
        let status = app.state::<AppState>().status.lock().unwrap().clone();
        let kind = tray_icon_kind(&status);
        // 仅在状态色变化时 set_icon，避免每次刷新重复设置。
        let state = app.state::<AppState>();
        let mut current = state.tray_icon_kind.lock().unwrap();
        if *current == Some(kind) {
            return;
        }
        if let Ok(icon) = Image::from_bytes(tray_icon_bytes(kind)) {
            if tray.set_icon(Some(icon)).is_ok() {
                *current = Some(kind);
            }
        }
    }
}
#[cfg(test)]
mod tray_icon_tests {
    use super::*;

    #[test]
    fn tray_icon_kind_follows_spec_mapping() {
        use ConnectionStatus::*;
        assert_eq!(tray_icon_kind(&Connected), TrayIconKind::Green);
        assert_eq!(
            tray_icon_kind(&Error { reason: "x".into() }),
            TrayIconKind::Red
        );
        for status in [
            Connecting,
            WaitingForAuthorization,
            Disconnected { reason: "x".into() },
            Stopped,
            NotConfigured,
        ] {
            assert_eq!(
                tray_icon_kind(&status),
                TrayIconKind::Yellow,
                "未连接/异常以外的状态应为黄: {status:?}"
            );
        }
    }
}
fn show_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let before = w.is_visible().unwrap_or(false);
        if let Err(error) = w.unminimize() {
            log::warn!("恢复设置窗口失败: {error}");
        }
        if let Err(error) = w.show() {
            log::error!("显示设置窗口失败: {error}");
        }
        if let Err(error) = w.set_focus() {
            log::error!("聚焦设置窗口失败: {error}");
        }
        let after = w.is_visible().unwrap_or(false);
        log::info!("show_settings: before_visible={before} after_visible={after}");
    } else {
        log::error!("设置窗口 main 不存在");
    }
}

#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("只允许打开 HTTP/HTTPS 链接".into());
    }
    if url.chars().any(char::is_control) {
        return Err("链接包含非法控制字符".into());
    }

    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", &url])
        .spawn();

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&url).spawn();

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(&url).spawn();

    result
        .map(|_| ())
        .map_err(|e| format!("打开浏览器失败: {e}"))
}

#[tauri::command]
fn get_status(app: AppHandle) -> ConnectionStatus {
    app.state::<AppState>().status.lock().unwrap().clone()
}
#[tauri::command]
fn get_config(app: AppHandle) -> Config {
    app.state::<AppState>().config.lock().unwrap().clone()
}
#[tauri::command]
fn save_config(app: AppHandle, mut config: Config) -> Result<(), String> {
    let has_webdav_password =
        !config.webdav_password.is_empty() || load_secret(WEBDAV_PASSWORD_KEY).is_some();
    config.validate(has_webdav_password)?;
    if !config.webdav_password.is_empty() {
        save_secret(WEBDAV_PASSWORD_KEY, &config.webdav_password)?;
    }
    config.app_secret.clear();
    config.webdav_password.clear();
    save_config_file(&app, &config)?;
    let was_running = app.state::<AppState>().server.lock().unwrap().is_some();
    app.state::<AppState>()
        .config
        .lock()
        .unwrap()
        .clone_from(&config);
    if was_running {
        let app2 = app.clone();
        tauri::async_runtime::spawn(async move {
            let _ = start_webdav_and_report(&app2).await;
        });
    }
    Ok(())
}

#[tauri::command]
fn auth_state(app: AppHandle) -> AuthStateView {
    app.state::<AppState>().auth.lock().unwrap().view()
}

#[tauri::command]
async fn auth_begin(app: AppHandle, input: AuthInput) -> Result<AuthorizationChallenge, String> {
    begin_auth_flow(app, input).await
}

#[tauri::command]
async fn auth_retry(app: AppHandle, flow_id: String) -> Result<AuthorizationChallenge, String> {
    let session = {
        let state = app.state::<AppState>();
        let session = state.auth.lock().unwrap().active_session(&flow_id);
        session
    };
    let Some(session) = session else {
        return Err("授权请求已失效，请重新输入凭据".into());
    };
    let input = AuthInput {
        app_key: session.app_key().to_string(),
        app_name: session.app_name().to_string(),
        app_secret: session.app_secret().to_string(),
    };
    begin_auth_flow(app, input).await
}

#[tauri::command]
fn auth_cancel(app: AppHandle, flow_id: String) {
    let state = app.state::<AppState>();
    let mut auth = state.auth.lock().unwrap();
    match auth.cancel(&flow_id) {
        CancelOutcome::Ignored => return,
        CancelOutcome::Cancelled(_) => {}
    }

    emit_auth_state(&app, &auth);
    auth.finish_cancelled();
    emit_auth_state(&app, &auth);
}

#[tauri::command]
async fn auth_disconnect(app: AppHandle) {
    let _ = stop_webdav(&app).await;
    *app.state::<AppState>().token.lock().unwrap() = None;
    delete_secret(TOKEN_KEY);
    let state = app.state::<AppState>();
    let mut auth = state.auth.lock().unwrap();
    auth.invalidate_and_disconnect();
    emit_auth_state(&app, &auth);
    *app.state::<AppState>().status.lock().unwrap() = ConnectionStatus::WaitingForAuthorization;
    refresh_tray(&app);
}

#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

async fn begin_auth_flow(
    app: AppHandle,
    input: AuthInput,
) -> Result<AuthorizationChallenge, String> {
    let app_key = input.app_key.trim().to_string();
    let app_name = input.app_name.trim().to_string();
    let app_secret = input.app_secret.trim().to_string();
    if app_key.is_empty() || app_name.is_empty() || app_secret.is_empty() {
        return Err("请填写 App Key、Secret Key 和应用名称".into());
    }
    if app_name.contains('/') || app_name == "." || app_name == ".." {
        return Err("应用名称不能包含路径分隔符".into());
    }

    {
        let state = app.state::<AppState>();
        let mut auth = state.auth.lock().unwrap();
        let _ = auth.invalidate_active();
        auth.editing_credentials(&app_key, &app_name);
        auth.requesting_device_code();
        emit_auth_state(&app, &auth);
    }

    let client = Arc::new(BaiduClient::new(
        app_key.clone(),
        app_secret.clone(),
        app_name.clone(),
        String::new(),
    ));
    let flow = match DeviceAuth::start(client).await {
        Ok(flow) => flow,
        Err(error) => {
            let state = app.state::<AppState>();
            let mut auth = state.auth.lock().unwrap();
            auth.failed(auth_failure(error.clone()));
            emit_auth_state(&app, &auth);
            return Err(error);
        }
    };

    let flow_id = uuid::Uuid::new_v4().to_string();
    let challenge = AuthorizationChallenge {
        flow_id: flow_id.clone(),
        user_code: flow.info.user_code.clone(),
        verification_url: flow.info.verification_url.clone(),
        qr_code_url: flow.info.qrcode_url.clone(),
        expires_at: now_secs() + flow.info.expires_in as i64,
        poll_interval_seconds: flow.info.interval.unwrap_or(6).max(6),
    };
    let session = AuthSession::new(flow_id.clone(), app_key, app_name, app_secret);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    {
        let state = app.state::<AppState>();
        let mut auth = state.auth.lock().unwrap();
        auth.waiting_authorization(session, challenge.clone(), cancel_tx);
        emit_auth_state(&app, &auth);
    }
    spawn_auth_wait(app, flow, flow_id, cancel_rx);
    Ok(challenge)
}

fn spawn_auth_wait(
    app: AppHandle,
    flow: DeviceAuth,
    flow_id: String,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) {
    tauri::async_runtime::spawn(async move {
        let result = tokio::select! {
            result = flow.wait() => Some(result),
            changed = cancel_rx.changed() => {
                if changed.is_ok() && *cancel_rx.borrow() {
                    None
                } else {
                    return;
                }
            }
        };
        match result {
            None => {}
            Some(Ok(token)) => {
                let session = {
                    let state = app.state::<AppState>();
                    let mut auth = state.auth.lock().unwrap();
                    auth.accept_token(&flow_id)
                };
                let Some(session) = session else {
                    return;
                };
                let app_key = session.app_key().to_string();
                let app_name = session.app_name().to_string();
                if let Err(error) = save_secret(SECRET_KEY, session.app_secret()) {
                    fail_auth(
                        &app,
                        AuthErrorView::new(
                            pandock_core::auth_flow::AuthErrorCode::KeyringFailed,
                            format!("无法安全保存百度授权: {error}"),
                            true,
                        ),
                    );
                    return;
                }
                let Some(refresh) = token.refresh_token.as_ref() else {
                    fail_auth(&app, auth_failure("百度未返回 refresh token"));
                    return;
                };
                if let Err(error) = save_secret(TOKEN_KEY, refresh) {
                    fail_auth(
                        &app,
                        AuthErrorView::new(
                            pandock_core::auth_flow::AuthErrorCode::KeyringFailed,
                            format!("无法安全保存百度授权: {error}"),
                            true,
                        ),
                    );
                    return;
                }

                {
                    let state = app.state::<AppState>();
                    let mut config = state.config.lock().unwrap().clone();
                    config.app_key = app_key;
                    config.app_name = app_name;
                    config.app_secret.clear();
                    if let Err(error) = save_config_file(&app, &config) {
                        fail_auth(&app, auth_failure(error));
                        return;
                    }
                    *state.config.lock().unwrap() = config;
                    *state.token.lock().unwrap() = Some(token);
                }
                drop(session);

                {
                    let state = app.state::<AppState>();
                    let mut auth = state.auth.lock().unwrap();
                    auth.exchanging_token();
                    emit_auth_state(&app, &auth);
                    auth.starting_webdav();
                    emit_auth_state(&app, &auth);
                }

                match start_webdav_and_report(&app).await {
                    Ok(()) => {
                        let state = app.state::<AppState>();
                        let mut auth = state.auth.lock().unwrap();
                        auth.connected(now_secs());
                        emit_auth_state(&app, &auth);
                    }
                    Err(error) => {
                        fail_auth(
                            &app,
                            AuthErrorView::new(
                                pandock_core::auth_flow::AuthErrorCode::WebdavStartFailed,
                                error,
                                true,
                            ),
                        );
                    }
                }
            }
            Some(Err(error)) => {
                let active = {
                    let state = app.state::<AppState>();
                    let active = state.auth.lock().unwrap().is_active(&flow_id);
                    active
                };
                if !active {
                    return;
                }
                let mapped = auth_failure(error);
                let keep_pending = matches!(
                    mapped.code,
                    pandock_core::auth_flow::AuthErrorCode::Timeout
                        | pandock_core::auth_flow::AuthErrorCode::NetworkUnreachable
                        | pandock_core::auth_flow::AuthErrorCode::ExpiredToken
                );
                if !keep_pending {
                    let state = app.state::<AppState>();
                    let mut auth = state.auth.lock().unwrap();
                    let _ = auth.invalidate_active();
                }
                fail_auth(&app, mapped);
            }
        }
    });
}

fn fail_auth(app: &AppHandle, error: AuthErrorView) {
    let state = app.state::<AppState>();
    let mut auth = state.auth.lock().unwrap();
    auth.failed(error);
    emit_auth_state(app, &auth);
}

async fn emit_update_state(app: &AppHandle) {
    let view = app.state::<AppState>().update.view().await;
    let _ = app.emit("pandock://update/state", view);
}

#[tauri::command]
async fn update_state(app: AppHandle) -> update_runtime::UpdateStateView {
    app.state::<AppState>().update.view().await
}

#[tauri::command]
async fn update_check(app: AppHandle) -> Result<update_runtime::UpdateStateView, String> {
    match app.state::<AppState>().update.check().await {
        Ok(view) => {
            emit_update_state(&app).await;
            Ok(view)
        }
        Err(error) => {
            app.state::<AppState>().update.set_error(error.clone());
            Err(error.to_string())
        }
    }
}

#[tauri::command]
async fn update_download(app: AppHandle) -> Result<update_runtime::UpdateStateView, String> {
    match app.state::<AppState>().update.download().await {
        Ok(view) => {
            emit_update_state(&app).await;
            Ok(view)
        }
        Err(error) => {
            app.state::<AppState>().update.set_error(error.clone());
            Err(error.to_string())
        }
    }
}

#[cfg(target_os = "windows")]
#[tauri::command]
async fn update_install(app: AppHandle) -> Result<(), String> {
    let webdav_addr = app
        .state::<AppState>()
        .server
        .lock()
        .unwrap()
        .as_ref()
        .map(|handle| handle.addr());
    stop_webdav(&app).await?;
    if let Some(addr) = webdav_addr {
        wait_for_port_released(addr).await?;
    }
    match app.state::<AppState>().update.pending_kind() {
        update_runtime::PendingKind::Velopack => {
            // Velopack apply 会拉起 Update.exe 并退出当前进程。
            app.state::<AppState>()
                .update
                .install()
                .await
                .map_err(|error| error.to_string())
        }
        update_runtime::PendingKind::Portable => update_install_portable(&app).await,
        update_runtime::PendingKind::None => Err("没有已下载的更新".to_string()),
    }
}

/// Portable 实例的就地更新：journal + helper 交换（ADR-102 机制）。
#[cfg(target_os = "windows")]
async fn update_install_portable(app: &AppHandle) -> Result<(), String> {
    let staged = app
        .state::<AppState>()
        .update
        .portable_staged()
        .ok_or_else(|| "没有已下载并验证的更新".to_string())?;
    let app_dir = std::env::current_exe()
        .map_err(|error| error.to_string())?
        .parent()
        .ok_or_else(|| "无法定位应用目录".to_string())?
        .to_path_buf();
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let journal_file = journal_path(&app_dir);
    let rollback_dir = app_dir
        .join("updates")
        .join("rollback")
        .join(&current_version);
    let readiness_token = uuid::Uuid::new_v4().to_string();
    let journal = UpdateJournal::new(
        current_version,
        staged.version.clone(),
        &app_dir,
        &staged.staging_dir,
        &rollback_dir,
        &staged.helper_path,
        std::process::id(),
        &readiness_token,
    );
    journal
        .save(&journal_file)
        .map_err(|error| error.to_string())?;
    let launch = app_dir.join(LAUNCH_EXE_NAME);
    std::process::Command::new(&staged.helper_path)
        .arg("--protocol")
        .arg("1")
        .arg("--journal")
        .arg(&journal_file)
        .arg("--target-app")
        .arg(&app_dir)
        .arg("--launch")
        .arg(&launch)
        .arg("--expected-version")
        .arg(&staged.version)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--readiness-token")
        .arg(&readiness_token)
        .current_dir(&app_dir)
        .spawn()
        .map_err(|error| error.to_string())?;
    app.state::<AppState>().update.mark_installing();
    app.exit(0);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
async fn update_install(_app: AppHandle) -> Result<(), String> {
    Err("当前平台只支持手动下载".into())
}

#[tauri::command]
async fn stop_webdav_command(app: AppHandle) {
    let _ = stop_webdav(&app).await;
}
#[tauri::command]
async fn reconnect(app: AppHandle) -> Result<(), String> {
    start_webdav_and_report(&app).await
}
#[tauri::command]
async fn disconnect(app: AppHandle) {
    auth_disconnect(app).await;
}

/// 前端按平台裁剪功能入口（例如 NativeSync 仅 Windows）。
#[tauri::command]
fn runtime_platform() -> &'static str {
    std::env::consts::OS
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> Result<bool, String> {
    autostart::is_enabled(&app)
}
#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    autostart::set_enabled(&app, enabled)?;
    let state = app.state::<AppState>();
    let mut config = state.config.lock().unwrap().clone();
    config.launch_at_login = enabled;
    save_config_file(&app, &config)?;
    *state.config.lock().unwrap() = config;
    if let Some(item) = app
        .state::<AppState>()
        .autostart_item
        .lock()
        .unwrap()
        .as_ref()
    {
        let _ = item.set_checked(enabled);
    }
    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Info)
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: None,
                    }),
                ])
                .build(),
        )
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_settings(app)
        }))
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name("Pandock")
                .build(),
        )
        .manage(AppState {
            auth: Mutex::new(AuthFlowCoordinator::new()),
            update: update_runtime::UpdateRuntime::new(env!("CARGO_PKG_VERSION"))
                .expect("初始化更新运行时失败"),
            config: Mutex::new(Config::default()),
            status: Mutex::new(ConnectionStatus::NotConfigured),
            token: Mutex::new(None),
            server: Mutex::new(None),
            lifecycle: tokio::sync::Mutex::new(()),
            data_dir: Mutex::new(PathBuf::new()),
            tray_icon_kind: Mutex::new(None),
            status_item: Mutex::new(None),
            autostart_item: Mutex::new(None),
            native_sync: tokio::sync::Mutex::new(None),
        })
        .setup(|app| {
            migrate_legacy_runtime(app.handle())?;
            migrate_legacy_keyring();
            cleanup_legacy_exe();
            let mut config = load_config(app.handle());
            match autostart::reconcile(app.handle(), config.launch_at_login) {
                Ok(enabled) if enabled != config.launch_at_login => {
                    config.launch_at_login = enabled;
                    if let Err(error) = save_config_file(app.handle(), &config) {
                        log::warn!("同步开机启动配置失败: {error}");
                    }
                }
                Ok(_) => {}
                Err(error) => log::warn!("修复开机启动项失败: {error}"),
            }
            log::info!("Pandock 启动: version={}", env!("CARGO_PKG_VERSION"));
            // 首次运行（数据目录中还没有配置文件）时打开设置窗口，
            // 让无向导安装的用户落地即看到百度网盘连接入口。
            let first_run = !config_path(app.handle())
                .map(|path| path.exists())
                .unwrap_or(false);
            let data_dir = data_dir(app.handle())?;
            let has_token = load_secret(TOKEN_KEY).is_some();
            let state = app.state::<AppState>();
            *state.config.lock().unwrap() = config.clone();
            let readiness_token = readiness_token_from_args();
            let app_dir = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|parent| parent.to_path_buf()));
            if let Some(app_dir) = app_dir.as_ref() {
                let existing_journal = journal_path(app_dir);
                if existing_journal.exists() {
                    if let Ok(journal) = UpdateJournal::load(&existing_journal) {
                        let active_self = journal.state == JournalState::LaunchStarted
                            && readiness_token.as_deref() == Some(journal.readiness_token.as_str());
                        if journal.state != JournalState::Completed && !active_self {
                            state.update.mark_recovery_required(format!(
                                "检测到未完成的更新 journal：{}，请先修复或回滚。",
                                existing_journal.display()
                            ));
                        }
                    }
                }
            }
            *state.data_dir.lock().unwrap() = data_dir;
            // P1/§7.3 启动自动重连：db 中 enabled=1 时静默恢复注册/连接并预热映射；
            // 未启用、未授权或非 Windows 时保持原状，不弹窗打扰。
            // P2: 网络调用（refresh_token）不得在 native_sync 锁内进行——
            // 先无锁构造 backend，再短暂持锁注入，最后本地 reconnect（无网络）。
            {
                let autostart_app = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let state = autostart_app.state::<AppState>();
                    let data_dir = match state.data_dir.lock() {
                        Ok(guard) => guard.clone(),
                        Err(_) => return,
                    };
                    // 1) 无锁网络阶段：backend 获取（复用 access token 或 refresh 轮换）。
                    let backend = {
                        let token = state.token.lock().unwrap().clone();
                        let config = state.config.lock().unwrap().clone();
                        let secret = load_secret(SECRET_KEY).unwrap_or_default();
                        if config.app_key.is_empty() || config.app_name.is_empty() || secret.is_empty() {
                            None
                        } else {
                            let access = match token {
                                Some(t) => Some(t.access_token),
                                None => match load_secret(TOKEN_KEY) {
                                    Some(refresh) => {
                                        let bootstrap = pandock_core::baidu::BaiduClient::new(
                                            config.app_key.clone(),
                                            secret.clone(),
                                            config.app_name.clone(),
                                            String::new(),
                                        );
                                        match bootstrap.refresh_token(&refresh).await {
                                            Ok(t) => {
                                                if let Some(rotated) = t.refresh_token.as_ref() {
                                                    let _ = save_secret(TOKEN_KEY, rotated);
                                                }
                                                Some(t.access_token)
                                            }
                                            Err(_) => None,
                                        }
                                    }
                                    None => None,
                                },
                            };
                            access.map(|access| {
                                let client = std::sync::Arc::new(pandock_core::baidu::BaiduClient::new(
                                    config.app_key,
                                    secret,
                                    config.app_name,
                                    access,
                                ));
                                let cloud: std::sync::Arc<dyn pandock_core::cloud::CloudFs> =
                                    std::sync::Arc::new(pandock_core::cloud::BaiduCloudFs::new(client));
                                std::sync::Arc::new(pandock_nativesync::SnapshotBackend::new(cloud))
                                    as std::sync::Arc<dyn pandock_nativesync::CloudBackend>
                            })
                        }
                    };
                    // 2) 短暂持锁：确保 manager 存在并注入 backend。
                    {
                        let mut guard = state.native_sync.lock().await;
                        if guard.is_none() {
                            match pandock_nativesync::NativeSyncManager::new(&data_dir, None) {
                                Ok(manager) => *guard = Some(manager),
                                Err(_) => return,
                            }
                        }
                        if let (Some(manager), Some(backend)) = (guard.as_mut(), backend) {
                            manager.set_backend(backend);
                        }
                    }
                    // 3) reconnect 为本地操作（db + Cloud Files API），持锁短暂完成。
                    let status = {
                        let mut guard = state.native_sync.lock().await;
                        match guard.as_mut() {
                            Some(manager) => manager.reconnect_from_db().ok(),
                            None => None,
                        }
                    };
                    if let Some(status) = status {
                        use tauri::Emitter;
                        let _ = autostart_app.emit(
                            crate::native_sync::NATIVE_SYNC_STATE_EVENT,
                            &status,
                        );
                        log::info!(
                            "native-sync autostart reconnect: enabled={} state={} total={}",
                            status.enabled,
                            status.state,
                            status.total_entries
                        );
                    }
                });
            }
            {
                let mut auth = state.auth.lock().unwrap();
                if config.app_key.is_empty() || config.app_name.is_empty() {
                    auth.invalidate_and_disconnect();
                } else if has_token {
                    auth.starting_webdav();
                } else {
                    auth.invalidate_and_disconnect();
                }
                emit_auth_state(app.handle(), &auth);
            }
            *state.status.lock().unwrap() =
                if config.app_key.is_empty() || config.app_name.is_empty() {
                    ConnectionStatus::NotConfigured
                } else if has_token {
                    ConnectionStatus::Connecting
                } else {
                    ConnectionStatus::WaitingForAuthorization
                };
            let status_item = MenuItemBuilder::with_id("status", "Pandock · 未配置")
                .enabled(false)
                .build(app)?;
            let sync_manager = MenuItemBuilder::with_id("sync-manager", "打开同步资源浏览")
                .enabled(true)
                .build(app)?;
            let settings = MenuItemBuilder::with_id("settings", "打开设置")
                .enabled(true)
                .build(app)?;

            let autostart_enabled = {
                use tauri_plugin_autostart::ManagerExt;
                app.autolaunch().is_enabled().unwrap_or(false)
            };
            let autostart = CheckMenuItemBuilder::with_id("autostart", "开机启动")
                .enabled(true)
                .checked(autostart_enabled)
                .build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "退出")
                .enabled(true)
                .build(app)?;
            let menu = MenuBuilder::new(app)
                .items(&[&status_item, &sync_manager, &settings, &autostart, &quit])
                .build()?;
            *state.status_item.lock().unwrap() = Some(status_item);
            *state.autostart_item.lock().unwrap() = Some(autostart);
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Pandock")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| {
                    log::info!("tray menu event: {}", event.id.as_ref());
                    match event.id.as_ref() {
                    "sync-manager" => {
                        let _ = crate::native_sync::open_manager_window(app);
                    }
                    "settings" => show_settings(app),

                    "autostart" => {
                        use tauri_plugin_autostart::ManagerExt;
                        let m = app.autolaunch();
                        let on = m.is_enabled().unwrap_or(false);
                        if let Err(error) = set_autostart(app.clone(), !on) {
                            log::error!("切换开机启动失败: {error}");
                        }
                    }
                    "quit" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = stop_webdav(&app).await;
                            app.exit(0);
                        });
                    }
                    _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::DoubleClick { .. } = event {
                        // 双击：NativeSync 已启用 → 同步资源浏览；未启用 → 回退设置面板。
                        let app = tray.app_handle().clone();
                        tauri::async_runtime::spawn(async move {
                            let open_sync = {
                                let state = app.state::<AppState>();
                                let guard = state.native_sync.lock().await;
                                match guard.as_ref() {
                                    Some(manager) => {
                                        matches!(manager.status(), Ok(s) if s.enabled)
                                    }
                                    None => false,
                                }
                            };
                            if open_sync {
                                let _ = crate::native_sync::open_manager_window(&app);
                            } else {
                                show_settings(&app);
                            }
                        });
                    }
                })
                .build(app)?;
            refresh_tray(app.handle());
            if first_run || std::env::var_os("PANDOCK_SHOW_SETTINGS").is_some() {
                show_settings(app.handle());
            }
            spawn_health_monitor(app.handle().clone());
            let requires_webdav = has_token && config.start_webdav_automatically;
            if !requires_webdav {
                if let (Some(token), Some(app_dir)) = (readiness_token.clone(), app_dir.clone()) {
                    let _ = write_readiness_after_startup(
                        &app_dir,
                        &token,
                        env!("CARGO_PKG_VERSION"),
                        true,
                        false,
                        false,
                    );
                }
            }
            if has_token && config.start_webdav_automatically {
                let app2 = app.handle().clone();
                let readiness_token = readiness_token.clone();
                let app_dir = app_dir.clone();
                tauri::async_runtime::spawn(async move {
                    if let Ok(client) = client_from_state(&app2) {
                        if let Some(refresh) = load_secret(TOKEN_KEY) {
                            match client.refresh_token(&refresh).await {
                                Ok(token) => {
                                    let Some(rotated) = token.refresh_token.as_ref() else {
                                        report_start_failure(
                                            &app2,
                                            "百度刷新响应缺少 refresh token",
                                        );
                                        return;
                                    };
                                    if let Err(error) = save_secret(TOKEN_KEY, rotated) {
                                        report_start_failure(
                                            &app2,
                                            &format!("无法保存新授权: {error}"),
                                        );
                                        return;
                                    }
                                    *app2.state::<AppState>().token.lock().unwrap() = Some(token);
                                    match start_webdav_and_report(&app2).await {
                                        Ok(()) => {
                                            let state = app2.state::<AppState>();
                                            let mut auth = state.auth.lock().unwrap();
                                            auth.connected(now_secs());
                                            emit_auth_state(&app2, &auth);
                                            drop(auth);
                                            if let (Some(token), Some(app_dir)) =
                                                (readiness_token.clone(), app_dir.clone())
                                            {
                                                let _ = write_readiness_after_startup(
                                                    &app_dir,
                                                    &token,
                                                    env!("CARGO_PKG_VERSION"),
                                                    true,
                                                    true,
                                                    true,
                                                );
                                            }
                                        }
                                        Err(error) => {
                                            fail_auth(&app2, AuthErrorView::new(
                                                pandock_core::auth_flow::AuthErrorCode::WebdavStartFailed,
                                                error,
                                                true,
                                            ));
                                        }
                                    }
                                }
                                Err(error) => {
                                    *app2.state::<AppState>().status.lock().unwrap() =
                                        ConnectionStatus::Error {
                                            reason: format!("Token 刷新失败: {error}"),
                                        };
                                    fail_auth(&app2, auth_failure(format!("Token 刷新失败: {error}")));
                                    refresh_tray(&app2);
                                }
                            }
                        }
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            open_external_url,
            get_config,
            save_config,
            auth_state,
            auth_begin,
            auth_retry,
            auth_cancel,
            auth_disconnect,
            app_version,
            update_state,
            runtime_platform,
            update_check,
            update_download,
            update_install,
            stop_webdav_command,
            reconnect,
            disconnect,
            get_autostart,
            set_autostart,
            native_sync::native_sync_status,
            native_sync::native_sync_enable,
            native_sync::native_sync_disable,
            native_sync::native_sync_sync_now,
            native_sync::native_sync_list,
            native_sync::native_sync_action,
            native_sync::native_sync_open_manager,
            native_sync::native_sync_open_in_explorer
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn port_release_check_succeeds_after_listener_drop() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(wait_for_port_released(addr).await.is_ok());
    }
}
