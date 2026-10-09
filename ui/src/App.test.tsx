import React from "react";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { App, type AuthorizationChallenge, type UpdateStateView } from "./App";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));
const eventListeners = new Map<string, (event: { payload: unknown }) => void>();
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (name: string, cb: (event: { payload: unknown }) => void) => { eventListeners.set(name, cb); return () => undefined; }) }));

const baseUpdate: UpdateStateView = {
  phase: "idle",
  currentVersion: "1.0.0",
  availableVersion: null,
  releaseUrl: null,
  manualOnly: false,
  error: null,
};

const baseConfig = {
  app_key: "",
  app_name: "",
  listen: "127.0.0.1:19090",
  dav_prefix: "/dav",
  basic_auth: false,
  username: "dav",
  webdav_password: "",
  cache_size_mb: 256,
  launch_at_login: false,
  start_webdav_automatically: true,
};

const challenge: AuthorizationChallenge = {
  flowId: "flow-1",
  userCode: "ABCD-1234",
  verificationUrl: "https://example.invalid/device",
  qrCodeUrl: "https://example.invalid/qr.png",
  expiresAt: 123,
  pollIntervalSeconds: 6,
};

function mockInvoke(
  authState: Record<string, unknown> = { phase: "disconnected", appKey: null, appName: null, challenge: null, error: null, connectedAt: null },
  beginChallenge: AuthorizationChallenge | Promise<AuthorizationChallenge> = challenge,
  updateState: UpdateStateView = baseUpdate,
  updateCheckResult: UpdateStateView | Promise<UpdateStateView> | (() => UpdateStateView | Promise<UpdateStateView>) = { ...updateState, phase: "update_available", availableVersion: "1.0.1" },
  saveResult: unknown = undefined,
) {
  invokeMock.mockImplementation(async (command: string) => {
    switch (command) {
      case "get_status": return { kind: "not_configured" };
      case "get_config": return baseConfig;
      case "get_autostart": return false;
      case "auth_state": return authState;
      case "runtime_platform": return "windows";
      case "update_state": return updateState;
      case "update_check": return typeof updateCheckResult === "function" ? updateCheckResult() : updateCheckResult;
      case "update_download": return { ...updateState, phase: "ready_to_install", availableVersion: "1.0.1" };
      case "auth_begin": return beginChallenge;
      case "auth_retry": return beginChallenge;
      case "auth_cancel": return undefined;
      case "auth_disconnect": return undefined;
      case "set_autostart": return undefined;
      case "save_config": return typeof saveResult === "function" ? (saveResult as () => unknown)() : saveResult;
      default: return undefined;
    }
  });
}

describe("Pandock auth modal", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    eventListeners.clear();
    mockInvoke();
  });

  it("shows connect button when disconnected", async () => {
    render(<App />);
    expect(await screen.findByRole("button", { name: "连接百度网盘" })).toBeInTheDocument();
  });

  it("shows connected state without connect button", async () => {
    mockInvoke({ phase: "connected", appKey: "key", appName: "App", challenge: null, error: null, connectedAt: 123 });
    render(<App />);
    expect(await screen.findByRole("button", { name: "断开连接" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "连接百度网盘" })).not.toBeInTheDocument();
  });

  it("persists the autostart toggle in the saved configuration", async () => {
    const user = userEvent.setup();
    render(<App />);
    const toggle = await screen.findByLabelText(/开机启动/);
    expect(toggle).not.toBeChecked();
    await user.click(toggle);
    expect(invokeMock).toHaveBeenCalledWith("set_autostart", { enabled: true });
    expect(toggle).toBeChecked();
    await user.click(toggle);
    expect(invokeMock).toHaveBeenCalledWith("set_autostart", { enabled: false });
    expect(toggle).not.toBeChecked();
  });

  it("allows enabling NativeSync with an empty root and passes null for the Known Folder default", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: "NativeSync" }));
    const rootInput = await screen.findByPlaceholderText("留空使用系统文档目录下的 Pandock（推荐）");
    expect(rootInput).toHaveValue("");
    const enableButton = screen.getByRole("button", { name: "启用同步" });
    expect(enableButton).toBeEnabled();
    await user.click(enableButton);
    expect(invokeMock).toHaveBeenCalledWith("native_sync_enable", { rootPath: null });
  });

  it("updates the NativeSync settings card from the state event", async () => {
    render(<App />);
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "NativeSync" }));
    const listener = eventListeners.get("pandock://nativesync/state");
    expect(listener).toBeDefined();
    act(() => listener!({ payload: { enabled: true, supported: true, rootPath: null, state: "idle", totalEntries: 9, placeholderEntries: 2, hydratedEntries: 6, attentionEntries: 1, activeJobs: 0, lastSyncAt: null, lastError: null } }));
    expect(await screen.findByText(/已下载 6/)).toBeInTheDocument();
    expect(screen.getByText("已启用")).toBeInTheDocument();
  });


  it("keeps the NativeSync card enabled when a transient disabled status arrives", async () => {
    render(<App />);
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "NativeSync" }));
    const listener = eventListeners.get("pandock://nativesync/state");
    expect(listener).toBeDefined();
    const good = { enabled: true, supported: true, rootPath: null, state: "idle", totalEntries: 9, placeholderEntries: 2, hydratedEntries: 6, attentionEntries: 1, activeJobs: 0, lastSyncAt: null, lastError: null };
    const transientDisabled = { ...good, enabled: false, state: "disabled" };
    act(() => listener!({ payload: good }));
    expect(await screen.findByText(/已下载 6/)).toBeInTheDocument();
    act(() => listener!({ payload: transientDisabled }));
    expect(screen.getByText("已启用")).toBeInTheDocument();
    expect(screen.getByText(/已下载 6/)).toBeInTheDocument();
  });


  it("opens credential modal and waits for authorization", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(await screen.findByRole("button", { name: "连接百度网盘" }));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    await user.type(screen.getByLabelText("App Key"), "key");
    await user.type(screen.getByLabelText("Secret Key"), "secret");
    await user.type(screen.getByLabelText("应用名称"), "App");
    await user.click(screen.getByRole("button", { name: "开始连接" }));
    expect(await screen.findByText("用户码：")).toBeInTheDocument();
    expect(screen.getByText("ABCD-1234")).toBeInTheDocument();
    expect(screen.queryByLabelText("Secret Key")).not.toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith("auth_begin", { input: { appKey: "key", appName: "App", appSecret: "secret" } });
  });

  it("falls back to user code and verification URL when QR is unavailable", async () => {
    const user = userEvent.setup();
    mockInvoke({ phase: "disconnected", appKey: null, appName: null, challenge: null, error: null, connectedAt: null }, { ...challenge, qrCodeUrl: null });
    render(<App />);
    await user.click(await screen.findByRole("button", { name: "连接百度网盘" }));
    await user.type(screen.getByLabelText("App Key"), "key");
    await user.type(screen.getByLabelText("Secret Key"), "secret");
    await user.type(screen.getByLabelText("应用名称"), "App");
    await user.click(screen.getByRole("button", { name: "开始连接" }));
    expect(await screen.findByText("二维码加载失败，请使用下方用户码或验证网址。")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "打开百度授权页面" })).toBeInTheDocument();
  });

  it("shows validation error before any backend call", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(await screen.findByRole("button", { name: "连接百度网盘" }));
    await user.click(screen.getByRole("button", { name: "开始连接" }));
    expect(await screen.findByText("请填写 App Key、Secret Key 和应用名称。")).toBeInTheDocument();
    expect(invokeMock).not.toHaveBeenCalledWith("auth_begin", expect.anything());
  });

  it("cancels the pending device-code request when the modal is closed", async () => {
    let resolveChallenge!: (value: AuthorizationChallenge) => void;
    const pendingChallenge = new Promise<AuthorizationChallenge>((resolve) => {
      resolveChallenge = resolve;
    });
    mockInvoke({ phase: "disconnected", appKey: null, appName: null, challenge: null, error: null, connectedAt: null }, pendingChallenge);
    const user = userEvent.setup();
    render(<App />);
    await user.click(await screen.findByRole("button", { name: "连接百度网盘" }));
    await user.type(screen.getByLabelText("App Key"), "key");
    await user.type(screen.getByLabelText("Secret Key"), "secret");
    await user.type(screen.getByLabelText("应用名称"), "App");
    await user.click(screen.getByRole("button", { name: "开始连接" }));
    expect(await screen.findByText("正在向百度网盘请求设备码...")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "取消" }));
    resolveChallenge(challenge);
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("auth_cancel", { flowId: "flow-1" });
    });
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("checks for updates from the About tile", async () => {
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: "关于" }));
    expect(await screen.findByText("当前版本")).toBeInTheDocument();
    expect(screen.getByText("1.0.0")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "检查更新" }));
    expect(await screen.findByText("可用版本：1.0.1")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "下载并校验" })).toBeInTheDocument();
  });

  it("renders the navigation with brand status and keeps save at the page bottom", async () => {
    const user = userEvent.setup();
    render(<App />);
    expect(await screen.findByText("Pandock")).toBeInTheDocument();
    const nav = screen.getByLabelText("设置导航");
    expect(within(nav).getAllByRole("button").map((button) => button.textContent)).toEqual(["常规", "WebDAV", "NativeSync", "关于"]);
    expect(within(nav).getByRole("button", { name: "常规" })).toHaveClass("active");
    await user.click(within(nav).getByRole("button", { name: "WebDAV" }));
    expect(await screen.findByLabelText("本地监听地址")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "连接百度网盘" })).not.toBeInTheDocument();
    const saveButton = screen.getByRole("button", { name: "保存设置" });
    expect(saveButton.closest(".settings-actions")).not.toBeNull();
  });

  it("maps the brand status dot to the connection state", async () => {
    render(<App />);
    expect(await screen.findByText("Pandock")).toBeInTheDocument();
    expect(document.querySelector(".brand-status")?.className).toContain("mid");
    expect(document.querySelector(".brand-status .dot")).not.toBeNull();
  });

  it("keeps update failures out of the global footer message", async () => {
    const forbiddenUpdate: UpdateStateView = {
      ...baseUpdate,
      phase: "error",
      error: {
        code: "forbidden",
        status: 403,
        message: "GitHub 权限不足",
        retryable: false,
      },
    };
    mockInvoke(
      undefined,
      challenge,
      forbiddenUpdate,
      () => Promise.reject(new Error("GitHub 权限不足")),
    );
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: "关于" }));
    await user.click(await screen.findByRole("button", { name: "检查更新" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("GitHub 权限不足");
    await user.click(screen.getByRole("button", { name: "WebDAV" }));
    expect(screen.getByLabelText("本地监听地址")).toBeInTheDocument();
    expect(document.querySelector(".settings-actions")?.textContent ?? "").not.toContain("GitHub 权限不足");
  });

  it("shows a Forbidden diagnostic only once in the About tile", async () => {
    const forbiddenUpdate: UpdateStateView = {
      ...baseUpdate,
      phase: "error",
      error: {
        code: "forbidden",
        status: 403,
        message: "GitHub 权限不足",
        retryable: false,
      },
    };
    mockInvoke(undefined, challenge, forbiddenUpdate);
    render(<App />);
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "关于" }));
    const alerts = await screen.findAllByRole("alert");
    expect(alerts).toHaveLength(1);
    expect(alerts[0]).toHaveTextContent("更新检查失败");
    expect(alerts[0]).toHaveTextContent("GitHub 权限不足");
    expect(alerts[0]).toHaveTextContent("HTTP 403");
    expect(alerts[0].textContent).not.toContain("GitHub App");
  });

  it("shows network failures with host diagnostics", async () => {
    const networkError: UpdateStateView = {
      ...baseUpdate,
      phase: "error",
      error: {
        code: "network_unreachable",
        status: null,
        message: "HTTP 403 from release-assets.githubusercontent.com",
        retryable: true,
        host: "release-assets.githubusercontent.com",
      },
    };
    mockInvoke(undefined, challenge, networkError);
    render(<App />);
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "关于" }));
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("无法连接更新服务");
    expect(alert).toHaveTextContent("请检查网络连接后重试");
    expect(alert).toHaveTextContent("release-assets.githubusercontent.com");
    expect(alert.textContent).not.toContain("GitHub App");
  });

  it("shows up-to-date success state without an error banner", async () => {
    const upToDate: UpdateStateView = { ...baseUpdate, phase: "up_to_date" };
    mockInvoke(undefined, challenge, baseUpdate, upToDate);
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: "关于" }));
    await user.click(await screen.findByRole("button", { name: "检查更新" }));
    const banner = await screen.findByRole("status");
    expect(banner).toHaveClass("msg");
    expect(banner).toHaveTextContent("当前版本已是最新。");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByText(/拒绝降级更新/)).not.toBeInTheDocument();
  });

  it("keeps About tile buttons in one action row and banners full width below", async () => {
    const upToDate: UpdateStateView = { ...baseUpdate, phase: "up_to_date" };
    mockInvoke(undefined, challenge, upToDate, upToDate);
    render(<App />);
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "关于" }));
    await screen.findByRole("button", { name: "检查更新" });
    const actions = document.querySelector(".about-update .about-actions");
    expect(actions).not.toBeNull();
    const labels = Array.from(document.querySelectorAll(".about-update .about-actions button")).map((button) => button.textContent);
    expect(labels).toContain("检查更新");
    expect(labels).not.toContain("连接 GitHub");
    const banner = await screen.findByRole("status");
    expect(banner.parentElement).toBe(actions);
    expect(banner.compareDocumentPosition(actions!) & Node.DOCUMENT_POSITION_PRECEDING).toBeTruthy();
    const aboutItems = document.querySelectorAll(".about-grid .about-item");
    expect(aboutItems).toHaveLength(3);
    expect(document.querySelector(".about-grid")).toHaveTextContent("GitHub 项目");
    expect(document.querySelector(".about-grid")).toHaveTextContent("weyham/pandock");
  });

  it("shows a delayed busy state only when the update check exceeds 500ms", async () => {
    let resolveCheck: ((value: UpdateStateView) => void) | undefined;
    const slow = new Promise<UpdateStateView>((resolve) => { resolveCheck = resolve; });
    mockInvoke(undefined, challenge, baseUpdate, () => slow);
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: "关于" }));
    const button = await screen.findByRole("button", { name: "检查更新" });
    await user.click(button);
    expect(button).toBeDisabled();
    expect(button).toHaveTextContent("检查中…");
    resolveCheck!({ ...baseUpdate, phase: "up_to_date" });
    expect(await screen.findByRole("button", { name: "检查更新" })).toBeEnabled();
    expect(await screen.findByRole("status")).toHaveTextContent("当前版本已是最新。");
  });


  it("does not flash a spinner for fast operations", async () => {
    mockInvoke(undefined, challenge, baseUpdate, { ...baseUpdate, phase: "up_to_date" });
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: "关于" }));
    await user.click(await screen.findByRole("button", { name: "检查更新" }));
    expect(await screen.findByRole("status")).toHaveTextContent("当前版本已是最新。");
    await new Promise((resolve) => setTimeout(resolve, 650));
    expect(document.querySelector(".spinner")).toBeNull();
  });

  it("enables save only when the form differs from the saved snapshot", async () => {
    mockInvoke();
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: "WebDAV" }));
    const saveButton = await screen.findByRole("button", { name: "保存设置" });
    expect(saveButton).toBeDisabled();
    const listenInput = screen.getByLabelText("本地监听地址");
    await user.clear(listenInput);
    await user.type(listenInput, "127.0.0.1:19091");
    expect(saveButton).toBeEnabled();
    await user.click(saveButton);
    expect(await screen.findByText("设置已保存。")).toBeInTheDocument();
    expect(saveButton).toBeDisabled();
    expect(listenInput).toHaveValue("127.0.0.1:19091");
  });

  it("treats an empty WebDAV password as unchanged and keeps the button enabled after a failed save", async () => {
    mockInvoke();
    const user = userEvent.setup();
    render(<App />);
    await user.click(screen.getByRole("button", { name: "WebDAV" }));
    const saveButton = await screen.findByRole("button", { name: "保存设置" });
    await user.click(screen.getByLabelText(/启用 WebDAV 密码/));
    expect(saveButton).toBeEnabled();
    await user.click(saveButton);
    expect(await screen.findByText("设置已保存。")).toBeInTheDocument();
    expect(saveButton).toBeDisabled();
    const passwordInput = screen.getByLabelText(/留空表示继续使用已保存密码/);
    await user.type(passwordInput, "secret1");
    expect(saveButton).toBeEnabled();
    await user.clear(passwordInput);
    expect(saveButton).toBeDisabled();

    invokeMock.mockImplementation(async (command: string) => {
      if (command === "save_config") throw new Error("disk full");
      return undefined;
    });
    await user.type(passwordInput, "secret2");
    expect(saveButton).toBeEnabled();
    await user.click(saveButton);
    expect(await screen.findByText(/disk full/)).toBeInTheDocument();
    expect(saveButton).toBeEnabled();
  });
});
