import React from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type NativeSyncStatus = {
  enabled: boolean; supported: boolean; supportReason?: string | null; rootPath?: string | null;
  state: string; totalEntries: number; placeholderEntries: number; hydratedEntries: number;
  attentionEntries: number; activeJobs: number; lastSyncAt?: number | null; lastError?: string | null;
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
const FolderIconLarge = () => (
  <svg className="native-sync-icon-lg" width="44" height="44" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
    <path d="M1.5 3.5A1.5 1.5 0 0 1 3 2h3.2a1.5 1.5 0 0 1 1.1.5L8.5 4H13a1.5 1.5 0 0 1 1.5 1.5v6A1.5 1.5 0 0 1 13 13H3a1.5 1.5 0 0 1-1.5-1.5v-8z" fill="#d5b34c" />
  </svg>
);
const FileIconLarge = () => (
  <svg className="native-sync-icon-lg" width="40" height="44" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
    <path d="M4 1.5h4.5L12 5v8.5a1.5 1.5 0 0 1-1.5 1.5h-6A1.5 1.5 0 0 1 3 13.5v-11A1.5 1.5 0 0 1 4.5 1z" fill="#8ea2b8" />
  </svg>
);

const shortStateLabels: Record<string, string> = {
  online_only: "仅在线", hydrating: "下载中", hydrated: "已下载", error: "错误", remote_missing: "远端已删", stale: "有更新",
};
function shortStateLabel(entry: NativeSyncEntry) {
  if (entry.state === "hydrating" && entry.progress != null) return formatNativeSyncProgress(entry.progress);
  return shortStateLabels[entry.state] || entry.state;
}

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
  status, statusError, rootPath, setRootPath, onRefresh, onEnable, onDisable, onSyncNow, busy,
}: {
  status: NativeSyncStatus | null; statusError: string; rootPath: string; setRootPath: (value: string) => void;
  onRefresh: () => void; onEnable: () => void; onDisable: () => void; onSyncNow: () => void;
  busy: ReturnType<typeof useDelayedBusy>;
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
        <div className="actions start native-sync-actions">
          {!enabled
            ? <button className="primary" disabled={busy.pending("native_sync_enable")} onClick={onEnable}>{busy.spinning("native_sync_enable") && <Spinner />}启用同步</button>
            : <button className="secondary" disabled={busy.pending("native_sync_disable")} onClick={onDisable}>{busy.spinning("native_sync_disable") && <Spinner />}停止同步</button>}
          <button className="secondary" disabled={!enabled || busy.pending("native_sync_sync_now")} onClick={onSyncNow}>{busy.spinning("native_sync_sync_now") && <Spinner />}立即同步</button>
          <button className="link-button native-sync-refresh" disabled={busy.pending("native_sync_status")} onClick={onRefresh}>{busy.spinning("native_sync_status") && <Spinner />}刷新状态</button>
        </div>
      </>}
    </div>
  </section>;
}

type TreeNodeState = { dirs: NativeSyncEntry[] | null; expanded: boolean; loading: boolean; error: string | null };

export function ManagerApp() {
  const [status, setStatus] = React.useState<NativeSyncStatus | null>(null);
  const [statusError, setStatusError] = React.useState("");
  const [entries, setEntries] = React.useState<NativeSyncEntry[]>([]);
  const [nextCursor, setNextCursor] = React.useState<string | null>(null);
  const [currentPath, setCurrentPath] = React.useState("");
  const [loading, setLoading] = React.useState(true);
  const [listError, setListError] = React.useState("");
  const [actionError, setActionError] = React.useState("");
  const [acknowledgedMissing, setAcknowledgedMissing] = React.useState<ReadonlySet<string>>(new Set());
  const [selectedFsId, setSelectedFsId] = React.useState<string | null>(null);
  const [treeNodes, setTreeNodes] = React.useState<Record<string, TreeNodeState>>({});
  const statusRef = React.useRef<NativeSyncStatus | null>(null);

  /** 后端 db.root() 失败时 status() 会返回 enabled=false 的错误默认值（manager.rs unwrap_or），
   *  保留上一次可信状态，避免整个浏览区被瞬时拆除导致目录树/节点消失。 */
  const applyStatus = React.useCallback((next: NativeSyncStatus) => {
    const previous = statusRef.current;
    const looksLikeErrorDefault = !next.enabled && next.state === "disabled" && !next.rootPath && !next.lastError;
    if (previous?.enabled && looksLikeErrorDefault) return;
    statusRef.current = next;
    setStatus(next);
  }, []);
  const busy = useDelayedBusy();

  const loadStatus = React.useCallback(async () => {
    try { applyStatus(await invoke<NativeSyncStatus>("native_sync_status")); setStatusError(""); }
    catch (error) { setStatusError(String(error)); throw error; }
  }, []);
  const loadEntries = React.useCallback(async (parentPath: string, cursor?: string | null, append = false) => {
    const result = await invoke<NativeSyncListResult>("native_sync_list", { parentPath: parentPath || null, cursor: cursor || null, limit: 50 });
    setEntries((previous) => {
      if (!append) return result.entries || [];
      const merged = new Map(previous.map((entry) => [entry.fsId, entry]));
      for (const entry of result.entries || []) merged.set(entry.fsId, entry);
      return [...merged.values()];
    });
    setNextCursor(result.nextCursor || null);
  }, []);
  const loadTreeChildren = React.useCallback(async (path: string) => {
    try {
      const result = await invoke<NativeSyncListResult>("native_sync_list", { parentPath: path || null, cursor: null, limit: 50 });
      setTreeNodes((previous) => {
        const current = previous[path] ?? { dirs: null, expanded: path === "", loading: false, error: null };
        return { ...previous, [path]: { ...current, dirs: (result.entries || []).filter((entry) => entry.isDir), loading: false, error: null } };
      });
    } catch (error) {
      setTreeNodes((previous) => {
        const current = previous[path] ?? { dirs: null, expanded: false, loading: false, error: null };
        return { ...previous, [path]: { ...current, loading: false, error: String(error) } };
      });
    }
  }, []);
  const refresh = React.useCallback(async () => {
    setListError("");
    const outcomes = await Promise.allSettled([loadStatus(), loadEntries(currentPath)]);
    const failure = outcomes.find((outcome): outcome is PromiseRejectedResult => outcome.status === "rejected");
    if (failure) setListError(String(failure.reason));
    setLoading(false);
  }, [currentPath, loadEntries, loadStatus]);
  React.useEffect(() => { setLoading(true); void refresh(); }, [refresh]);
  React.useEffect(() => { void loadTreeChildren(""); }, [loadTreeChildren]);
  React.useEffect(() => {
    const unlisten = listen<NativeSyncStatus>("pandock://nativesync/state", (event) => {
      applyStatus(event.payload);
      void loadEntries(currentPath).catch(() => undefined);
    });
    return () => { void unlisten.then((off) => off()); };
  }, [currentPath, loadEntries]);

  const openPath = (path: string) => { if (path === currentPath) return; setCurrentPath(path); setSelectedFsId(null); setLoading(true); setListError(""); };
  const toggleTreeNode = (path: string) => {
    const node = treeNodes[path];
    if (!node || (node.dirs === null && !node.loading)) {
      setTreeNodes((previous) => ({ ...previous, [path]: { dirs: null, expanded: true, loading: true, error: null } }));
      void loadTreeChildren(path);
      return;
    }
    if (node.loading) return;
    setTreeNodes((previous) => ({ ...previous, [path]: { ...node, expanded: !node.expanded } }));
  };
  const runAction = async (entry: NativeSyncEntry, action: NativeSyncAction) => {
    const key = `native_sync_action_${entry.fsId}_${action}`;
    setActionError("");
    try {
      const result = await busy.run(key, () => invoke<NativeSyncEntry | NativeSyncStatus>("native_sync_action", { relativePath: entry.relativePath, action }));
      if (action === "ack_remote_missing") setAcknowledgedMissing((previous) => new Set(previous).add(entry.fsId));
      if (result && "fsId" in result) {
        setEntries((previous) => previous.map((item) => {
          if (item.fsId !== entry.fsId) return item;
          if (action === "ack_remote_missing") return item;
          return { ...item, ...result };
        }));
      } else if (result && "enabled" in result) applyStatus(result);
      await loadStatus();
      await loadEntries(currentPath);
    } catch (error) { setActionError(String(error)); }
  };
  const openInExplorer = async (relativePath: string) => {
    const key = `native_sync_open_explorer_${relativePath}`;
    try { await busy.run(key, () => invoke("native_sync_open_in_explorer", { relativePath })); }
    catch (error) { setActionError(String(error)); }
  };
  const syncNow = async () => {
    setActionError("");
    try { applyStatus(await busy.run("native_sync_sync_now", () => invoke<NativeSyncStatus>("native_sync_sync_now"))); }
    catch (error) { setActionError(String(error)); }
  };
  const openRoot = async () => { await openInExplorer(""); };
  const refreshNow = async () => {
    setLoading(true);
    try { await busy.run("native_sync_refresh", async () => { await refresh(); await loadTreeChildren(""); }); }
    catch (error) { setListError(String(error)); setLoading(false); }
  };
  const loadMore = async () => {
    if (!nextCursor) return;
    setListError("");
    try { await busy.run("native_sync_load_more", () => loadEntries(currentPath, nextCursor, true)); }
    catch (error) { setListError(String(error)); }
  };

  const breadcrumbs = currentPath ? currentPath.split("/").filter(Boolean) : [];
  const unsupported = status?.supported === false;
  const disabled = status?.supported === true && !status.enabled;
  const selected = selectedFsId ? entries.find((entry) => entry.fsId === selectedFsId) ?? null : null;

  const renderTreeNode = (path: string, name: string, depth: number): React.ReactNode => {
    const node = treeNodes[path] ?? { dirs: null, expanded: path === "", loading: false, error: null };
    const expandable = node.dirs === null ? true : node.dirs.length > 0;
    return <React.Fragment key={path || "root"}>
      <div className={`native-sync-tree-row ${currentPath === path ? "current" : ""}`} style={{ paddingLeft: 8 + depth * 14 }}>
        <button
          type="button"
          className="native-sync-tree-toggle"
          aria-label={node.expanded ? `收起 ${name}` : `展开 ${name}`}
          disabled={!expandable}
          onClick={() => toggleTreeNode(path)}
        >{node.loading ? <Spinner /> : node.expanded ? "▾" : "▸"}</button>
        <button type="button" className="native-sync-tree-label" onClick={() => openPath(path)}>
          <FolderIcon />
          <span>{name}</span>
        </button>
      </div>
      {node.error && <div className="native-sync-tree-error" style={{ paddingLeft: 34 + depth * 14 }}>{node.error}</div>}
      {node.expanded && node.dirs?.map((dir) => renderTreeNode(dir.relativePath, dir.name, depth + 1))}
    </React.Fragment>;
  };

  return <main className="native-sync-manager" data-testid="native-sync-manager">
    <header className="native-sync-manager-header">
      <div><p className="eyebrow">Pandock NativeSync</p><h1>同步资源浏览</h1><p className="status-line">只管理同步状态与按需下载，文件打开交给资源管理器。</p></div>
      <div className="native-sync-header-actions">
        {status && <span className={`chip ${status.enabled ? "chip-connected" : status.supported ? "chip-muted" : "chip-error"}`}>{syncStateLabel(status)}</span>}
        <button className="secondary" disabled={busy.pending("native_sync_refresh")} onClick={() => void refreshNow()}>{busy.spinning("native_sync_refresh") && <Spinner />}刷新</button>
        <button className="secondary" disabled={!status?.enabled || busy.pending("native_sync_sync_now")} onClick={() => void syncNow()}>{busy.spinning("native_sync_sync_now") && <Spinner />}立即同步</button>
        <button className="secondary" disabled={!status?.enabled || busy.pending("native_sync_open_root")} onClick={() => void openRoot()}>{busy.spinning("native_sync_open_root") && <Spinner />}打开根目录</button>
      </div>
    </header>
    {status && <p className="native-sync-oneline">
      同步根 {status.rootPath || "未设置"} · 占位符 {status.placeholderEntries} · 已下载 {status.hydratedEntries} · 需处理 {status.attentionEntries}
      {status.lastSyncAt ? ` · 上次同步 ${formatNativeSyncDate(status.lastSyncAt)}` : ""}{status.activeJobs ? ` · ${status.activeJobs} 个任务进行中` : ""}
    </p>}
    {statusError && !status && !loading && <div className="native-sync-manager-state" role="alert"><h2>无法读取 NativeSync 状态</h2><p>{statusError}</p><button onClick={() => void refreshNow()}>重试</button></div>}
    {unsupported && <div className="native-sync-manager-state" role="alert"><h2>当前系统不支持 NativeSync</h2><p>{status?.supportReason || "需要 Windows 1903 或更高版本。"}</p></div>}
    {disabled && <div className="native-sync-manager-state"><h2>NativeSync 尚未启用</h2><p>请先在 Pandock 设置中选择同步根目录并启用同步。</p></div>}
    {!loading && !status && !statusError && <div className="native-sync-manager-state" role="status"><Spinner /><p>正在加载 NativeSync 状态…</p></div>}
    {!unsupported && !disabled && status?.enabled && <>
      <nav className="native-sync-addressbar" aria-label="地址栏">
        <button type="button" className={`native-sync-crumb ${!currentPath ? "current" : ""}`} onClick={() => openPath("")}>同步根目录</button>
        {breadcrumbs.map((part, index) => {
          const path = breadcrumbs.slice(0, index + 1).join("/");
          const last = index === breadcrumbs.length - 1;
          return <React.Fragment key={path}>
            <span className="native-sync-crumb-sep" aria-hidden="true">/</span>
            <button type="button" className={`native-sync-crumb ${last ? "current" : ""}`} onClick={() => openPath(path)}>{part}</button>
          </React.Fragment>;
        })}
      </nav>
      <section className="native-sync-browser">
        <aside className="native-sync-sidebar" aria-label="目录树">
          {renderTreeNode("", "同步根目录", 0)}
        </aside>
        <div className="native-sync-content">
          <div className="native-sync-content-header"><div><h2>{currentPath || "同步根目录"}</h2><p className="note">每页最多 50 项</p></div><span className="note">共 {status.totalEntries} 项</span></div>
          {actionError && <div className="error-banner native-sync-inline-error" role="alert"><strong>操作失败</strong><p>{actionError}</p></div>}
          {status.lastError && <div className="error-banner native-sync-inline-error" role="alert"><strong>同步需要处理</strong><p>{status.lastError}</p></div>}
          {loading && <div className="native-sync-manager-state" role="status"><Spinner /><p>正在加载同步目录…</p></div>}
          {!loading && listError && <div className="native-sync-manager-state" role="alert"><h2>无法加载同步目录</h2><p>{listError}</p><button onClick={() => void refreshNow()}>重试</button></div>}
          {!loading && !listError && entries.length === 0 && <div className="native-sync-manager-state"><h2>此目录为空</h2><p>同步目录中暂时没有可显示的项目。</p></div>}
          {!loading && !listError && entries.length > 0 && <>
            <div className="native-sync-grid" data-testid="entry-grid" aria-label="同步条目">
              {entries.map((entry) => <button type="button" key={entry.fsId} aria-label={entry.name} title={entry.error || entry.name}
                className={`native-sync-cell ${selectedFsId === entry.fsId ? "selected" : ""}`}
                onClick={() => setSelectedFsId(entry.fsId)}
                onDoubleClick={() => entry.isDir ? openPath(entry.relativePath) : void openInExplorer(entry.relativePath)}>
                <span className="native-sync-cell-icon">{entry.isDir ? <FolderIconLarge /> : <FileIconLarge />}
                  <span className={`native-sync-cell-badge ${stateClasses[entry.state] || "native-sync-badge-online"}`}>{shortStateLabel(entry)}</span>
                </span>
                <span className="native-sync-cell-name">{entry.name}</span>
              </button>)}
            </div>
            {nextCursor && <div className="native-sync-load-more"><button className="secondary" disabled={busy.pending("native_sync_load_more")} onClick={() => void loadMore()}>{busy.spinning("native_sync_load_more") && <Spinner />}加载更多</button></div>}
          </>}
        </div>
      </section>
    </>}
    {selected && !loading && <div className="native-sync-actionbar" aria-label="选中条目操作">
      <span className="native-sync-actionbar-name">{selected.isDir ? <FolderIcon /> : <FileIcon />}{selected.name}</span>
      <div className="native-sync-actionbar-actions">
        {!selected.isDir && selected.pinState !== "pinned" && <button className="secondary" title="始终保留在此设备上，不会被释放空间" disabled={busy.pending(`native_sync_action_${selected.fsId}_pin`)} onClick={() => void runAction(selected, "pin")}>{busy.spinning(`native_sync_action_${selected.fsId}_pin`) && <Spinner />}固定到本地</button>}
        {!selected.isDir && selected.pinState === "pinned" && <button className="secondary" title="恢复为按需下载" disabled={busy.pending(`native_sync_action_${selected.fsId}_unpin`)} onClick={() => void runAction(selected, "unpin")}>{busy.spinning(`native_sync_action_${selected.fsId}_unpin`) && <Spinner />}取消固定</button>}
        {!selected.isDir && selected.state === "hydrated" && selected.pinState !== "pinned" && <button className="secondary" disabled={busy.pending(`native_sync_action_${selected.fsId}_dehydrate`)} onClick={() => void runAction(selected, "dehydrate")}>{busy.spinning(`native_sync_action_${selected.fsId}_dehydrate`) && <Spinner />}释放空间</button>}
        {selected.state === "error" && <button className="secondary" disabled={busy.pending(`native_sync_action_${selected.fsId}_retry`)} onClick={() => void runAction(selected, "retry")}>{busy.spinning(`native_sync_action_${selected.fsId}_retry`) && <Spinner />}重试</button>}
        {selected.state === "remote_missing" && !acknowledgedMissing.has(selected.fsId) && <button className="secondary" disabled={busy.pending(`native_sync_action_${selected.fsId}_ack_remote_missing`)} onClick={() => void runAction(selected, "ack_remote_missing")}>{busy.spinning(`native_sync_action_${selected.fsId}_ack_remote_missing`) && <Spinner />}已知悉</button>}
        <button className="secondary" disabled={busy.pending(`native_sync_open_explorer_${selected.relativePath}`)} onClick={() => void openInExplorer(selected.relativePath)}>{busy.spinning(`native_sync_open_explorer_${selected.relativePath}`) && <Spinner />}在资源管理器中打开</button>
        <button className="link-button" onClick={() => setSelectedFsId(null)}>取消选中</button>
      </div>
    </div>}
  </main>;
}
