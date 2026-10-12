# ADR-110：注销同步根是破坏性操作（退出不得注销）

- 状态：已实施（代码与测试已提交；**真机回归验证待部署后执行**）
- 日期：2026-10-12
- 关联：ADR-103（Cloud Files 接入）、ADR-108（同步根外壳注册）
- 触发：真实机事故——用户托盘退出应用后，`D:\Documents\Pandock` 整棵树不再是 Cloud Files 占位符

## 背景与证据

事故现场（可复现的观测）：
```
fsutil reparsepoint query D:\Documents\Pandock                    → Reparse Tag 0x9000301A（根仍是占位符）
fsutil reparsepoint query D:\Documents\Pandock\cc-switch-sync      → Error 4390 不是重解析点
                                                       ...\default\db.sql → Error 4390
attrs: db.sql=0x100020、manifest.json=0x80020、skills.zip=0x20、目录=0x10（均无 0x400）
索引侧：local_state.placeholder=1（7 条）——**索引与文件系统事实不一致**
```
后果：占位符身份丢失后 **Explorer 没有任何云状态可显示**（无角标、无按需下载、无「释放空间」），
这与用户长期反馈的"资源管理器里状态/图标不更新"高度吻合。

机制（官方文档 + 代码路径）：
- `CfUnregisterSyncRoot`（MS Learn）原文：注销会**遍历同步根目录树**——
  **已水合的占位符文件转回普通文件**；**未水合的占位符文件被从本地永久删除**；
  已填充的占位符目录转回普通目录；未填充的占位符目录被永久删除。
- 本仓库的 `Drop(退出) → disconnect() → unregister_from_shell()`（原 `platform.rs:2622-2636`）
  就是"干净退出即注销"。实测形态与文档逐条吻合（4 个目录 → 普通目录、3 个已水合文件 → 普通文件、
  根目录本体仍是占位符）。
- 对照实验：**强杀**（`Stop-Process -Force`）后注册表 `SyncRootManager\Pandock!S-1-5-…` **仍然存在**、
  日志无注销行——即强杀符合 ADR-108 的意图，**干净退出反而违背它**。

## 决策

1. **干净退出不再注销同步根**：`disconnect()` 只做 `CfDisconnectSyncRoot` + 停止 worker；
   `unregister_from_shell()` 不再在其中调用。
2. **注销只保留给"刻意的移除"**：显式 `unregister()`（卸载 / 从资源管理器移除同步根）才注销，
   且必须在 UI/文档中告知风险：**未水合（仅在线）的占位符会被从本地永久删除**（云端数据仍在）。
   默认的"停止同步"**不得**注销——否则用户点一下停用就等于损坏本地状态。
3. **已存在非占位符的就地自愈**：`create_placeholder` 对已存在路径先按属性判定
   （纯函数 `is_placeholder_attrs` / `placeholder_repair_action`）：
   - 是占位符 → 维持 `mark_path_in_sync`；
   - **不是占位符 → `CfConvertToPlaceholder(identity=fs_id 8 字节, CF_CONVERT_FLAG_MARK_IN_SYNC)`
     就地转回占位符**（保留内容与 PINNED 位），失败 `warn` 不静默。
   这既是**事故树唯一的非破坏性恢复手段**（用户点一次「立即同步」即可把条目转回占位符），
   也是将来任何原因导致脱云时的自愈路径。

## 对 ADR-108 的修订（追加式，不改原文）

ADR-108 决策 3 写「外壳注册与引擎连接同生命周期：`connect()` 先外壳注册再 `CfConnectSyncRoot`；
`disconnect()` 先 `CfDisconnectSyncRoot` 再注销」以及"因此不在退出时注销"。
本 ADR 只修订**后半句的实现**：`disconnect()` **不再注销**（理由见下），前半句（connect 顺序）不变。
ADR-108 原文一字不改。

代价（ADR-108 已预见并接受，这里再次明确）：保持注册而引擎未连接时，
应用关闭期间访问该目录会报「云文件提供程序未运行」，而**不**退化为普通目录。
这是"保住占位符身份"与"应用关闭时仍可浏览"之间的取舍，本 ADR 选择保住占位符身份。

## 影响

- 用户**正常退出**应用不再破坏同步树；强杀与正常退出的行为一致（都不注销）。
- 应用关闭期间该目录不可浏览（预期，见 `docs/roadmap.md` 对应条目）。
- "停止同步"变为**非破坏**操作；破坏性的"移除同步根"仍需显式调用。

## 已知残留风险（后续工作项）

`connect()` 失败时的回滚仍会调用 `unregister_from_shell()`（ADR-108 决策 4：避免留下
"已注册但未连接"的不可访问目录）。若该根**此前已有未水合占位符**，这条回滚会**永久删除**它们。
建议后续改为：回滚前检查根下是否存在占位符，或改为保留注册并提示用户（不再用注销做回滚）。

## 修订（2026-10-12，追加式）：连接失败的回滚也不再注销

上面「已知残留风险」一节记录的问题**已在同一批次内修掉**：

- `CfConnectSyncRoot` 失败时**不再注销**同步根，只 `warn`（记录 HRESULT、根路径、并说明
  "保留注册，未注销"）；
- 是否注销统一由纯函数裁决：
  `sync_root_removal_action(SyncRootRemovalCause) -> SyncRootRemovalAction`，
  起因只有两种——`Deliberate`（刻意移除，允许 `Unregister`）与
  `ConnectFailure`（**恒为 `KeepRegistration`**）；
- 单测 `only_deliberate_removal_is_allowed_to_unregister_the_sync_root` 穷举全部起因，
  断言**唯一会返回 `Unregister` 的起因是刻意移除**；
- 权衡：保留"已注册但未连接"的根 → 该目录在未连接期间报「云文件提供程序未运行」，
  但**树与占位符都活着**，由下一次成功 `connect()` 后的自愈（`CfConvertToPlaceholder`）恢复；
  注销是不可逆的破坏。**前者严格优于后者。**

## 验证方式（待部署后执行）

1. **自愈生效**：用户点一次「立即同步」→ 7 个条目全部恢复为占位符（`fsutil` 有 Tag、attrs 出现 `0x400`），
   内容 sha256 与脱水前一致。
2. **根因回归**：用户从托盘退出 → 同步根注册表键**仍存在**、7 个条目**仍是占位符**、日志**无**注销行。
   三条同时满足才算修复成立；**若仍脱云 → 说明还有第二个致因，不得当作已修**。
