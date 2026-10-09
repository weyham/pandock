import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { NativeSyncSettings, type NativeSyncStatus } from "./NativeSync";

export type Status = { kind: string; reason?: string };
export type Config = {
  app_key: string;
  app_name: string;
  listen: string;
  dav_prefix: string;
  basic_auth: boolean;
  username: string;
  webdav_password: string;
  cache_size_mb: number;
  launch_at_login: boolean;
  start_webdav_automatically: boolean;
};
export type AuthPhase = "disconnected" | "editing_credentials" | "requesting_device_code" | "waiting_authorization" | "exchanging_token" | "starting_webdav" | "connected" | "cancelling" | "cancelled" | "error";
export type AuthErrorView = { code: string; message: string; retryable: boolean };
export type AuthorizationChallenge = { flowId: string; userCode?: string | null; verificationUrl?: string | null; qrCodeUrl?: string | null; expiresAt: number; pollIntervalSeconds: number };
export type AuthStateView = { phase: AuthPhase; appKey?: string | null; appName?: string | null; challenge?: AuthorizationChallenge | null; error?: AuthErrorView | null; connectedAt?: number | null };
export type UpdatePhase = "idle" | "checking" | "up_to_date" | "update_available" | "downloading" | "ready_to_install" | "installing" | "completed" | "manual_download" | "error";
export type UpdateSourceError = {
  code: string;
  message: string;
  retryable: boolean;
  retryAfterSeconds?: number | null;
  status?: number | null;
  host?: string | null;
};
export type UpdateStateView = { phase: UpdatePhase; currentVersion: string; availableVersion?: string | null; releaseUrl?: string | null; manualOnly: boolean; error?: UpdateSourceError | null };

const fallbackConfig: Config = { app_key: "", app_name: "", listen: "127.0.0.1:19090", dav_prefix: "/dav", basic_auth: false, username: "dav", webdav_password: "", cache_size_mb: 256, launch_at_login: false, start_webdav_automatically: true };
const fallbackAuth: AuthStateView = { phase: "disconnected", appKey: null, appName: null, challenge: null, error: null, connectedAt: null };
const fallbackUpdate: UpdateStateView = { phase: "idle", currentVersion: "1.0.0", availableVersion: null, releaseUrl: null, manualOnly: false, error: null };
const labels: Record<string, string> = { not_configured: "未配置", stopped: "服务已停止", waiting_for_authorization: "等待授权", connecting: "连接中", connected: "已连接", disconnected: "连接断开", error: "发生错误" };
const phaseLabels: Record<UpdatePhase, string> = { idle: "空闲", checking: "检查中", up_to_date: "已是最新", update_available: "有更新", downloading: "下载中", ready_to_install: "可安装", installing: "安装中", completed: "已完成", manual_download: "手动下载", error: "异常" };
const formatTimestamp = (value?: number | null) => value ? new Date(value * 1000).toLocaleString() : "—";

function useDelayedBusy(delayMs = 500) {
  const [pendingKeys, setPendingKeys] = React.useState<ReadonlySet<string>>(new Set());
  const [visibleKeys, setVisibleKeys] = React.useState<ReadonlySet<string>>(new Set());
  const run = React.useCallback(async <T,>(key: string, task: () => Promise<T>): Promise<T> => {
    setPendingKeys((prev) => new Set(prev).add(key));
    const timer = window.setTimeout(() => setVisibleKeys((prev) => new Set(prev).add(key)), delayMs);
    try {
      return await task();
    } finally {
      window.clearTimeout(timer);
      const remove = (prev: ReadonlySet<string>) => { const next = new Set(prev); next.delete(key); return next; };
      setPendingKeys(remove);
      setVisibleKeys(remove);
    }
  }, [delayMs]);
  const pending = React.useCallback((key: string) => pendingKeys.has(key), [pendingKeys]);
  const spinning = React.useCallback((key: string) => visibleKeys.has(key), [visibleKeys]);
  return { run, pending, spinning };
}

const Spinner = () => <span className="spinner" role="progressbar" aria-label="加载中" />;

export function App() {
  const [status, setStatus] = React.useState<Status>({ kind: "not_configured" });
  const [config, setConfig] = React.useState<Config>(fallbackConfig);
  const [authState, setAuthState] = React.useState<AuthStateView>(fallbackAuth);
  const [autostart, setAutostart] = React.useState(false);
  const [message, setMessage] = React.useState("");
  const [modalOpen, setModalOpen] = React.useState(false);
  const [modalStep, setModalStep] = React.useState<"credentials" | "requesting" | "waiting" | "error">("credentials");
  const [appKey, setAppKey] = React.useState("");
  const [appName, setAppName] = React.useState("");
  const [appSecret, setAppSecret] = React.useState("");
  const [modalError, setModalError] = React.useState("");
  const [challenge, setChallenge] = React.useState<AuthorizationChallenge | null>(null);
  const modalOpenRef = React.useRef(false);
  const [updateState, setUpdateState] = React.useState<UpdateStateView>(fallbackUpdate);
  const [platform, setPlatform] = React.useState<string>("windows");
  const [nativeSync, setNativeSync] = React.useState<NativeSyncStatus | null>(null);
  const [nativeSyncRoot, setNativeSyncRoot] = React.useState("");
  const [nativeSyncError, setNativeSyncError] = React.useState("");
  const [savedConfig, setSavedConfig] = React.useState<Config | null>(null);
  const busy = useDelayedBusy();
  const updateBusy = busy.pending("update_check") || busy.pending("update_download") || busy.pending("update_install");
  const configDirty = React.useMemo(() => {
    if (!savedConfig) return false;
    const keys: (keyof Config)[] = ["app_key", "app_name", "listen", "dav_prefix", "basic_auth", "username", "cache_size_mb", "launch_at_login", "start_webdav_automatically"];
    if (keys.some((key) => config[key] !== savedConfig[key])) return true;
    return config.webdav_password.trim() !== "";
  }, [config, savedConfig]);

  const openModal = React.useCallback(() => {
    setAppKey(authState.appKey || config.app_key || "");
    setAppName(authState.appName || config.app_name || "");
    setAppSecret("");
    setModalError("");
    setChallenge(null);
    setModalStep("credentials");
    modalOpenRef.current = true;
    setModalOpen(true);
  }, [authState.appKey, authState.appName, config.app_key, config.app_name]);

  const refreshUpdateState = React.useCallback(async () => {
    try {
      const value = await invoke<UpdateStateView>("update_state");
      if (value) setUpdateState(value);
    } catch {
      // The About tile keeps the last known state if the backend is unavailable.
    }
  }, []);

  const nativeSyncRef = React.useRef<NativeSyncStatus | null>(null);
  const applyNativeSyncStatus = React.useCallback((next: NativeSyncStatus) => {
    const previous = nativeSyncRef.current;
    const looksLikeErrorDefault = !next.enabled && next.state === "disabled" && !next.rootPath && !next.lastError;
    if (previous?.enabled && looksLikeErrorDefault) return;
    nativeSyncRef.current = next;
    setNativeSync(next);
  }, []);

  React.useEffect(() => {
    invoke<string>("runtime_platform").then(setPlatform).catch(() => {});
  }, []);

  const refreshNativeSync = React.useCallback(async () => {
    try {
      const next = await busy.run("native_sync_status", () => invoke<NativeSyncStatus>("native_sync_status"));
      applyNativeSyncStatus(next);
      setNativeSyncRoot(next.rootPath || "");
      setNativeSyncError("");
    } catch (error) { setNativeSyncError(String(error)); }
  }, [busy.run]);

  React.useEffect(() => {
    void invoke<Status>("get_status").then(setStatus);
    void invoke<Config>("get_config").then((value) => {
      const merged = { ...fallbackConfig, ...value };
      setConfig(merged);
      setSavedConfig(merged);
    });
    void invoke<boolean>("get_autostart").then(setAutostart);
    void invoke<AuthStateView>("auth_state").then(setAuthState);
    void refreshUpdateState();
    const timer = window.setInterval(() => void invoke<Status>("get_status").then(setStatus), 3000);
    const unlistenAuth = listen<AuthStateView>("pandock://auth/state", (event) => setAuthState(event.payload));
    const unlistenRequest = listen("auth-requested", () => openModal());
    const unlistenUpdate = listen<UpdateStateView>("pandock://update/state", (event) => setUpdateState(event.payload));
    const unlistenNativeSync = listen<NativeSyncStatus>("pandock://nativesync/state", (event) => { applyNativeSyncStatus(event.payload); if (event.payload.rootPath) setNativeSyncRoot(event.payload.rootPath); });
    return () => {
      window.clearInterval(timer);
      void unlistenAuth.then((off) => off());
      void unlistenRequest.then((off) => off());
      void unlistenUpdate.then((off) => off());
      void unlistenNativeSync.then((off) => off());
    };
  }, [openModal, refreshUpdateState, refreshNativeSync]);

  React.useEffect(() => {
    if (authState.phase === "connected") {
      modalOpenRef.current = false;
      setModalOpen(false);
      setAppSecret("");
      setChallenge(null);
    } else if (authState.phase === "waiting_authorization" && authState.challenge) {
      setChallenge(authState.challenge);
      setModalStep("waiting");
    } else if (authState.phase === "error" && authState.error) {
      setModalError(authState.error.message);
      setModalStep("error");
    } else if (authState.phase === "cancelled" || authState.phase === "disconnected") {
      if (authState.phase === "cancelled") {
        modalOpenRef.current = false;
        setModalOpen(false);
      }
    }
  }, [authState]);

  const closeModal = async () => {
    modalOpenRef.current = false;
    if (challenge?.flowId) {
      try { await invoke("auth_cancel", { flowId: challenge.flowId }); } catch { /* ignore */ }
    }
    setAppSecret("");
    setChallenge(null);
    setModalOpen(false);
  };

  const beginAuth = async () => {
    if (!appKey.trim() || !appName.trim() || !appSecret.trim()) {
      setModalError("请填写 App Key、Secret Key 和应用名称。");
      setModalStep("error");
      return;
    }
    setModalError("");
    setModalStep("requesting");
    try {
      const next = await invoke<AuthorizationChallenge>("auth_begin", { input: { appKey: appKey.trim(), appName: appName.trim(), appSecret } });
      if (!modalOpenRef.current) {
        try { await invoke("auth_cancel", { flowId: next.flowId }); } catch { /* ignore */ }
        return;
      }
      setChallenge(next);
      setModalStep("waiting");
    } catch (error) {
      setModalError(String(error));
      setModalStep("error");
    } finally {
      setAppSecret("");
    }
  };

  const retryAuth = async () => {
    if (!challenge?.flowId) {
      setModalStep("credentials");
      return;
    }
    setModalError("");
    setModalStep("requesting");
    try {
      const next = await invoke<AuthorizationChallenge>("auth_retry", { flowId: challenge.flowId });
      if (!modalOpenRef.current) {
        try { await invoke("auth_cancel", { flowId: next.flowId }); } catch { /* ignore */ }
        return;
      }
      setChallenge(next);
      setModalStep("waiting");
    } catch (error) {
      setModalError(String(error));
      setModalStep("credentials");
    }
  };

  const checkUpdate = async () => {
    try { setUpdateState(await busy.run("update_check", () => invoke<UpdateStateView>("update_check"))); }
    catch { await refreshUpdateState(); }
  };

  const downloadUpdate = async () => {
    try { setUpdateState(await busy.run("update_download", () => invoke<UpdateStateView>("update_download"))); }
    catch { await refreshUpdateState(); }
  };

  const installUpdate = async () => {
    try { await busy.run("update_install", () => invoke("update_install")); }
    catch { await refreshUpdateState(); }
  };

  const disconnect = async () => {
    try { await busy.run("auth_disconnect", () => invoke("auth_disconnect")); setModalOpen(false); setAppSecret(""); } catch (error) { setMessage(String(error)); }
  };
  const reconnect = async () => { try { await busy.run("webdav_reconnect", () => invoke("reconnect")); setMessage("WebDAV 已重新连接。"); } catch (error) { setMessage(String(error)); } };
  const stopWebdav = async () => { try { await busy.run("webdav_stop", () => invoke("stop_webdav_command")); setMessage("WebDAV 已停止。"); } catch (error) { setMessage(String(error)); } };
  const toggleAutostart = async () => {
    const next = !autostart;
    try {
      await busy.run("set_autostart", () => invoke("set_autostart", { enabled: next }));
      setAutostart(next);
      setConfig((prev) => ({ ...prev, launch_at_login: next }));
      setSavedConfig((prev) => (prev ? { ...prev, launch_at_login: next } : prev));
    } catch (error) {
      setMessage(String(error));
    }
  };
  const save = async () => {
    const payload = { ...config, app_secret: "" };
    try {
      await busy.run("save_config", () => invoke("save_config", { config: payload }));
      setSavedConfig({ ...config, webdav_password: "" });
      setConfig((prev) => ({ ...prev, webdav_password: "" }));
      setMessage("设置已保存。");
    } catch (error) { setMessage(String(error)); }
  };
  const copyUrl = async () => { await navigator.clipboard.writeText("http://" + config.listen + config.dav_prefix + "/"); setMessage("WebDAV 地址已复制。"); };
  const openExternal = async (url?: string | null) => { if (!url) return; try { await invoke("open_external_url", { url }); } catch (error) { setModalError(String(error)); } };
  const [page, setPage] = React.useState<"general" | "webdav" | "nativesync" | "about">("general");
  const connected = authState.phase === "connected";
  const runNativeSync = async (key: string, command: string, args?: Record<string, unknown>) => {
    try {
      const next = await busy.run(key, () => invoke<NativeSyncStatus>(command, args));
      applyNativeSyncStatus(next);
      if (next.rootPath) setNativeSyncRoot(next.rootPath);
      setNativeSyncError("");
    } catch (error) { setNativeSyncError(String(error)); }
  };
  const enableNativeSync = () => void runNativeSync("native_sync_enable", "native_sync_enable", { rootPath: nativeSyncRoot.trim() || null });
  const disableNativeSync = () => void runNativeSync("native_sync_disable", "native_sync_disable", { unregister: false });
  const syncNativeNow = () => void runNativeSync("native_sync_sync_now", "native_sync_sync_now");

  const renderUpdateError = () => {
    const error = updateState.error;
    if (!error?.message) return null;
    const code = error.code;
    const title = code === "rate_limited"
      ? "更新服务请求被限流"
      : code === "network_unreachable"
        ? "无法连接更新服务"
        : code === "not_found"
          ? "未找到发布信息"
          : "更新检查失败";
    const hint = code === "rate_limited"
      ? `请求过于频繁${error.retryAfterSeconds ? `，请在 ${error.retryAfterSeconds} 秒后重试` : "，请稍后重试"}。`
      : code === "network_unreachable"
        ? "请检查网络连接后重试。"
        : error.message;
    const diagnostics = [
      error.status ? `HTTP ${error.status}` : null,
      error.host ? `主机: ${error.host}` : null,
    ].filter(Boolean).join(" · ");
    return <span className="msg err" role="alert">{title}：{hint}{diagnostics ? `（${diagnostics}）` : ""}</span>;
  };
  const nativeSyncSupported = platform === "windows";
  const navItems = [
    { key: "general", label: "常规" },
    { key: "webdav", label: "WebDAV" },
    ...(nativeSyncSupported ? [{ key: "nativesync", label: "NativeSync" } as const] : []),
    { key: "about", label: "关于" },
  ] as const;
  const dotClass = status.kind === "connected" ? "ok" : status.kind === "error" ? "down" : "mid";
  return <div className="layout">
    <aside className="side">
      <div className="brand">
        <div className="brand-name">Pandock</div>
        <div className={`brand-status ${dotClass}`}><span className="dot" /><span>{labels[status.kind] || status.kind}{status.reason ? " · " + status.reason : ""}</span></div>
        <div className="brand-addr">{"http://" + config.listen + config.dav_prefix + "/"}</div>
      </div>
      <nav className="nav" aria-label="设置导航">
        {navItems.map((item) => <button key={item.key} className={`nav-item ${page === item.key ? "active" : ""}`} onClick={() => setPage(item.key)}>{item.label}</button>)}
      </nav>
    </aside>
    <div className="content"><div className="page-body">
      {page !== "about" && <div className="page-head"><h2>{navItems.find((item) => item.key === page)?.label}</h2></div>}
      {page === "general" && <>
        <label className="switch general-autostart-row"><input type="checkbox" checked={autostart} disabled={busy.pending("set_autostart")} onChange={() => void toggleAutostart()} /> 开机启动（登录后启动并隐藏窗口）{busy.spinning("set_autostart") && <Spinner />}</label>
        <section className="tile">
          <div className="tile-header"><h2>百度网盘连接</h2><span className={`chip ${connected ? "chip-connected" : "chip-muted"}`}>{connected ? "已连接" : "未连接"}</span></div>
          <div className="tile-body">
            <p className="tile-description">连接百度开放平台应用目录，并映射到本地 WebDAV。</p>
            {connected
              ? <div className="actions start"><button className="secondary" disabled={busy.pending("auth_disconnect")} onClick={() => void disconnect()}>{busy.spinning("auth_disconnect") && <Spinner />}断开连接</button></div>
              : <div className="actions start"><button className="primary" onClick={openModal}>连接百度网盘</button></div>}
          </div>
        </section>
      </>}
      {page === "webdav" && <section className="tile">
        <div className="tile-header"><h2>WebDAV</h2><span className={`chip ${status.kind === "connected" ? "chip-connected" : "chip-muted"}`}>{labels[status.kind] || status.kind}</span></div>
        <div className="tile-body">
          <div className="form-grid">
            <label className="field">本地监听地址<input value={config.listen} onChange={(e) => setConfig((prev) => ({ ...prev, listen: e.target.value }))} /></label>
            <label className="field">路径<input value={config.dav_prefix} onChange={(e) => setConfig((prev) => ({ ...prev, dav_prefix: e.target.value }))} /></label>
          </div>
          <label className="switch"><input type="checkbox" checked={config.basic_auth} onChange={(e) => setConfig((prev) => ({ ...prev, basic_auth: e.target.checked }))} /> 启用 WebDAV 密码</label>
          {config.basic_auth && <div className="form-grid">
            <label className="field">WebDAV 用户名<input value={config.username} onChange={(e) => setConfig((prev) => ({ ...prev, username: e.target.value }))} /></label>
            <label className="field">WebDAV 密码（留空表示继续使用已保存密码）<input type="password" value={config.webdav_password} onChange={(e) => setConfig((prev) => ({ ...prev, webdav_password: e.target.value }))} /></label>
          </div>}
          <div className="url-panel">
            <p className="url">http://{config.listen}{config.dav_prefix}/</p>
            <p className="note">当前挂载范围：百度开放平台应用目录 /apps/{config.app_name || "<应用名称>"}</p>
          </div>
          <div className="actions start">
            <button className="secondary" onClick={() => void copyUrl()}>复制地址</button>
            {status.kind === "connected" ? <button className="secondary" disabled={busy.pending("webdav_stop")} onClick={() => void stopWebdav()}>{busy.spinning("webdav_stop") && <Spinner />}停止 WebDAV</button> : <button className="secondary" disabled={busy.pending("webdav_reconnect")} onClick={() => void reconnect()}>{busy.spinning("webdav_reconnect") && <Spinner />}启动/重连</button>}
          </div>
        </div>
      </section>}
      {page === "nativesync" && nativeSyncSupported && <NativeSyncSettings status={nativeSync} statusError={nativeSyncError} rootPath={nativeSyncRoot} setRootPath={setNativeSyncRoot} onRefresh={() => void refreshNativeSync()} onEnable={enableNativeSync} onDisable={disableNativeSync} onSyncNow={syncNativeNow} busy={busy} />}
      {page === "about" && <>
        <div className="page-head">
          <div>
            <h2>Pandock</h2>
            <p className="page-subtitle">百度网盘本地接入客户端</p>
          </div>
          <span className={`status-chip ${updateState.phase === "error" ? "error" : updateState.phase === "up_to_date" || updateState.phase === "update_available" ? "ok" : ""}`}>{phaseLabels[updateState.phase] || updateState.phase}</span>
        </div>
        <section className="about-hero card">
          <div className="about-mark">P</div>
          <div>
            <div className="about-title">Pandock</div>
            <div className="hint">轻量、本地、可审计的百度网盘同步与按需下载客户端</div>
          </div>
        </section>
        <div className="about-grid">
          <div className="card about-item"><span>当前版本</span><strong>{updateState.currentVersion}</strong></div>
          <div className="card about-item"><span>GitHub 项目</span><strong>weyham/pandock</strong></div>
          <div className="card about-item"><span>更新源</span><strong>GitHub Releases</strong></div>
        </div>
        <section className="card about-update">
          <div className="card-title">检查更新</div>
          <p className="hint">通过 GitHub Releases 检查新版本。安装版自动下载并安装；便携版就地替换，失败自动回滚。</p>
          <div className="about-actions">
            <button className="primary" disabled={updateBusy} onClick={() => void checkUpdate()}>{busy.pending("update_check") ? "检查中…" : "检查更新"}</button>
            {updateState.phase === "update_available" && <button className="secondary" disabled={updateBusy} onClick={() => void downloadUpdate()}>下载并校验</button>}
            {updateState.phase === "ready_to_install" && <button className="primary" disabled={updateBusy} onClick={() => void installUpdate()}>安装并重启</button>}
            {message && <span className="msg ok">{message}</span>}
            {updateState.phase === "up_to_date" && <span className="msg ok" role="status">当前版本已是最新。</span>}
            {renderUpdateError()}
          </div>
          {updateState.availableVersion && <p className="hint about-note">可用版本：{updateState.availableVersion}{updateState.manualOnly ? "（当前平台需手动下载）" : ""}</p>}
          {updateState.phase === "manual_download" && updateState.releaseUrl && <div className="about-actions"><button className="primary" onClick={() => void openExternal(updateState.releaseUrl)}>前往下载页面</button></div>}
        </section>
      </>}

      {(page === "general" || page === "webdav") && <footer className="settings-actions">
        {message && <p className="msg">{message}</p>}
        <button className="primary" disabled={!configDirty || busy.pending("save_config")} onClick={() => void save()}>{busy.spinning("save_config") && <Spinner />}保存设置</button>
      </footer>}
    </div></div>
    {modalOpen && <div className="modal-backdrop" role="presentation"><div className="modal" role="dialog" aria-modal="true" aria-labelledby="connect-title">
      <div className="modal-header"><h2 id="connect-title">连接百度网盘</h2><button className="icon-button" aria-label="关闭" onClick={() => void closeModal()}>×</button></div>
      {modalStep === "credentials" && <div className="modal-body"><label>App Key<input value={appKey} onChange={(e) => setAppKey(e.target.value)} /></label><label>Secret Key<input type="password" value={appSecret} onChange={(e) => setAppSecret(e.target.value)} /></label><label>应用名称<input value={appName} onChange={(e) => setAppName(e.target.value)} /></label>{modalError && <p className="connection-message">{modalError}</p>}<div className="actions"><button className="primary" onClick={() => void beginAuth()}>开始连接</button><button className="secondary" onClick={() => void closeModal()}>取消</button></div></div>}
      {modalStep === "requesting" && <div className="modal-body"><p>正在向百度网盘请求设备码...</p><div className="actions"><button className="secondary" onClick={() => void closeModal()}>取消</button></div></div>}
      {modalStep === "waiting" && challenge && <div className="modal-body"><p>请使用百度 App 扫码，或打开验证页面完成授权。</p>{challenge.qrCodeUrl ? <><img className="qrcode" src={challenge.qrCodeUrl} alt="百度网盘授权二维码" /><p><button className="link-button" onClick={() => void openExternal(challenge.qrCodeUrl)}>打开授权二维码</button></p></> : <p className="connection-message">二维码加载失败，请使用下方用户码或验证网址。</p>}<p>用户码：<strong>{challenge.userCode || "—"}</strong></p>{challenge.verificationUrl && <p><button className="link-button" onClick={() => void openExternal(challenge.verificationUrl)}>打开百度授权页面</button></p>}<p>完成授权后此窗口会自动关闭。</p><div className="actions"><button className="secondary" onClick={() => void closeModal()}>取消</button></div></div>}
      {modalStep === "error" && <div className="modal-body"><p className="connection-message">{modalError || "连接失败"}</p><div className="actions"><button className="primary" onClick={() => void retryAuth()}>重试</button><button className="secondary" onClick={() => setModalStep("credentials")}>修改凭据</button><button className="secondary" onClick={() => void closeModal()}>取消</button></div></div>}
    </div></div>}
  </div>;

}
