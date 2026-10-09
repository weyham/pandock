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

## 1.2.0 计划（开源迁移，方案已批准 2026-10-10）

详见 docs/release/1.2.0-open-source-migration-plan.md 与 ADR-107。

- 迁移至公开仓 weyham/pandock（无历史单初始提交，旧仓归档），License MIT。
- 更新链路零 API 化：删除 GitHub App 授权，改走 releases/latest/download CDN 路由。
- GitHub Actions CI/CD（托管 runner、SHA pin、release 人工闸门）。
- SignPath OSS 签名申请（Pandock + Halcyon 联合）；获批前接受未签名发布。
- macOS 首发：仅 WebDAV 模块，未公证分发，NativeSync 不提供。

## 1.3.0 候选

- NativeSync 双向同步与冲突处理。
- Explorer 稀疏包原生集成（依赖 SignPath 签名获批）。
- winget / scoop 分发。
- 多语言界面（中文优先，英文跟进）。
- macOS NativeSync（File Provider，需 Apple 开发者账号，远期）。
