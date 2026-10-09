# ADR-101：私有 GitHub Release + 应用内 GitHub 授权

> **状态：已被取代**。私有发布通道设计由 ADR-106（Velopack 更新通道）与
> ADR-107（开源迁移：公开仓 + 零 API CDN 更新）取代。本文仅作历史决策留存。


- 状态：修订定稿，待开发总监复审
- 日期：2026-09-24
- 关联：Stage B、`docs/release/1.0.0-stage-b-updater-plan.md`
- 决策：使用同一私有源码仓库 `weyham/baidupan-shim` 的 GitHub Releases 发布更新制品，客户端通过 GitHub App user authorization（Device Flow）读取私有 Release。
- 更新实现边界：只修订 `UpdateSource` 的认证/传输层，不改变 manifest、minisign/Ed25519、SHA-256、防降级、helper 或回滚规则。

## 背景

CloudDock 的源码仓库 `weyham/baidupan-shim` 必须保持私有；更新用户需要由仓库所有者邀请为该仓库的只读协作者。应用需要从该仓库检查版本、读取 `latest.json` 并下载签名制品，且不能把 PAT、GitHub App private key、client secret 或长期共享凭据编译进客户端。

原先的公开 Release、私有 Release + 登录、私有服务和手动更新四个选项，现由开发总监裁决为：

- 源码仓库继续私有；
- 不创建独立私有二进制仓库 `weyham/clouddock-releases`；更新制品直接发布在 `weyham/baidupan-shim` 的 GitHub Releases；
- 用户由仓库所有者邀请为该私有源码仓库的只读协作者；
- 应用内完成 GitHub 授权、刷新、退出和重新授权；
- macOS 1.0.0 只检查更新并提示从私有 Release 手动下载，不自动替换；
- Windows portable 使用同一 `UpdateSource` 检查、下载并进入既有签名/helper 更新流程。

## 决策

### 1. 使用 GitHub App，而不是 OAuth App

认证路径使用 GitHub App 的 user access token，并通过 GitHub App Device Flow 获取。

选择原因：

- GitHub App user access token 使用细粒度权限，不使用传统 OAuth scopes；
- 私有 Release 读取/资产下载所需的 GitHub 官方最小权限是 `Contents: read`；
- Device Flow 适合桌面应用；GitHub App 可在设置中启用 Device Flow；
- Device Flow 生成 token 时不需要把 client secret 放进公共客户端；该 token 的 refresh 请求也不需要 client secret；
- GitHub App 的权限与仓库选择由 App 安装范围控制，不会让用户授权整个账号下的所有私有仓库；
- token 是短期凭据，可刷新、可撤销，并且能够降低凭据泄露影响。

GitHub App 安装范围必须只选择 `weyham/baidupan-shim`，且权限只保留 `Contents: read`。用户仍必须是该私有仓库的只读协作者，因为 user access token 的有效范围是“App 能访问的资源”和“用户能访问的资源”的交集。

### 2. 不采用 OAuth App Device Flow 作为主方案

OAuth App 的 Device Flow 实现简单，但访问私有仓库需要 `repo` scope。GitHub 官方对此 scope 的定义是：对公有和私有仓库的完全访问，包括代码读写、提交状态、邀请、协作者、部署状态和 webhook；还可能访问组织项目、邀请、团队成员和 webhook。

该授权面显著大于“读取一个私有 Release 仓库”的需求，因此不作为正式方案。GitHub App Device Flow 的 `scope` 为空，权限由 GitHub App 配置和用户权限交集决定。

### 3. Release 仓库和资产布局

发布对象固定在同一私有源码仓库的 GitHub Releases，不创建独立二进制仓库：

~~~text
weyham/baidupan-shim
~~~

仓库只存放：

- GitHub Release；
- 已签名可执行文件/便携包；
- `latest.json`；
- `latest.json.minisig`；
- 制品签名；
- SHA-256 校验值；
- 发布说明。

源码仓库不向未受邀用户开放；更新用户必须获得该仓库只读访问权限。客户端不包含仓库凭据。

### 4. 客户端凭据边界

允许编译进客户端：

- GitHub App client ID；它是公开标识，不是 client secret；
- minisign/Ed25519 公钥；
- API 版本、仓库 owner/repo 和协议常量。

禁止编译、写入配置或进入事件：

- PAT；
- GitHub App private key；
- GitHub App client secret；
- 长期共享更新服务密钥；
- GitHub access token 或 refresh token（运行时只写入系统凭据库）。

### 5. Token 生命周期与失效处理

- access token 默认 8 小时过期；refresh token 默认 6 个月过期；客户端应显式启用 user-to-server token expiration；
- 使用 refresh token 刷新 access token 时，GitHub 同时签发新的 refresh token；旧 refresh token 与旧 access token 立即失效；
- 在过期前预留刷新窗口，并做单飞刷新，避免并发重复刷新；
- 遇到 `bad_refresh_token`、401 或用户撤销授权时，清除本应用保存的 access/refresh token，要求用户重新完成 Device Flow；
- 网络错误、超时或离线不清除 token，也不反复弹出授权窗口；
- 403/404 需区分“限流”“无权限/无协作者”“仓库/资产不存在”；
- 组织如果启用 SAML SSO，用户可能需要先建立 SSO 会话再重新授权。

### 6. Release API 传输规则

固定使用 GitHub REST API，不把私有 `browser_download_url` 当作匿名地址：

1. 使用 `GET /repos/{owner}/{repo}/releases/latest` 或按 tag 获取 Release 元数据；
2. 从 Release 资产列表定位 `latest.json`、对应签名和平台制品；
3. 读取 manifest 后先验证 minisign/Ed25519 签名，再读取其中的版本和制品信息；
4. 使用 `GET /repos/{owner}/{repo}/releases/assets/{asset_id}` 下载资产；
5. 设置 `Accept: application/octet-stream`，正确处理 `200` 和 `302`；
6. 跨域重定向时移除 `Authorization` header，只允许 HTTPS 和受控重定向链；
7. 下载后继续执行既有 SHA-256、helper 哈希、安全解压、白名单、防降级和 journal 流程。

### 7. 限流、缓存与离线

- GitHub App user access token 的 REST 主限额按已认证用户计算，通常为每用户每小时 5,000 次；以响应头 `x-ratelimit-*` 为准；
- 更新检查必须低频，并使用 `ETag`/条件请求避免重复传输未变化元数据；
- 命中 403/429 且 `x-ratelimit-remaining: 0` 时，按 `x-ratelimit-reset` 或 `Retry-After` 退避，不进行紧密重试；
- 离线或网络不可达只标记“更新检查延迟”，不清 token、不把错误伪装成授权失效；
- 本地已有且签名校验通过的 staged 制品可以继续按独立更新流程处理；离线时不能获取新的 manifest 或资产。

### 8. macOS 与 Windows 边界

Windows portable：

- 使用私有 GitHub Release 的认证读取；
- 下载、验签、SHA-256、helper 哈希、替换、readiness 和回滚全部沿用既有契约。

macOS：

- 使用同一 UpdateSource 检查私有版本；
- 在 UI 中显示版本和发布说明；
- 打开私有 Release 页面，由用户手动登录、下载和替换；
- 1.0.0 不自动下载，也不自动替换 `.app`。

## 被否决或降级的替代方案

| 方案 | 结论 | 原因 |
|---|---|---|
| OAuth App Device Flow + `repo` scope | 不采用 | scope 过宽，超出只读私有 Release 的最小权限 |
| GitHub App Web application flow | 不作为 1.0.0 主路径 | 桌面端需要回调和浏览器流程；Device Flow 更适合无服务端客户端 |
| 私有更新服务/对象存储 | 不采用 | 需要额外部署、认证和持续运维，当前无服务端 |
| 手动私有更新 | 只作为无网络/故障退路 | 不满足应用内检查体验，且仍要求 GitHub 账号和仓库访问权限 |
| PAT 或入包 client secret | 禁止 | 可被提取，违反客户端无长期凭据约束 |

## 后果

正面：

- 源码和二进制发行物均保持私有；
- 更新用户必须获得 `weyham/baidupan-shim` 的只读访问权限；不再通过独立二进制仓库做访问隔离；
- 客户端不嵌入长期凭据；
- 使用 GitHub App 的细粒度 `Contents: read`；
- 签名、校验和回滚仍独立于认证/传输层；
- 未来可替换为私有服务源而不改变更新核心。

代价与风险：

- 用户必须有 GitHub 账号，并接受私有仓库协作者邀请；
- 首次授权依赖 GitHub 网络和 Device Flow；
- 用户撤销授权、token 过期或 GitHub 变更 API 时会中断更新；
- 私有仓库 Release 依赖 GitHub 服务可用性；
- 需要处理 Device Flow 轮询、refresh 轮换、SAML SSO 和限流退避。

## 官方依据

- [Generating a user access token for a GitHub App](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app)：GitHub App device flow、无 scope、8 小时 token、撤销后的 `401 Bad Credentials`；
- [Refreshing user access tokens](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/refreshing-user-access-tokens)：access token 8 小时、refresh token 6 个月、刷新后旧 token 失效、Device Flow 生成 token 刷新不要求 client secret；
- [Scopes for OAuth apps](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/scopes-for-oauth-apps)：`repo` scope 的过宽权限；
- [Best practices for creating a GitHub App](https://docs.github.com/en/apps/creating-github-apps/about-creating-github-apps/best-practices-for-creating-a-github-app)：最小权限、客户端凭据和 Device Flow/PKCE 风险说明；
- [REST API endpoints for release assets](https://docs.github.com/en/rest/releases/assets)：`Contents: read`、`Accept: application/octet-stream`、处理 `200/302`；
- [Rate limits for the REST API](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api)：user access token 的 5,000 次/小时基准、`x-ratelimit-*` 和退避规则；
- [Token expiration and revocation](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/token-expiration-and-revocation)：用户撤销和应用侧失效处理。
