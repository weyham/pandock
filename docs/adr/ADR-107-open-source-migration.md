# ADR-107：开源迁移与公开化技术决策

状态：已批准（2026-10-10）
关联：ADR-106（Velopack 更新通道，部分条款被本决策修订）
详细执行计划：docs/release/1.2.0-open-source-migration-plan.md

## 背景

Pandock 从私有仓转为开源项目。公开化影响更新通道、发布协议、签名策略、
分发平台范围与文档边界。

## 决策

1. **迁移方式**：新建公开仓 `weyham/pandock`，无历史单初始提交；旧仓
   `weyham/baidupan-shim` GitHub Archive 归档。仓名去除 baidupan 商标词。
2. **License**：MIT，`Copyright (c) 2026 weyham`，与 Halcyon 统一。
3. **文档边界**：过程性文档（问题与解决、release 过程报告、迁移计划）剥离到
   本地 internal-docs 仓；公开仓仅保留面向外部的文档与清理后的 ADR。
4. **更新通道零 API 化**：删除 GitHub App Device Flow 整套（授权 UI、keyring
   token、编译期 client id），更新检查与下载全部走
   `releases/latest/download/` CDN 路由；Velopack GithubSource 改 HttpSource。
   理由：公开仓 Release 匿名可读；api.github.com 匿名限额 60 次/小时/IP 在
   发布日实测耗尽（Halcyon 实测），零 API 是公开仓必选项。
5. **更新信任链**：维持 HTTPS + Velopack feed SHA256，不恢复 minisign。
   minisign-in-CI 的私钥与 GitHub 账户同属一个信任域，对最大威胁场景
   （账户被盗）无增量防护；残余风险由 2FA、分支保护、release environment
   人工审批兜底。
6. **代码签名**：申请 SignPath Foundation OSS 免费签名（Pandock + Halcyon
   联合申请）；获批前接受未签名发布（SmartScreen 警告照旧，Halcyon 先例）。
   获批后接入 CI，并解锁 Explorer 稀疏包集成的交付资格。
7. **macOS 范围**：首发仅 WebDAV 模块，未公证分发（用户手动放行）；
   NativeSync 不提供（CFAPI 为 Windows 独占；macOS 对应物 File Provider
   需付费 Apple 开发者账号与 App Extension 架构，远期单独立项）。
   nativesync crate 做 Windows-only 编译门控。
8. **存量迁移**：旧仓 1.1.1 客户端用户唯一（开发者本人），不发布过渡版本，
   手动安装 1.2.0。

## 后果

- "关于"页失去 GitHub 连接管理功能，简化为版本 + 检查更新。
- 更新可用性依赖 GitHub Releases CDN 稳定性，无 API 限额风险。
- 签名获批前，Setup.exe 仍有 SmartScreen 警告；Explorer 原生集成保持搁置。
- macOS 用户无自动更新，仅新版本提醒 + 手动下载。
- 旧仓归档后，1.1.x 历史 release 留在旧仓，不再维护。
