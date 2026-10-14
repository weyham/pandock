import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type NativeSyncStatus = {
  enabled: boolean; supported: boolean; supportReason?: string | null; rootPath?: string | null;
  state: string; totalEntries: number; placeholderEntries: number; hydratedEntries: number;
  attentionEntries: number; activeJobs: number;
  /** 后端契约是字符串形式的 epoch 秒；兼容旧夹具的数字形态。 */
  lastSyncAt?: string | number | null; lastError?: string | null;
  /** ADR-111 自动对账：ok / backing_off / paused / disabled。 */
  autoSyncState?: string; consecutiveFailures?: number;
  backoffUntil?: string | null; lastLayerAt?: string | null;
};
export type NativeSyncProgress = { completed: number; total: number };
export type NativeSyncEntry = {
  fsId: string; parentFsId?: string | null; name: string; relativePath: string; isDir: boolean;
  size: number; modifiedAt?: number | null;
  state: "online_only" | "hydrating" | "hydrated" | "error" | "remote_missing" | "stale" | string;
  pinState: "pinned" | "unpinned" | string; progress?: NativeSyncProgress | null; error?: string | null; hasChildren: boolean;
};
export type NativeSyncListResult = { entries: NativeSyncEntry[]; nextCursor?: string | null; total: number; parentPath?: string | null };
export type NativeSyncAction = "pin" | "unpin" | "dehydrate" | "retry" | "ack_remote_missing";

export function useDelayedBusy(delayMs = 500) {
  const [pendingKeys, setPendingKeys] = React.useState<ReadonlySet<string>>(new Set());
  const [visibleKeys, setVisibleKeys] = React.useState<ReadonlySet<string>>(new Set());
  const run = React.useCallback(async <T,>(key: string, task: () => Promise<T>): Promise<T> => {
    setPendingKeys((previous) => new Set(previous).add(key));
    const timer = window.setTimeout(() => setVisibleKeys((previous) => new Set(previous).add(key)), delayMs);
    try { return await task(); }
    finally {
      window.clearTimeout(timer);
      const remove = (previous: ReadonlySet<string>) => { const next = new Set(previous); next.delete(key); return next; };
      setPendingKeys(remove); setVisibleKeys(remove);
    }
  }, [delayMs]);
  return {
    run,
    pending: React.useCallback((key: string) => pendingKeys.has(key), [pendingKeys]),
    spinning: React.useCallback((key: string) => visibleKeys.has(key), [visibleKeys]),
  };
}

export function Spinner() { return <span className="spinner" role="progressbar" aria-label="加载中" />; }

const FolderIcon = () => (
  <svg className="native-sync-icon" width="14" height="14" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
    <path d="M1.5 3.5A1.5 1.5 0 0 1 3 2h3.2a1.5 1.5 0 0 1 1.1.5L8.5 4H13a1.5 1.5 0 0 1 1.5 1.5v6A1.5 1.5 0 0 1 13 13H3a1.5 1.5 0 0 1-1.5-1.5v-8z" fill="#d5b34c" />
  </svg>
);
const FileIcon = () => (
  <svg className="native-sync-icon" width="14" height="14" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
    <path d="M4 1.5h4.5L12 5v8.5a1.5 1.5 0 0 1-1.5 1.5h-6A1.5 1.5 0 0 1 3 13.5v-11A1.5 1.5 0 0 1 4.5 1z" fill="#8ea2b8" />
  </svg>
);
export function formatNativeSyncSize(size: number) {
  if (!Number.isFinite(size) || size < 1024) return `${Math.max(0, size || 0)} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = size; let unit = "B";
  for (const candidate of units) { value /= 1024; unit = candidate; if (value < 1024 || candidate === "TB") break; }
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${unit}`;
}
/** modifiedAt is Unix seconds from the backend contract. */
export function formatNativeSyncDate(value?: number | null) { return value ? new Date(value * 1000).toLocaleString() : "—"; }
export function formatNativeSyncProgress(progress?: NativeSyncProgress | null) {
  if (!progress || progress.total <= 0) return "下载中";
  const percent = Math.max(0, Math.min(100, Math.round((progress.completed / progress.total) * 100)));
  return `下载中 ${percent}%`;
}

const stateLabels: Record<string, string> = {
  online_only: "仅在线", hydrating: "下载中", hydrated: "已下载", error: "错误",
  remote_missing: "远端已删除，本地内容保留", stale: "远端有更新",
};
const stateClasses: Record<string, string> = {
  online_only: "native-sync-badge-online", hydrating: "native-sync-badge-progress",
  hydrated: "native-sync-badge-hydrated", error: "native-sync-badge-error",
  remote_missing: "native-sync-badge-missing", stale: "native-sync-badge-stale",
};
/** epoch 秒（string/number）→ HH:mm:ss；无效输入 ⇒ null。 */
export function formatSyncTimeOfDay(value?: string | number | null): string | null {
  if (value == null || value === "") return null;
  const secs = typeof value === "number" ? value : Number(value);
  if (!Number.isFinite(secs) || secs <= 0) return null;
  // 钉死 zh-CN：界面文案全中文，时间格式跟中文走；空 locale 数组跟随系统
  // （en-US 会输出 "10:13:20 AM"，中英混杂且 CI 依赖系统 locale 不稳定）。
  return new Date(secs * 1000).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

/** 「上次对账 HH:mm:ss · 结果」一行（ADR-111 §2.6）。paused 时同一行升级为非模态提示（绝不弹窗）。 */
export function autoSyncLine(status: NativeSyncStatus, autoEnabled: boolean): { tone: "note" | "warn"; text: string } | null {
  if (!status.enabled) return null;
  if (!autoEnabled) return { tone: "note", text: "自动对账已关闭 · 可手动「立即对账（诊断）」" };
  const at = formatSyncTimeOfDay(status.lastSyncAt) ?? "—";
  if (status.autoSyncState === "paused") {
    const reason = status.lastError ? `（最近失败：${status.lastError}）` : "";
    return { tone: "warn", text: `自动对账已暂停${reason} · 手动「立即对账」成功或授权恢复后自动恢复` };
  }
  if (status.autoSyncState === "backing_off") {
    const retry = formatSyncTimeOfDay(status.backoffUntil);
    const times = status.consecutiveFailures ? `（连续失败 ${status.consecutiveFailures} 次）` : "";
    return { tone: "warn", text: `上次对账 ${at} · 失败${times}${retry ? `，将于 ${retry} 重试` : ""}` };
  }
  return { tone: "note", text: `上次对账 ${at} · 正常` };
}

export function NativeSyncBadge({ entry }: { entry: NativeSyncEntry }) {
  const label = stateLabels[entry.state] || entry.state;
  return <span className={`native-sync-badge ${stateClasses[entry.state] || "native-sync-badge-online"}`}>
    {entry.state === "hydrating" && entry.progress != null ? formatNativeSyncProgress(entry.progress) : label}
  </span>;
}

export function syncStateLabel(status: NativeSyncStatus | null) {
  if (!status) return "加载中";
  if (!status.supported) return "系统不支持";
  if (!status.enabled) return "未启用";
  if (status.state === "syncing") return "同步中";
  if (status.state === "error") return "需要处理";
  return "已启用";
}

export function NativeSyncSettings({
  status, statusError, rootPath, setRootPath, onRefresh, onEnable, onDisable, onSyncNow, onOpenRoot, busy,
  autoSyncEnabled, onToggleAutoSync,
}: {
  status: NativeSyncStatus | null; statusError: string; rootPath: string; setRootPath: (value: string) => void;
  onRefresh: () => void; onEnable: () => void; onDisable: () => void; onSyncNow: () => void; onOpenRoot: () => void;
  busy: ReturnType<typeof useDelayedBusy>;
  /** `native_sync_auto` 配置（ADR-111 §2.5 回滚开关）；切换即保存，无需按「保存设置」。 */
  autoSyncEnabled: boolean; onToggleAutoSync: (next: boolean) => void;
}) {
  const supported = status?.supported ?? true;
  const enabled = status?.enabled ?? false;
  return <section className="tile native-sync-settings" aria-label="NativeSync 同步设置">
    <div className="tile-header">
      <div><h2>NativeSync 同步</h2><p className="tile-subtitle">按需下载云端文件，不替代本地文件管理器</p></div>
      {status && <span className={`chip ${enabled ? "chip-connected" : supported ? "chip-muted" : "chip-error"}`}>{syncStateLabel(status)}</span>}
    </div>
    <div className="tile-body">
      {statusError && <div className="connection-message" role="alert">无法读取 NativeSync 状态：{statusError}</div>}
      {!status && !statusError && <p className="note native-sync-loading-note">正在读取 NativeSync 状态…</p>}
      {status && !supported && <div className="error-banner native-sync-inline-error" role="alert"><strong>当前系统不支持 NativeSync</strong><p>{status.supportReason || "需要 Windows 1903 或更高版本。"}</p></div>}
      {supported && <>
        {statusError && <p className="note">后端状态暂不可用；可先填写同步根目录，操作会在后端恢复后生效。</p>}
        <label className="field native-sync-root-field">同步根目录<input value={enabled ? (status?.rootPath || rootPath) : rootPath} disabled={enabled || busy.pending("native_sync_enable")} placeholder="留空使用系统文档目录下的 Pandock（推荐）" onChange={(event) => setRootPath(event.target.value)} /></label>
        {status?.enabled && <div className="native-sync-settings-stats"><span>占位符 {status.placeholderEntries}</span><span>已下载 {status.hydratedEntries}</span><span>需处理 {status.attentionEntries}</span></div>}
        {status?.enabled && (() => { const line = autoSyncLine(status, autoSyncEnabled); return line
          ? <p className={`note native-sync-autoline${line.tone === "warn" ? " native-sync-autoline-warn" : ""}`} role="status">{line.text}</p>
          : null; })()}
        {status?.enabled && <label className="switch native-sync-auto-toggle">
          <input type="checkbox" checked={autoSyncEnabled} disabled={busy.pending("save_config")} onChange={(event) => onToggleAutoSync(event.target.checked)} />
          自动同步云端变化（约每 10 分钟对账一次，失败自动退避）
        </label>}
        <div className="actions start native-sync-actions">
          {!enabled
            ? <button className="primary" disabled={busy.pending("native_sync_enable")} onClick={onEnable}>{busy.spinning("native_sync_enable") && <Spinner />}启用同步</button>
            : <button className="secondary" disabled={busy.pending("native_sync_disable")} onClick={onDisable}>{busy.spinning("native_sync_disable") && <Spinner />}停止同步</button>}
          <button className="secondary" disabled={!enabled || busy.pending("native_sync_sync_now")} onClick={onSyncNow} title="手动触发一次全量对账（诊断/应急用途；云端变化平时会自动跟上）">{busy.spinning("native_sync_sync_now") && <Spinner />}立即对账（诊断）</button>
          <button className="link-button native-sync-refresh" disabled={busy.pending("native_sync_status")} onClick={onRefresh}>{busy.spinning("native_sync_status") && <Spinner />}刷新状态</button>
          {enabled && <button className="secondary" disabled={busy.pending("native_sync_open_root")} onClick={onOpenRoot}>{busy.spinning("native_sync_open_root") && <Spinner />}打开根目录</button>}
        </div>
        {enabled && <NativeSyncEntryList busy={busy} />}
      </>}
    </div>
  </section>;
}

/** 同步条目列表（设置页内的操作面）。
 *
 * 默认视图 = 「可操作的条目」：全树扁平、显示相对路径，只收**已下载 / 已固定 / 需处理 / 本地新建**
 *（由后端 `native_sync_list` 的 `filter: "actionable"` 给出）——用户来这里是要操作，不是浏览。
 * 「按目录浏览全部」可下钻到某个目录（顶部一行路径 + 「返回」），数据走现有 `native_sync_list(parentPath)`。
 * 不做目录树 / 面包屑 / 地址栏 / 双栏 / 大图标网格——那是退役的独立窗口的职责。 */
export function NativeSyncEntryList({ busy }: { busy: ReturnType<typeof useDelayedBusy> }) {
  const [mode, setMode] = React.useState<"actionable" | "browse">("actionable");
  const [browsePath, setBrowsePath] = React.useState("");
  const [entries, setEntries] = React.useState<NativeSyncEntry[]>([]);
  const [nextCursor, setNextCursor] = React.useState<string | null>(null);
  const [loading, setLoading] = React.useState(true);
  const [listError, setListError] = React.useState("");
  const [actionError, setActionError] = React.useState("");
  const [acknowledgedMissing, setAcknowledgedMissing] = React.useState<ReadonlySet<string>>(new Set());
  const [selectedPath, setSelectedPath] = React.useState<string | null>(null);

  /** 拉一页；`append=true` 时按 relativePath 去重合并（分页追加）。
   *  注意：本地新建条目没有云端 fsId ⇒ 一律按 relativePath 键合（它总是存在且在视图内唯一）。 */
  const loadPage = React.useCallback(async (cursor?: string | null, append = false) => {
    const args = mode === "actionable"
      ? { parentPath: null as string | null, cursor: cursor || null, limit: 50, filter: "actionable" as string | null }
      : { parentPath: browsePath || null, cursor: cursor || null, limit: 50, filter: null as string | null };
    const result = await invoke<NativeSyncListResult>("native_sync_list", args);
    setEntries((previous) => {
      if (!append) return result.entries || [];
      const merged = new Map(previous.map((entry) => [entry.relativePath, entry]));
      for (const entry of result.entries || []) merged.set(entry.relativePath, entry);
      return [...merged.values()];
    });
    setNextCursor(result.nextCursor || null);
  }, [mode, browsePath]);

  const refresh = React.useCallback(async () => {
    setListError("");
    try { await loadPage(null, false); } catch (error) { setListError(String(error)); }
    setLoading(false);
  }, [loadPage]);

  React.useEffect(() => {
    setLoading(true);
    setSelectedPath(null);
    void refresh();
  }, [refresh]);
  // 状态变化（同步完成 / 水合终态等）→ 重拉当前视图的第一页（与退役窗口同语义）。
  React.useEffect(() => {
    const unlisten = listen<NativeSyncStatus>("pandock://nativesync/state", () => {
      void loadPage(null, false).catch(() => undefined);
    });
    return () => { void unlisten.then((off) => off()); };
  }, [loadPage]);

  const browseTo = (path: string) => { setMode("browse"); setBrowsePath(path); };
  const browseBack = () => {
    if (browsePath === "") { backToActionable(); return; }
    setBrowsePath(browsePath.split("/").slice(0, -1).join("/"));
  };
  const backToActionable = () => { setMode("actionable"); setBrowsePath(""); };

  const runAction = async (entry: NativeSyncEntry, action: NativeSyncAction) => {
    const key = `native_sync_action_${entry.relativePath}_${action}`;
    setActionError("");
    try {
      const result = await busy.run(key, () => invoke<NativeSyncEntry | NativeSyncStatus>("native_sync_action", { relativePath: entry.relativePath, action }));
      if (action === "ack_remote_missing") {
        setAcknowledgedMissing((previous) => new Set(previous).add(entry.relativePath));
      }
      if (result && "fsId" in result && action !== "ack_remote_missing") {
        setEntries((previous) => previous.map((item) => (item.relativePath === entry.relativePath ? { ...item, ...result } : item)));
      }
      await loadPage(null, false);
    } catch (error) { setActionError(String(error)); }
  };
  const openInExplorer = async (relativePath: string) => {
    const key = `native_sync_open_explorer_${relativePath}`;
    setActionError("");
    try { await busy.run(key, () => invoke("native_sync_open_in_explorer", { relativePath })); }
    catch (error) { setActionError(String(error)); }
  };
  const loadMore = async () => {
    if (!nextCursor) return;
    setListError("");
    try { await busy.run("native_sync_load_more", () => loadPage(nextCursor, true)); }
    catch (error) { setListError(String(error)); }
  };

  const selected = selectedPath ? entries.find((entry) => entry.relativePath === selectedPath) ?? null : null;
  const actionKey = (action: string) => (selected ? `native_sync_action_${selected.relativePath}_${action}` : "");

  return <section className="native-sync-entrylist" aria-label="同步条目">
    <div className="native-sync-entrylist-header">
      {mode === "actionable"
        ? <span className="note">可操作的条目（已下载 / 已固定 / 需处理）</span>
        : <span className="note">浏览：{browsePath || "同步根目录"}</span>}
      {mode === "actionable"
        ? <button type="button" className="link-button native-sync-entrylist-browse" onClick={() => browseTo("")}>按目录浏览全部</button>
        : <button type="button" className="link-button native-sync-entrylist-back" onClick={browseBack}>{browsePath === "" ? "返回可操作列表" : "返回上一级"}</button>}
    </div>
    {actionError && <div className="error-banner native-sync-inline-error" role="alert"><strong>操作失败</strong><p>{actionError}</p></div>}
    {listError && <div className="native-sync-entrylist-error" role="alert"><strong>无法加载同步条目</strong><p>{listError}</p><button className="secondary" onClick={() => void refresh()}>重试</button></div>}
    {loading && <div className="native-sync-entrylist-loading" role="status"><Spinner /><p className="note">正在加载同步条目…</p></div>}
    {!loading && !listError && entries.length === 0 && (
      <p className="note native-sync-entrylist-empty">{mode === "actionable" ? "当前没有可操作的条目（云端文件会先出现在资源管理器里）。" : "此目录为空。"}</p>
    )}
    {!loading && !listError && entries.length > 0 && <ul className="native-sync-entry-rows" data-testid="entry-rows">
      {entries.map((entry) => {
        const isSelected = entry.relativePath === selectedPath;
        return <li key={entry.relativePath}>
          <button type="button"
            className={`native-sync-entry-row ${isSelected ? "selected" : ""}`}
            title={entry.error || entry.name}
            onClick={() => {
              if (mode === "browse" && entry.isDir) { browseTo(entry.relativePath); return; }
              setSelectedPath(isSelected ? null : entry.relativePath);
            }}>
            <span className="native-sync-entry-row-icon">{entry.isDir ? <FolderIcon /> : <FileIcon />}</span>
            <span className="native-sync-entry-row-main">
              <span className="native-sync-entry-row-name">{entry.name}</span>
              {entry.relativePath !== entry.name && <span className="native-sync-entry-row-path">{entry.relativePath}</span>}
            </span>
            <NativeSyncBadge entry={entry} />
            <span className="native-sync-entry-row-meta">{entry.isDir ? "目录" : formatNativeSyncSize(entry.size)}{entry.modifiedAt ? ` · ${formatNativeSyncDate(entry.modifiedAt)}` : ""}</span>
          </button>
          {isSelected && !loading && <div className="native-sync-entry-actions" aria-label="选中条目操作">
            <span className="native-sync-entry-actions-name">{entry.isDir ? <FolderIcon /> : <FileIcon />}{entry.name}</span>
            <div className="native-sync-entry-actions-buttons">
              {!entry.isDir && entry.pinState !== "pinned" && <button className="secondary" title="始终保留在此设备上，不会被释放空间" disabled={busy.pending(actionKey("pin"))} onClick={() => void runAction(entry, "pin")}>{busy.spinning(actionKey("pin")) && <Spinner />}固定到本地</button>}
              {!entry.isDir && entry.pinState === "pinned" && <button className="secondary" title="恢复为按需下载" disabled={busy.pending(actionKey("unpin"))} onClick={() => void runAction(entry, "unpin")}>{busy.spinning(actionKey("unpin")) && <Spinner />}取消固定</button>}
              {!entry.isDir && entry.state === "hydrated" && entry.pinState !== "pinned" && <button className="secondary" disabled={busy.pending(actionKey("dehydrate"))} onClick={() => void runAction(entry, "dehydrate")}>{busy.spinning(actionKey("dehydrate")) && <Spinner />}释放空间</button>}
              {entry.state === "error" && <button className="secondary" disabled={busy.pending(actionKey("retry"))} onClick={() => void runAction(entry, "retry")}>{busy.spinning(actionKey("retry")) && <Spinner />}重试</button>}
              {entry.state === "remote_missing" && !acknowledgedMissing.has(entry.relativePath) && <button className="secondary" disabled={busy.pending(actionKey("ack_remote_missing"))} onClick={() => void runAction(entry, "ack_remote_missing")}>{busy.spinning(actionKey("ack_remote_missing")) && <Spinner />}已知悉</button>}
              <button className="secondary" disabled={busy.pending(`native_sync_open_explorer_${entry.relativePath}`)} onClick={() => void openInExplorer(entry.relativePath)}>{busy.spinning(`native_sync_open_explorer_${entry.relativePath}`) && <Spinner />}打开所在位置</button>
            </div>
          </div>}
        </li>;
      })}
    </ul>}
    {nextCursor && <div className="native-sync-entrylist-more"><button className="secondary" disabled={busy.pending("native_sync_load_more")} onClick={() => void loadMore()}>{busy.spinning("native_sync_load_more") && <Spinner />}加载更多</button></div>}
  </section>;
}
