# ADR-108：NativeSync 同步根改用外壳注册（StorageProviderSyncRootManager）

- 状态：接受
- 日期：2026-10-12
- 关联问题：NativeSync 启用后，资源管理器侧没有 OneDrive 式的同步状态呈现（导航树节点、状态角标、状态列、固定到本地/释放空间原生动词）

## 背景

1.1.0 起 NativeSync 通过 `CfRegisterSyncRoot` + `CfConnectSyncRoot` 建立同步根。占位符、按需下载、
释放空间在应用内可用，但资源管理器只把同步根当成普通目录：

- 没有同步根节点与云朵/绿勾状态角标；
- 没有「状态」列；
- 没有「始终保留在此设备上」「释放空间」等原生动词。

对照微软官方示例 `Windows-classic-samples/Samples/CloudMirror` 后确认：外壳集成来自
**`StorageProviderSyncRootManager.Register`（WinRT）**，该示例的调用顺序是「外壳注册 → `CfConnectSyncRoot`」，
并不调用 `CfRegisterSyncRoot`。两个注册路径混用会返回 `0x8007018B`（已拒绝访问云文件）。

## 决策

1. **注册路径改为外壳注册**：`StorageProviderSyncRootManager.Register` + `CfConnectSyncRoot`；
   不再调用 `CfRegisterSyncRoot`。
2. **同步根 Id**：`<providerId>!<用户SID>!<accountId>`，即 `Pandock!<SID>!Personal`。含 SID 保证每用户一份。
   注册表位置：`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager\<Id>`。
3. **外壳注册与引擎连接同生命周期**：`connect()` 先外壳注册再 `CfConnectSyncRoot`；
   `disconnect()` 先 `CfDisconnectSyncRoot` 再注销。原因是实测发现「已注册但提供程序未连接」会让该目录
   完全不可访问（报「云文件提供程序未运行」），而不是退化为普通目录。
4. **连接失败必须回滚**：外壳注册成功但 `CfConnectSyncRoot` 失败时立即注销，避免留下不可访问的同步根。
5. **不需要管理员提权**：`HKLM\...\Explorer\SyncRootManager` 的 ACL 对 `BUILTIN\Users` 开放 `CreateSubKey`，
   实测非管理员可完成注册与注销。
6. **幂等**：重复注册（`0x800700B7`）视为成功；注销时「未注册」（`0x80070490` / `0x80070002`）视为成功。
7. **数据来源不变**：占位符仍由应用侧 `sync_now` 主动创建；`FETCH_PLACEHOLDERS` 保持返回空
   （`CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION`），不启用 Windows 按需填充。

## 同批修复（属于同一决策范围）

- 创建占位符后、水合完成后、以及路径已存在时调用 `CfSetInSyncState`，否则状态列长期显示「同步挂起」。
- 「释放空间」顺序改为**先** `CfDehydratePlaceholder` **再**标记 in-sync；顺序相反会在释放后残留「同步挂起」。
- 同步时回读 `FILE_ATTRIBUTE_PINNED` / `FILE_ATTRIBUTE_UNPINNED`，修正资源管理器里固定/取消固定造成的状态错位。
- `remote_entry` upsert 兼容 `remote_path` 唯一约束，消除「目录建出但文件全部缺失」的同步中断。

## 影响

- 资源管理器出现同步根节点、状态角标与状态列，以及「始终保留在此设备上」「释放空间」原生动词。
- 应用被强杀后同步根保留、目录仍可访问（与 OneDrive 一致），因此**不在退出时注销**。
- **明确代价：本地改动不上传。** 当前提供程序只注册 `FETCH_DATA`、`CANCEL_FETCH_DATA`、
  `FETCH_PLACEHOLDERS` 三个回调；`NOTIFY_DELETE`、`NOTIFY_RENAME`、`NOTIFY_DEHYDRATE`、
  `NOTIFY_FILE_OPEN_COMPLETION`、`NOTIFY_FILE_CLOSE_COMPLETION` 均未注册。因此资源管理器里的
  删除、改名、固定、释放只在本地生效，云端不受影响；应用仅在下一次 `sync_now` 对账时回读事实。
  这与 ADR-103 第一阶段「remote -> local 单向」一致，双向同步在 1.3.0 候选范围内单独立项。
- 稀疏包 `Weyham.Pandock.Shell`（资源管理器右键菜单）不在本决策范围，独立处理。

## 替代方案

- 继续只用 `CfRegisterSyncRoot`：无外壳集成，不满足状态可视化需求，不采用。
- 外壳注册与 `CfRegisterSyncRoot` 并用：混用返回 `0x8007018B`，不采用。
- 改用 MSIX + 稀疏包获取更深外壳集成：与现有 portable / Velopack 分发方式冲突，且签名尚未就绪，
  列为 1.3.0 候选。