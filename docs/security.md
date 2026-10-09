# 安全与保密规范

本仓库按私有、专有代码库管理。即使代码不公开发布，也按“可能泄露”的标准控制秘密和用户数据。

## 保密分类

允许提交：

- 源代码、测试和构建脚本；
- 不含真实值的配置示例；
- 架构、接口和运维文档；
- 第三方依赖锁文件和公开许可证文本；
- 脱敏后的测试数据。

禁止提交：

- App Secret、Access Token、Refresh Token、Cookie、API Key；
- WebDAV 密码、私钥、证书私钥、签名材料；
- 用户网盘路径、文件名、账号、请求抓包和真实业务数据；
- 生产配置、数据库、日志、备份、上传临时文件；
- 本机绝对路径、用户名、主机清单或内部网络凭据；
- 构建产物、缓存、安装包和压缩包；
- 未经确认许可证的第三方代码或资源。

## 凭据存储

- 桌面端凭据使用 Windows Credential Manager 或 macOS Keychain。
- 普通配置文件仅保存非敏感设置。
- 配置读取接口不得返回已保存的 Secret。
- 更新凭据时采用替换语义，禁止把空输入误解释为删除。
- 日志和错误信息不得回显 Authorization、Token、Secret 或密码。

## 仓库与 GitHub 设置

创建私有仓库后应启用：

- Secret scanning 和 Push protection；
- 分支保护，禁止未评审直接推送 `main`；
- 最小权限的仓库协作者；
- 启用依赖更新和安全公告；
- 对发布签名、云服务和部署凭据使用 GitHub Environment 或外部密钥库。

`1.0.0` 前不启用工作流。后续 CI 只能引用 Secrets，不得把真实值写入 YAML、脚本、缓存或构建日志。

## 本地扫描

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\check-secrets.ps1
```

扫描覆盖私钥、常见云平台 Token、GitHub Token、Bearer Token 和明显的 Secret 赋值。扫描通过不代替人工审查。

## 泄露处置

1. 立即吊销或轮换泄露凭据，优先于清理 Git 历史。
2. 停止相关发布和服务，确认影响范围。
3. 从当前分支移除内容，并检查历史、缓存、制品和日志。
4. 必要时重写历史、强制更新远端并通知协作者重新克隆。
5. 记录根因、修复和防止再次发生的门禁。
