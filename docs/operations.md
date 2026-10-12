# 运维说明

## 运行模式

Pandock 是本地桌面服务，不提供远程管理接口。默认监听：

```text
127.0.0.1:19090
```

WebDAV 地址：

```text
http://127.0.0.1:19090/dav/
```

## 配置项

- 百度开放平台 App Key；
- 百度开放平台 Secret Key；
- 开放平台应用名称；
- WebDAV 监听地址和路径前缀；
- 是否启用 WebDAV Basic Auth；
- WebDAV 用户名和密码；
- 是否随系统启动；
- 是否在授权后自动启动 WebDAV。

Secret Key、Refresh Token 和 WebDAV 密码存储于系统凭据库。普通配置文件只保存非敏感项。

## 运行数据

- Windows portable：可执行文件同目录下的 `data/`。
- macOS：系统应用数据目录。
- 上传临时文件：`data/uploads/` 或对应平台的应用数据目录。
- 配置替换前保留备份；备份包含非敏感配置，不包含凭据。

不要将 `data/`、日志、配置备份或上传临时目录提交到 Git 或上传到问题跟踪系统。

## 启动和停止

- 关闭设置窗口只隐藏窗口，不停止服务。
- 托盘菜单可以显示设置、停止 WebDAV、重连和退出。
- 退出应用会停止服务并释放监听端口。
- 已经运行时再次启动，只聚焦现有实例。

## 常见问题

### 状态一直是黄色

可能原因：

- 尚未完成设备码授权；
- App Key、Secret 或应用名称无效；
- 网络不可达；
- 云端探针失败。

查看设置窗口的连接信息，但提交问题时必须脱敏。

### 端口被占用

修改监听地址或端口，或停止占用该端口的进程。非本机地址必须启用 WebDAV 认证。

### WebDAV 客户端无法写入

确认客户端使用支持的状态码和 HTTP 方法，并检查：

- 应用目录是否存在；
- Access Token 是否仍有效；
- WebDAV Basic Auth 是否正确；
- 百度开放平台配额和限流状态。

### 重新授权

在设置窗口点击“断开连接”。该操作删除当前 Refresh Token，但保留 App Key、应用名称和非敏感配置。之后可重新发起设备码授权。

## NativeSync 本地变更捕获的紧急开关

NativeSync 会注册删除 / 改名 / 脱水 / 开闭完成的 Cloud Files 回调，用来把本地变更
实时记成「本地已变更、尚未同步到云端」（S1 **不上传**）。若这些回调在某些环境下
导致资源管理器操作卡顿，可以整体关闭捕获——**只读召回（双击打开）与占位符创建不受影响**。

关闭顺序（从最轻到最终手段）：

1. 在 `config.json` 里设 `"native_sync_notifications": false`，重启应用；
2. 若配置无法编辑，改用环境变量（**优先级最高**，用于免编辑恢复）：

   ```powershell
   $env:PANDOCK_NATIVESYNC_NOTIFICATIONS = "0"
   ```

3. 启动日志会打印生效值，可用于确认：

   ```text
   native-sync notifications effective=false config=false env=0
   ```

恢复：删除该环境变量、或把 `native_sync_notifications` 改回 `true`，重启应用。

> 注意：`config.json` 的键一律是 **snake_case**（与既有 `app_key`、`launch_at_login` 一致），
> 不是 camelCase。camelCase（如 `pendingEntries`）只用于 Tauri 事件/命令的 payload。

关闭捕获后，`native_sync_status` 的 `notificationsEnabled` 为 `false`，
`pandock://nativesync/change` 不再有事件；此前已记录的待同步标记仍保留在
`data/native-sync/sync.db` 里，重新开启后继续生效。

## 诊断日志

日志用于定位连接和协议错误，不得记录凭据、完整授权头或用户文件内容。对外提供日志前必须删除账号、路径、Token、Cookie、文件名和 IP 等敏感信息。
