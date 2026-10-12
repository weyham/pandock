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
- 自愈：占位符被平台标成未同步（蓝箭头、右键动词变灰）时，`sync_now` 在「无本地改动且内容与云端一致」的前提下自动 `CfSetInSyncState(IN_SYNC)` 恢复；并常驻低噪探测 in-sync 状态。
- 行为变更（ADR-110）：应用**正常退出不再注销同步根**（注销会把整棵树的占位符转成普通文件、未水合的会被本地永久删除）。代价是应用关闭期间阅读该目录会报「云文件提供程序未运行」，不再退化为普通目录；已存在但非占位符的路径由「立即同步」就地自愈回占位符。
## 1.3.0 候选

- NativeSync 双向同步与冲突处理。
- Explorer 稀疏包原生集成（依赖 SignPath 签名获批）。
- winget / scoop 分发。
- 多语言界面（中文优先，英文跟进）。
- macOS NativeSync（File Provider，需 Apple 开发者账号，远期）。

- 工具任务（后续，**不阻塞本次部署**）：⑤ Cloud Files 元数据/写操作的**权限验证 harness**（独立 ProviderId + `%TEMP%` 临时根 + 本进程内，按 A–E 阶段逐步断言"对已脱水文件做元数据调用后 `OnDisk` 仍为 0"）。可复用脚手架：`nativesync/examples/register_sync_root.rs`（`:74` 的 `StorageProviderSyncRootManager::Register`）+ `diag_probe.rs` 的读取代码；**不用** `CfRegisterSyncRoot`（ADR-108：混用会 `0x8007018B`）。当前判断：其独有增量价值已被官方文档（句柄权限）+ 验收路径（脱水后 `mark_in_sync` 的真实副作用可被外部探针立刻观测）覆盖，故降级为后续工具。