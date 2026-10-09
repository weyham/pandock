# 安全政策

## 报告安全问题

如果你发现 Pandock 的安全问题（尤其是可能泄露凭据、用户数据或云端文件的问题），**请不要提交公开 Issue**。

请通过 GitHub 的 [Private vulnerability reporting](https://github.com/weyham/pandock/security/advisories/new) 私密报告，或直接联系仓库所有者。我们会在确认后尽快响应。

## 支持范围

- 仅维护默认分支上的最新版本；
- 旧版本不承诺安全修复回填，请始终升级到最新 Release。

## 报告时请提供

- 受影响版本或提交；
- 可复现步骤；
- 影响范围；
- 已脱敏的日志或请求（**切勿包含 token、密码或授权头**）；
- 可行的缓解建议（如有）。

## 安全模型概要

- 百度 App Secret、OAuth Token、WebDAV 密码均存入系统凭据库（Windows 凭据管理器 / macOS 钥匙串），不写入配置文件；
- 配置接口不会把已保存的密钥返回给前端；
- 日志与诊断输出不包含授权头、Token 或密码；
- WebDAV 默认仅监听回环地址 `127.0.0.1`；监听非回环地址时强制要求用户名密码；
- 云端删除、移动、覆盖只由用户主动发起的 WebDAV 请求执行；
- 上传经由本地临时文件完成，失败不会把未完成内容标记为成功；
- 应用内更新校验 Velopack feed 中的 SHA-256 与文件大小，便携版更新失败自动回滚。

详见 [docs/security.md](docs/security.md)。
