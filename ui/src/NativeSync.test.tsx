import React from "react";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  NativeSyncSettings,
  useDelayedBusy,
  type NativeSyncEntry,
  type NativeSyncStatus,
} from "./NativeSync";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));
let nativeSyncStateListener: ((event: { payload: NativeSyncStatus }) => void) | undefined;
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (_name: string, cb: (event: { payload: NativeSyncStatus }) => void) => { nativeSyncStateListener = cb; return () => undefined; }) }));

const enabledStatus: NativeSyncStatus = {
  enabled: true, supported: true, rootPath: "C:\\Pandock\\NativeSync", state: "idle",
  totalEntries: 8, placeholderEntries: 3, hydratedEntries: 3, attentionEntries: 1, activeJobs: 0,
  lastSyncAt: 1727000000, lastError: null,
};

/** 默认视图（可操作）的夹具：含一个 4 层深的 hydrated 文件（验收可达性样本）+ 需处理 + 已固定。 */
const actionableEntries: NativeSyncEntry[] = [
  { fsId: "deep-db", name: "db.sql", relativePath: "cc-switch-sync/v2/db-v6/default/db.sql", isDir: false, size: 52699, state: "hydrated", pinState: "unpinned", modifiedAt: 1727000000, hasChildren: false },
  { fsId: "pinned-1", name: "ready.pdf", relativePath: "ready.pdf", isDir: false, size: 20480, state: "hydrated", pinState: "pinned", modifiedAt: 1727000000, hasChildren: false },
  { fsId: "err-1", name: "broken.bin", relativePath: "broken.bin", isDir: false, size: 0, state: "error", pinState: "unpinned", error: "下载失败", hasChildren: false },
  { fsId: "missing-1", name: "deleted.doc", relativePath: "deleted.doc", isDir: false, size: 4096, state: "remote_missing", pinState: "pinned", hasChildren: false },
];
/** 浏览模式：根层（一个目录 + 一个文件）与 docs 目录的内容。 */
const browseRootEntries: NativeSyncEntry[] = [
  { fsId: "dir-docs", name: "docs", relativePath: "docs", isDir: true, size: 0, state: "online_only", pinState: "unpinned", hasChildren: true },
  { fsId: "root-file", name: "root.txt", relativePath: "root.txt", isDir: false, size: 10, state: "online_only", pinState: "unpinned", hasChildren: false },
];
const browseDocsEntries: NativeSyncEntry[] = [
  { fsId: "docs-file", name: "nested.txt", relativePath: "docs/nested.txt", isDir: false, size: 12, state: "hydrated", pinState: "unpinned", hasChildren: false },
];

function mockBackend({
  status = enabledStatus,
  actionable = { entries: actionableEntries, nextCursor: null, total: actionableEntries.length, parentPath: "/" },
  browseRoot = { entries: browseRootEntries, nextCursor: null, total: browseRootEntries.length, parentPath: "/" },
  browseDocs = { entries: browseDocsEntries, nextCursor: null, total: browseDocsEntries.length, parentPath: "docs" },
}: {
  status?: NativeSyncStatus;
  actionable?: { entries: NativeSyncEntry[]; nextCursor?: string | null; total: number; parentPath?: string | null };
  browseRoot?: { entries: NativeSyncEntry[]; nextCursor?: string | null; total: number; parentPath?: string | null };
  browseDocs?: { entries: NativeSyncEntry[]; nextCursor?: string | null; total: number; parentPath?: string | null };
} = {}) {
  invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case "native_sync_status": return status;
      case "native_sync_list": {
        if (args?.filter === "actionable") return actionable;
        if (args?.parentPath === "docs") return browseDocs;
        return browseRoot;
      }
      case "native_sync_action": {
        const entry = [...actionableEntries, ...browseRootEntries, ...browseDocsEntries].find((item) => item.relativePath === args?.relativePath)!;
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

/** 与 App.tsx 同构的最小接线：设置卡 + 自带 busy（覆盖 useDelayedBusy 语义）。 */
function renderSettings(overrides: { status?: NativeSyncStatus; syncNowTask?: () => Promise<NativeSyncStatus> } = {}) {
  const Harness = () => {
    const busy = useDelayedBusy();
    return <NativeSyncSettings
      status={overrides.status ?? enabledStatus}
      statusError=""
      rootPath=""
      setRootPath={() => {}}
      onRefresh={() => {}}
      onEnable={() => {}}
      onDisable={() => {}}
      onSyncNow={() => { void busy.run("native_sync_sync_now", overrides.syncNowTask ?? (async () => enabledStatus)); }}
      onOpenRoot={() => { void busy.run("native_sync_open_root", async () => invokeMock("native_sync_open_in_explorer", { relativePath: "" })); }}
      busy={busy}
    />;
  };
  return render(<Harness />);
}

describe("NativeSync settings page entry list", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    nativeSyncStateListener = undefined;
    mockBackend();
  });

  it("renders actionable entries in the default view without drilling", async () => {
    renderSettings();
    // 验收可达性：4 层深的 hydrated 条目在默认视图出现（无需点目录下钻）。
    const row = await screen.findByRole("button", { name: /^db\.sql/ });
    expect(within(row).getByText("cc-switch-sync/v2/db-v6/default/db.sql")).toBeTruthy();
    expect(within(row).getByText("已下载")).toBeTruthy();
    expect(within(row).getByText(/51 KB/)).toBeTruthy();
  });

  it("loads another cursor page and merges by relativePath", async () => {
    let listCalls = 0;
    invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
      if (command === "native_sync_status") return enabledStatus;
      if (command === "native_sync_list") {
        listCalls += 1;
        if (listCalls === 1) {
          return { entries: [actionableEntries[0]], nextCursor: "1", total: 2, parentPath: "/" };
        }
        expect(args?.filter).toBe("actionable");
        expect(args?.cursor).toBe("1");
        return {
          entries: [
            actionableEntries[0],
            { fsId: "p2", name: "page2.txt", relativePath: "page2.txt", isDir: false, size: 2, state: "hydrated", pinState: "unpinned", hasChildren: false },
          ],
          nextCursor: null,
          total: 2,
          parentPath: "/",
        };
      }
      return undefined;
    });
    renderSettings();
    expect(await screen.findByText("db.sql")).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "加载更多" }));
    await screen.findByText("page2.txt");
    // 去重合并：db.sql 仍然只出现一次
    expect(screen.getAllByText("db.sql").length).toBe(1);
  });

  it("shows the action row for the selected entry with state-appropriate buttons", async () => {
    renderSettings();
    const user = userEvent.setup();
    // hydrated 未固定 → 释放空间 + 固定到本地 + 打开所在位置
    await user.click(await screen.findByRole("button", { name: /^db\.sql/ }));
    expect(await screen.findByRole("button", { name: "释放空间" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "固定到本地" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "打开所在位置" })).toBeTruthy();
    // 已固定 → 取消固定
    await user.click(screen.getByRole("button", { name: /ready\.pdf/ }));
    expect(await screen.findByRole("button", { name: "取消固定" })).toBeTruthy();
    // error → 重试
    await user.click(screen.getByRole("button", { name: /broken\.bin/ }));
    expect(await screen.findByRole("button", { name: "重试" })).toBeTruthy();
    // remote_missing 且未知悉 → 已知悉
    await user.click(screen.getByRole("button", { name: /deleted\.doc/ }));
    expect(await screen.findByRole("button", { name: "已知悉" })).toBeTruthy();
  });

  it("sends action parameters with relativePath and action", async () => {
    renderSettings();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /^db\.sql/ }));
    await user.click(await screen.findByRole("button", { name: "释放空间" }));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("native_sync_action", { relativePath: "cc-switch-sync/v2/db-v6/default/db.sql", action: "dehydrate" }));
  });

  it("opens the entry's location in Explorer", async () => {
    renderSettings();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /^db\.sql/ }));
    await user.click(await screen.findByRole("button", { name: "打开所在位置" }));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("native_sync_open_in_explorer", { relativePath: "cc-switch-sync/v2/db-v6/default/db.sql" }));
  });

  it("opens the sync root in Explorer from the settings card", async () => {
    renderSettings();
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "打开根目录" }));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("native_sync_open_in_explorer", { relativePath: "" }));
  });

  it("updates the entry list when the state event arrives", async () => {
    renderSettings();
    await screen.findByText("db.sql");
    const before = invokeMock.mock.calls.filter((call) => call[0] === "native_sync_list").length;
    act(() => { nativeSyncStateListener?.({ payload: enabledStatus }); });
    await waitFor(() => {
      const after = invokeMock.mock.calls.filter((call) => call[0] === "native_sync_list").length;
      expect(after).toBeGreaterThan(before);
    });
  });

  it("drills into a directory and returns with the back row", async () => {
    renderSettings();
    const user = userEvent.setup();
    await screen.findByText("db.sql");
    // 进入按目录浏览（根层）
    await user.click(screen.getByRole("button", { name: "按目录浏览全部" }));
    await screen.findByRole("button", { name: /^root\.txt/ });
    expect(screen.getByText("浏览：同步根目录")).toBeTruthy();
    // 点目录行下钻
    await user.click(screen.getByRole("button", { name: /^docs/ }));
    await screen.findByText("nested.txt");
    expect(screen.getByText("浏览：docs")).toBeTruthy();
    // 返回上一级
    await user.click(screen.getByRole("button", { name: "返回上一级" }));
    await screen.findByText("root.txt");
    expect(screen.getByText("浏览：同步根目录")).toBeTruthy();
    // 返回可操作列表
    await user.click(screen.getByRole("button", { name: "返回可操作列表" }));
    await screen.findByText("db.sql");
  });

  it("disables action immediately and shows delayed spinner for a slow sync", async () => {
    let resolveSync: ((value: NativeSyncStatus) => void) | undefined;
    renderSettings({ syncNowTask: () => new Promise<NativeSyncStatus>((resolve) => { resolveSync = resolve; }) });
    const button = await screen.findByRole("button", { name: "立即同步" });
    await userEvent.click(button);
    expect(button).toBeDisabled();
    expect(within(button).queryByRole("progressbar")).toBeNull();
    await waitFor(() => expect(within(button).queryByRole("progressbar")).not.toBeNull(), { timeout: 1500 });
    resolveSync!(enabledStatus);
    await waitFor(() => expect(within(button).queryByRole("progressbar")).toBeNull());
  });
});
