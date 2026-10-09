import React from "react";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ManagerApp, formatNativeSyncDate, formatNativeSyncProgress, type NativeSyncEntry, type NativeSyncStatus } from "./NativeSync";
import { selectWindowMode } from "./windowMode";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));
let nativeSyncStateListener: ((event: { payload: NativeSyncStatus }) => void) | undefined;
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (_name: string, cb: (event: { payload: NativeSyncStatus }) => void) => { nativeSyncStateListener = cb; return () => undefined; }) }));

const enabledStatus: NativeSyncStatus = {
  enabled: true, supported: true, rootPath: "C:\\Pandock\\NativeSync", state: "idle",
  totalEntries: 8, placeholderEntries: 3, hydratedEntries: 3, attentionEntries: 1, activeJobs: 0,
  lastSyncAt: 1727000000, lastError: null,
};

const entries: NativeSyncEntry[] = [
  { fsId: "1", name: "online.txt", relativePath: "online.txt", isDir: false, size: 512, state: "online_only", pinState: "unpinned", hasChildren: false },
  { fsId: "2", name: "downloading.zip", relativePath: "downloading.zip", isDir: false, size: 2048, state: "hydrating", pinState: "unpinned", progress: { completed: 42, total: 100 }, hasChildren: false },
  { fsId: "3", name: "ready.pdf", relativePath: "ready.pdf", isDir: false, size: 20480, state: "hydrated", pinState: "pinned", modifiedAt: 1727000000, hasChildren: false },
  { fsId: "4", name: "broken.bin", relativePath: "broken.bin", isDir: false, size: 0, state: "error", pinState: "unpinned", error: "下载失败", hasChildren: false },
  { fsId: "5", name: "deleted.doc", relativePath: "deleted.doc", isDir: false, size: 4096, state: "remote_missing", pinState: "pinned", hasChildren: false },
  { fsId: "6", name: "stale.csv", relativePath: "stale.csv", isDir: false, size: 1024, state: "stale", pinState: "unpinned", hasChildren: false },
  { fsId: "dir", name: "docs", relativePath: "docs", isDir: true, size: 0, state: "hydrated", pinState: "unpinned", hasChildren: true },
  { fsId: "7", name: "cached.tmp", relativePath: "cached.tmp", isDir: false, size: 8192, state: "hydrated", pinState: "unpinned", hasChildren: false },
];
const docsEntries: NativeSyncEntry[] = [
  { ...entries[0], fsId: "nested", name: "nested.txt", relativePath: "docs/nested.txt" },
  { fsId: "sub", name: "sub", relativePath: "docs/sub", isDir: true, size: 0, state: "hydrated", pinState: "unpinned", hasChildren: false },
];

function mockBackend({
  status = enabledStatus,
  list = { entries, nextCursor: null, total: entries.length, parentPath: "" },
}: { status?: NativeSyncStatus; list?: { entries: NativeSyncEntry[]; nextCursor?: string | null; total: number; parentPath?: string | null } } = {}) {
  invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case "native_sync_status": return status;
      case "native_sync_list":
        if (args?.parentPath === "docs") return { entries: docsEntries, nextCursor: null, total: docsEntries.length, parentPath: "docs" };
        return list;
      case "native_sync_action": {
        const entry = entries.find((item) => item.relativePath === args?.relativePath)!;
        if (args?.action === "ack_remote_missing") return { ...entry };
        if (args?.action === "unpin") return { ...entry, pinState: "unpinned" };
        if (args?.action === "dehydrate") return { ...entry, state: "online_only", pinState: "unpinned" };
        if (args?.action === "pin") return { ...entry, state: "hydrated", pinState: "pinned" };
        if (args?.action === "retry") return { ...entry, state: "hydrated" };
        return entry;
      }
      case "native_sync_open_in_explorer": return undefined;
      case "native_sync_sync_now": return status;
      default: return undefined;
    }
  });
}

const listTable = () => within(screen.getByTestId("entry-grid"));

describe("NativeSync manager", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    nativeSyncStateListener = undefined;
    mockBackend();
  });

  it("calculates progress percentage from completed and total", () => {
    expect(formatNativeSyncProgress({ completed: 42, total: 100 })).toBe("下载中 42%");
    expect(formatNativeSyncProgress({ completed: 11, total: 16 })).toBe("下载中 69%");
  });

  it("formats modifiedAt as Unix seconds", () => {
    expect(formatNativeSyncDate(1727000000)).toBe(new Date(1727000000 * 1000).toLocaleString());
  });

  it("selects manager mode only for the secondary window label", () => {
    expect(selectWindowMode("native-sync-manager")).toBe("native-sync-manager");
    expect(selectWindowMode("main")).toBe("settings");
    expect(selectWindowMode("anything-else")).toBe("settings");
  });

  it("renders the status summary and all supported entry badges", async () => {
    render(<ManagerApp />);
    expect(await screen.findByText(/C:\\Pandock\\NativeSync/)).toBeInTheDocument();
    expect(screen.getByText(/占位符 3/)).toBeInTheDocument();
    expect(listTable().getByText("仅在线")).toBeInTheDocument();
    expect(listTable().getByText("下载中 42%")).toBeInTheDocument();
    expect(listTable().getAllByText("已下载").length).toBeGreaterThan(0);
    expect(listTable().getByText("错误")).toBeInTheDocument();
    expect(listTable().getByText("远端已删")).toBeInTheDocument();
    expect(listTable().getByText("有更新")).toBeInTheDocument();
  });

  it("lazily expands the directory tree and collapses it again", async () => {
    const user = userEvent.setup();
    render(<ManagerApp />);
    const expandDocs = await screen.findByRole("button", { name: "展开 docs" });
    await user.click(expandDocs);
    expect(invokeMock).toHaveBeenCalledWith("native_sync_list", { parentPath: "docs", cursor: null, limit: 50 });
    await waitFor(() => expect(screen.getByRole("button", { name: "收起 docs" })).toBeInTheDocument());
    expect(within(screen.getByLabelText("目录树")).getByRole("button", { name: "sub" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "收起 docs" }));
    expect(screen.queryByRole("button", { name: "sub" })).not.toBeInTheDocument();
  });

  it("navigates with the address bar breadcrumbs", async () => {
    const user = userEvent.setup();
    render(<ManagerApp />);
    await screen.findByText(/占位符 3/);
    await user.dblClick(await listTable().findByRole("button", { name: "docs" }));
    expect(await within(await screen.findByTestId("entry-grid")).findByText("nested.txt")).toBeInTheDocument();
    expect(within(screen.getByLabelText("地址栏")).getByRole("button", { name: "docs" })).toHaveClass("current");
    await user.click(within(screen.getByLabelText("地址栏")).getByRole("button", { name: "同步根目录" }));
    expect(await listTable().findByText("online.txt")).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith("native_sync_list", { parentPath: null, cursor: null, limit: 50 });
  });

  it("selects entries and enables the bottom action bar by entry state", async () => {
    const user = userEvent.setup();
    render(<ManagerApp />);
    await screen.findByText(/占位符 3/);
    await user.click(await listTable().findByRole("button", { name: "online.txt" }));
    const bar = screen.getByLabelText("选中条目操作");
    expect(within(bar).getByRole("button", { name: "固定到本地" })).toBeInTheDocument();
    expect(listTable().getByRole("button", { name: "online.txt" })).toHaveClass("selected");
    await user.click(listTable().getByRole("button", { name: "docs" }));
    const dirBar = screen.getByLabelText("选中条目操作");
    expect(within(dirBar).queryByRole("button", { name: "固定到本地" })).not.toBeInTheDocument();
    expect(within(dirBar).getByRole("button", { name: "在资源管理器中打开" })).toBeInTheDocument();
    await user.click(within(dirBar).getByRole("button", { name: "取消选中" }));
    expect(screen.queryByLabelText("选中条目操作")).not.toBeInTheDocument();
  });

  it("opens files in Explorer on double click and enters directories", async () => {
    const user = userEvent.setup();
    render(<ManagerApp />);
    await screen.findByText(/占位符 3/);
    await user.dblClick(await listTable().findByRole("button", { name: "online.txt" }));
    expect(invokeMock).toHaveBeenCalledWith("native_sync_open_in_explorer", { relativePath: "online.txt" });
    await user.dblClick(listTable().getByRole("button", { name: "docs" }));
    expect(invokeMock).toHaveBeenCalledWith("native_sync_list", { parentPath: "docs", cursor: null, limit: 50 });
    expect(await within(await screen.findByTestId("entry-grid")).findByText("nested.txt")).toBeInTheDocument();
  });

  it("sends action parameters from the bottom action bar", async () => {
    const user = userEvent.setup();
    render(<ManagerApp />);
    await screen.findByText(/占位符 3/);
    const select = async (name: string) => { await user.click(await listTable().findByRole("button", { name })); return within(screen.getByLabelText("选中条目操作")); };
    let bar = await select("online.txt");
    await user.click(bar.getByRole("button", { name: "固定到本地" }));
    expect(invokeMock).toHaveBeenCalledWith("native_sync_action", { relativePath: "online.txt", action: "pin" });
    bar = await select("broken.bin");
    await user.click(bar.getByRole("button", { name: "重试" }));
    expect(invokeMock).toHaveBeenCalledWith("native_sync_action", { relativePath: "broken.bin", action: "retry" });
    bar = await select("deleted.doc");
    await user.click(bar.getByRole("button", { name: "已知悉" }));
    expect(invokeMock).toHaveBeenCalledWith("native_sync_action", { relativePath: "deleted.doc", action: "ack_remote_missing" });
    await waitFor(() => expect(within(screen.getByLabelText("选中条目操作")).queryByRole("button", { name: "已知悉" })).not.toBeInTheDocument());
    expect(listTable().getByText("远端已删")).toBeInTheDocument();
    const missingCell = listTable().getByRole("button", { name: "deleted.doc" });
    expect(within(missingCell).queryByRole("button", { name: "释放空间" })).toBeNull();
    bar = await select("ready.pdf");
    expect(within(screen.getByLabelText("选中条目操作")).getByTitle("恢复为按需下载")).toBeInTheDocument();
    expect(bar.queryByRole("button", { name: "释放空间" })).not.toBeInTheDocument();
    await user.click(bar.getByRole("button", { name: "取消固定" }));
    expect(invokeMock).toHaveBeenCalledWith("native_sync_action", { relativePath: "ready.pdf", action: "unpin" });
    bar = await select("cached.tmp");
    await user.click(bar.getByRole("button", { name: "释放空间" }));
    expect(invokeMock).toHaveBeenCalledWith("native_sync_action", { relativePath: "cached.tmp", action: "dehydrate" });
    bar = await select("online.txt");
    await user.click(bar.getByRole("button", { name: "在资源管理器中打开" }));
    expect(invokeMock).toHaveBeenCalledWith("native_sync_open_in_explorer", { relativePath: "online.txt" });
    await user.click(screen.getByRole("button", { name: "立即同步" }));
    expect(invokeMock).toHaveBeenCalledWith("native_sync_sync_now");
    await user.click(screen.getByRole("button", { name: "打开根目录" }));
    expect(invokeMock).toHaveBeenCalledWith("native_sync_open_in_explorer", { relativePath: "" });
  });

  it("refreshes the list after actions so pin state updates", async () => {
    let pinned = false;
    invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
      if (command === "native_sync_status") return enabledStatus;
      if (command === "native_sync_list") return {
        entries: pinned ? entries.map((entry) => entry.fsId === "1" ? { ...entry, pinState: "pinned" } : entry) : entries,
        nextCursor: null, total: entries.length, parentPath: "",
      };
      if (command === "native_sync_action") { pinned = true; return { ...entries[0], pinState: "pinned" }; }
      return undefined;
    });
    const user = userEvent.setup();
    render(<ManagerApp />);
    await screen.findByText(/占位符 3/);
    await user.click(await listTable().findByRole("button", { name: "online.txt" }));
    await user.click(within(screen.getByLabelText("选中条目操作")).getByRole("button", { name: "固定到本地" }));
    await waitFor(() => expect(within(screen.getByLabelText("选中条目操作")).getByRole("button", { name: "取消固定" })).toBeInTheDocument());
    const onlineCell = listTable().getByRole("button", { name: "online.txt" });
    expect(within(onlineCell).queryByRole("button", { name: "固定到本地" })).toBeNull();
  });

  it("loads another cursor page", async () => {
    const user = userEvent.setup();
    mockBackend({ list: { entries: entries.slice(0, 2), nextCursor: "cursor-2", total: entries.length, parentPath: "" } });
    render(<ManagerApp />);
    await screen.findByText(/占位符 3/);
    await listTable().findByText("online.txt");
    await user.click(screen.getByRole("button", { name: "加载更多" }));
    expect(invokeMock).toHaveBeenCalledWith("native_sync_list", { parentPath: null, cursor: "cursor-2", limit: 50 });
    expect(await listTable().findByText("downloading.zip")).toBeInTheDocument();
  });

  it("updates status and refreshes entries when the state event arrives", async () => {
    render(<ManagerApp />);
    await screen.findByText(/占位符 3/);
    expect(nativeSyncStateListener).toBeDefined();
    act(() => nativeSyncStateListener!({ payload: { ...enabledStatus, state: "syncing", hydratedEntries: 4 } }));
    expect(await screen.findByText(/已下载 4/)).toBeInTheDocument();
    expect(screen.getByText("同步中")).toBeInTheDocument();
    const rootListCalls = invokeMock.mock.calls.filter((call) => call[0] === "native_sync_list" && call[1]?.parentPath === null);
    expect(rootListCalls.length).toBeGreaterThanOrEqual(2);
  });


  it("keeps the browser and tree when a transient disabled status arrives", async () => {
    render(<ManagerApp />);
    await screen.findByText(/占位符 3/);
    expect(nativeSyncStateListener).toBeDefined();
    const transientDisabled = { ...enabledStatus, enabled: false, state: "disabled", rootPath: null, lastError: null };
    act(() => nativeSyncStateListener!({ payload: transientDisabled }));
    expect(screen.getByText("已启用")).toBeInTheDocument();
    expect(screen.getByText(/C:\\Pandock\\NativeSync/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "展开 docs" })).toBeInTheDocument();
  });

  it("keeps a leaf directory node visible after clicking it", async () => {
    const user = userEvent.setup();
    render(<ManagerApp />);
    await screen.findByText(/占位符 3/);
    await user.click(await screen.findByRole("button", { name: "展开 docs" }));
    await waitFor(() => expect(within(screen.getByLabelText("目录树")).getByRole("button", { name: "sub" })).toBeInTheDocument());
    await user.click(within(screen.getByLabelText("目录树")).getByRole("button", { name: "sub" }));
    expect(within(screen.getByLabelText("目录树")).getByRole("button", { name: "sub" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "收起 docs" })).toBeInTheDocument();
    expect(screen.getByLabelText("地址栏")).toBeInTheDocument();
  });


  it("keeps the browser mounted when clicking the same directory again", async () => {
    const user = userEvent.setup();
    render(<ManagerApp />);
    await screen.findByText(/占位符 3/);
    await user.click(within(screen.getByLabelText("目录树")).getByRole("button", { name: "docs" }));
    expect(await within(await screen.findByTestId("entry-grid")).findByText("nested.txt")).toBeInTheDocument();
    await user.click(within(screen.getByLabelText("目录树")).getByRole("button", { name: "docs" }));
    expect(listTable().getByText("nested.txt")).toBeInTheDocument();
    expect(screen.getByLabelText("地址栏")).toBeInTheDocument();
    expect(within(screen.getByLabelText("目录树")).getByRole("button", { name: "docs" })).toBeInTheDocument();
  });

  it("keeps the tree and address bar visible while the content area is loading", async () => {
    let resolveDocs: ((value: { entries: NativeSyncEntry[]; nextCursor: string | null; total: number; parentPath: string }) => void) | undefined;
    invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
      if (command === "native_sync_status") return enabledStatus;
      if (command === "native_sync_list" && args?.parentPath === "docs") return new Promise<typeof docsEntries>((resolve) => { resolveDocs = resolve as never; });
      if (command === "native_sync_list") return { entries, nextCursor: null, total: entries.length, parentPath: "" };
      return undefined;
    });
    const user = userEvent.setup();
    render(<ManagerApp />);
    await screen.findByText(/占位符 3/);
    await user.click(within(screen.getByLabelText("目录树")).getByRole("button", { name: "docs" }));
    expect(screen.getByLabelText("地址栏")).toBeInTheDocument();
    expect(within(screen.getByLabelText("目录树")).getByRole("button", { name: "docs" })).toBeInTheDocument();
    expect(await screen.findByText("正在加载同步目录…")).toBeInTheDocument();
    resolveDocs!({ entries: docsEntries, nextCursor: null, total: docsEntries.length, parentPath: "docs" });
    expect(await within(await screen.findByTestId("entry-grid")).findByText("nested.txt")).toBeInTheDocument();
    expect(screen.getByLabelText("地址栏")).toBeInTheDocument();
  });


  it("disables action immediately and shows delayed spinner for a slow sync", async () => {
    let resolveSync: ((value: NativeSyncStatus) => void) | undefined;
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "native_sync_status") return enabledStatus;
      if (command === "native_sync_list") return { entries: [], nextCursor: null, total: 0, parentPath: "" };
      if (command === "native_sync_sync_now") return new Promise<NativeSyncStatus>((resolve) => { resolveSync = resolve; });
      return undefined;
    });
    const user = userEvent.setup();
    render(<ManagerApp />);
    const button = await screen.findByRole("button", { name: "立即同步" });
    await user.click(button);
    expect(button).toBeDisabled();
    expect(document.querySelector(".spinner")).toBeNull();
    await waitFor(() => expect(document.querySelector(".spinner")).not.toBeNull(), { timeout: 1500 });
    resolveSync!(enabledStatus);
    await waitFor(() => expect(document.querySelector(".spinner")).toBeNull());
  });

  it("handles unsupported, disabled, empty, and error states", async () => {
    mockBackend({ status: { ...enabledStatus, supported: false, enabled: false, supportReason: "Windows 1903 或更高版本" } });
    render(<ManagerApp />);
    expect(await screen.findByText("当前系统不支持 NativeSync")).toBeInTheDocument();
    expect(screen.getByText("Windows 1903 或更高版本")).toBeInTheDocument();

    const { unmount } = render(<ManagerApp />);
    unmount();
    mockBackend({ status: { ...enabledStatus, enabled: false, state: "disabled" }, list: { entries: [], nextCursor: null, total: 0, parentPath: "" } });
    render(<ManagerApp />);
    expect(await screen.findByText("NativeSync 尚未启用")).toBeInTheDocument();

    const { unmount: unmount2 } = render(<ManagerApp />);
    unmount2();
    mockBackend({ list: { entries: [], nextCursor: null, total: 0, parentPath: "" } });
    render(<ManagerApp />);
    expect(await screen.findByText("此目录为空")).toBeInTheDocument();

    const { unmount: unmount3 } = render(<ManagerApp />);
    unmount3();
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "native_sync_status") return enabledStatus;
      if (command === "native_sync_list") throw new Error("backend unavailable");
      return undefined;
    });
    render(<ManagerApp />);
    expect(await screen.findByText("无法加载同步目录")).toBeInTheDocument();
    expect(screen.getAllByText(/backend unavailable/).length).toBeGreaterThan(0);
  });
});
