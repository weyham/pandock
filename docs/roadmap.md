# Pandock 路线图

## 已交付

### 1.0.0（2026-09-24）

- 百度网盘设备码授权与模态连接流程。
- WebDAV 本地桥接、基础认证与系统凭据存储。
- 私有 GitHub Release 更新源、签名清单与 portable 更新助手。
- Windows portable zip 分发。

### 1.1.0（2026-10-03）

- NativeSync 第一阶段：Cloud Files API 占位符、按需下载、释放空间、同步资源浏览与管理台。
- Pandock 品牌与图标体系、统一设置页视觉、托盘与菜单修正。
- Windows 开机启动改用用户 Startup 快捷方式。
- 安装与更新通道迁移到 Velopack（ADR-106）：无向导 Setup.exe、固定用户数据目录、安装版应用内自动更新。
- portable 与安装版双产物；portable 数据目录保持在 exe 旁 `data\`。
- 首次启动自动打开设置页，落地即见百度网盘连接入口。

### 1.1.1（2026-10-03）

- 修正便携包命名与 Velopack 上传约定。
- 完成 1.1.0 → 1.1.1 双通道应用内更新验证：
  - Setup.exe 安装版：检查、下载校验、应用并重启成功；
  - portable 版：检查、下载校验、helper 就地替换、journal 回滚能力验证成功。
- 清理未发布的 NSIS 选型与旧 minisign 更新管线，保留 portable helper/journal 机制。

## 当前状态

- 主通道：Velopack Setup.exe + 规范 portable ZIP，同一 GitHub Release 提供两种产物。
- 质量门禁：fmt、clippy、Rust 测试、核心覆盖率、UI 测试、UI 构建、密钥扫描。
- 当前核心行覆盖率：87.29%；UI 测试：40/40。

### 1.2.0（2026-10-10）

- 开源迁移：公开仓 [weyham/pandock](https://github.com/weyham/pandock) 上线（无历史单初始提交，tree-hash 构造性比对验证），旧仓归档，License MIT。
- ADR-104 更名收尾：pandock.exe / pandock-updater.exe、com.weyham.pandock、keyring 服务名迁移与旧 exe 首 run 清理。
- 更新链路零 API 化：GitHub App 授权流程整体移除，更新走 Releases CDN 匿名路由（实测通过）。
- GitHub Actions CI 上线：windows / coverage / macos-check 三门禁全绿；release 流水线配人工审批闸门。
- macOS 首发：WebDAV 模块（未公证 dmg，手动放行）；NativeSync 不提供（CFAPI 为 Windows 独占）。
- SignPath OSS 签名申请推进中；获批前产物未签名（SmartScreen 警告照旧）。

### 1.2.1 之后（NativeSync 外壳集成，进行中）

- 同步根改用外壳注册（ADR-108）：资源管理器出现同步根节点、状态角标、状态列，以及
  「始终保留在此设备上」「释放空间」原生动词；外壳注册与引擎连接同生命周期，连接失败自动回滚。
- 修正状态列长期显示「同步挂起」：占位符创建、水合完成、路径已存在时标记 in-sync。
- 「释放空间」改为先脱水再标记 in-sync。
- 同步时回读 `FILE_ATTRIBUTE_PINNED` / `FILE_ATTRIBUTE_UNPINNED`，修正固定状态错位。
- `remote_entry` upsert 兼容 `remote_path` 唯一约束，消除「目录建出但文件缺失」的同步中断。
- 已知限制：本地新建/编辑/删除/改名不上传云端（单向 remote -> local），原因见 ADR-108。
- 已知限制：本地新建/编辑/删除/改名**已实时捕获**（`pendingEntries` / `pandock://nativesync/change`），但仍不上传云端（S1 单向），见 ADR-109；视觉呈现待 UI 小切片。
- UI 小切片（后续，本轮不做）：把「部分在本地」（`0 < OnDiskDataSize < 文件大小`）作为**独立的可见状态**呈现。
  背景：`sync_now` 对账对部分区间只落 warn、**不改状态**（S1 硬规则：部分区间不得标记 hydrated），
  若界面长期显示旧状态，用户看到的状态会与磁盘不符。
- 行为变更（方案甲，2026-10-12）：**不注册** `NOTIFY_DEHYDRATE` / `_COMPLETION`（官方文档允许不实现）。
  平台自行完成外部/系统发起的脱水，脱水事实由 `sync_now` 磁盘对账取得；
  代价是失去「即将脱水」的拦截点，故新增守卫：**有未同步本地改动 ⇒ 标 `NOT_IN_SYNC`**（平台据此拒绝脱水）。
  **边界**：S1 无上传方向，`NOT_IN_SYNC` 可能**长期保持**（如实表示「有未同步改动」），S2 上传成功后清回 `IN_SYNC`。
- **更正（2026-10-12 迭代 2 实测，已收手）**：**外部/第三方进程发起的 `CfDehydratePlaceholder` 不可用**，
  与是否注册回调无关——三种形态：只 ACK ⇒ 无限等待；提供程序在回调内完成 ⇒ 能完成但 **53,597 ms**；
  不注册（甲，当前部署）⇒ 无限等待（>2 分钟，无 dehydrate 日志）。
  **受支持路径 = 应用内「释放空间」**（已实测成功，`OnDisk=0`）。**本条不再占用部署额度**，
  如要再碰须另立任务（候选：`CfOpenFileWithOplock` 之类特殊打开方式、shell verb 的另一条通道）。
- 后续单独调研（不阻塞当前交付）：**外部/系统发起的「释放空间」**。现有三条证据：
  ① 回调内联完成脱水实测 53.6 秒；② ACK-only 时调用方无限等待；③ 官方文档说该回调可选。
  目标：找到既不阻塞调用方、又能让平台真正释放空间的形态（候选：ACK 先行 + 后台异步脱水 + 事后对账）。
- 自愈：占位符被平台标成未同步（蓝箭头、右键动词变灰）时，`sync_now` 在「无本地改动且内容与云端一致」的前提下自动 `CfSetInSyncState(IN_SYNC)` 恢复；并常驻低噪探测 in-sync 状态。
- 行为变更（ADR-110）：应用**正常退出不再注销同步根**（注销会把整棵树的占位符转成普通文件、未水合的会被本地永久删除）。代价是应用关闭期间阅读该目录会报「云文件提供程序未运行」，不再退化为普通目录；已存在但非占位符的路径由「立即同步」就地自愈回占位符。
- 退役独立的「同步资源浏览」窗口（ADR-108 之后资源管理器已能呈现同步根节点/状态列/图标，窗口属重复建设）：
  托盘菜单去掉其入口、双击托盘一律打开设置页；**per-entry 操作（释放空间/固定/取消固定/重试/已知悉/打开根目录）与状态计数迁移到设置页 NativeSync 区块**（平铺「可操作条目」列表：已下载/已固定/需处理 + 目录下钻）——
  应用内「释放空间」是受支持路径，迁移后仍可用；`native_sync_list` 增加可选参数 `filter: "actionable"`（additive）。
## 1.3.0 候选

- NativeSync 双向同步与冲突处理。
- Explorer 稀疏包原生集成（依赖 SignPath 签名获批）。
- winget / scoop 分发。
- 多语言界面（中文优先，英文跟进）。
- macOS NativeSync（File Provider，需 Apple 开发者账号，远期）。

- 工具任务（后续，**不阻塞本次部署**）：⑤ Cloud Files 元数据/写操作的**权限验证 harness**（独立 ProviderId + `%TEMP%` 临时根 + 本进程内，按 A–E 阶段逐步断言"对已脱水文件做元数据调用后 `OnDisk` 仍为 0"）。可复用脚手架：`nativesync/examples/register_sync_root.rs`（`:74` 的 `StorageProviderSyncRootManager::Register`）+ `diag_probe.rs` 的读取代码；**不用** `CfRegisterSyncRoot`（ADR-108：混用会 `0x8007018B`）。当前判断：其独有增量价值已被官方文档（句柄权限）+ 验收路径（脱水后 `mark_in_sync` 的真实副作用可被外部探针立刻观测）覆盖，故降级为后续工具。

### 1.3.0（2026-10-13）

- NativeSync S1 单向同步真实机验收收口（应用停止快速失败 23ms、重启后按需水合 1071ms 且内容逐字节一致、删除/改名捕获、自愈）。
- 退役「同步资源浏览」独立窗口：per-entry 操作迁移到设置页 NativeSync 区块（可操作条目列表 + 目录下钻）；托盘菜单四项、双击托盘开设置页。
- 部署脚本硬化（pre-flight 语义化 + 停后轮询解锁 + 失败先恢复服务）；`native_sync_list` 增 `filter="actionable"`（additive）。

### 1.4.0（2026-10-14，已发布）

### 1.4.1（2026-10-14，已发布）

- Windows 构建 GNU → MSVC 迁移（ADR-113）：WebView2 静态链接，产物不再携带 WebView2Loader.dll；/Brepro 确定性构建；产物约减半；本地门禁恢复 app lib 测试。
- delta 更新包暂不引入（feed 消费侧只认 Full；若引入需同步改 elopack_feed.rs 与 release.yml——体积优化项，非缺陷）。

- **S1.5 无感同步（ADR-111，已终审已上线）**：云变 → 本地自动跟上（首层 60s、之后 10 分钟、退避封顶 60m、连续失败 5 次暂停；设置页「上次对账」状态行 + `native_sync_auto` 开关；「立即同步」降级为「立即对账（诊断）」）。
- **S2 上传方向（ADR-112，已终审已上线）**：本地新建/编辑/删除/改名自动传播云端（本地侧对账捕获 + 3s 防抖 + 串行上传队列 + uploadid 断点续传）；冲突按 OneDrive 语义保留双方（`<原名>-<设备名>` 副本回传、原名进需处理）；删除传播至百度回收站（已实测可恢复）；回声抑制（上传回执即时回写基线）；`native_sync_upload` 总开关（默认开）。
- 真实机验收记录与 10 缺陷发现史：`docs/validation.md`「S2 上传方向（双向同步）真实机验收」。

#### 已知边界与后续排队（S2 收口时落档）

- **Gap D**：已上传文件仍是本地普通文件，其删除/改名暂不传播（候选：上传成功后转回占位符，待评估）。
- 第三形态文件（recall+write 后）`CfSetInSyncState` 失败 ⇒ 防脱水标记打不上的残余风险（待评估如何恢复完整占位符身份）。
- **-6/令牌失效应触发重新授权引导**（现为死重试）；多实例/脚本共享单例轮换令牌链的共存策略（2026-10-14 令牌链碰撞事故落档）。
- **ReadDirectoryChangesW 近实时监听**（上传提速到秒级；本地侧对账做兜底）。
- 自愈回位条目的 `remote_missing` 外观残留清理（upsert 复活行时按磁盘真值清状态）。
- **`sync.db` 的 `.bak` 备份是空壳**（不含 WAL）⇒ 改用 SQLite 在线备份（高优先）。
- WebDAV 目录列举新鲜度：list 延迟导致刚写入/删除的文件在目录视图延迟出现/消失（实证实验进行中，写穿补偿层设计卡待评审）。
- 2026-10-14 实验结论（负结果）：3 轮 PUT/DELETE + MOVE 均亚秒级在目录列举可见，未复现延迟 ⇒ **暂不实现写穿补偿层**；上游滞后定性为间歇性，NativeSync 的 stat 确认守卫继续保留（见 docs/validation.md）。