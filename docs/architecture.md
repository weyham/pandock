# 架构设计

## 组件

Pandock 由三个主要部分组成：

1. `pandock-core`：与 UI 无关的 Rust 核心库。
2. `pandock-app`：Tauri 桌面应用，负责进程生命周期、托盘、配置、系统凭据和文件系统接入。
3. `pandock-ui`：React + TypeScript 设置窗口。

## 数据流

```text
WebDAV client
    |
    v
dav-server + WebDAV filesystem adapter
    |
    v
CloudFs contract
    |
    v
Baidu Open Platform API
    |
    v
/apps/<application-name>/...
```

设置窗口仅通过 Tauri command 与桌面层交互。核心 API 不依赖 Tauri，便于独立测试和替换。

## 核心模块

- `auth`：设备码授权轮询和超时、降速处理。
- `baidu`：OAuth、Token 刷新、目录列表、文件元数据、上传、下载、移动、复制和删除。
- `cloud`：远端资源路径、快照、修订标识、错误分类和 `CloudFs` 契约。
- `filesystem`：将 `dav-server` 的文件系统接口转换为云端操作。
- `server`：启动和停止 WebDAV，执行 Basic Auth、请求预检和安全关闭。
- `config`：本机监听、WebDAV 前缀、认证开关和运行配置校验。
- `status`：托盘状态与端到端连通性状态。

## 桌面生命周期

1. 加载本地配置，系统凭据库中的 Secret 不进入配置对象。
2. 如存在 Refresh Token，刷新 Access Token。
3. 校验配置及 WebDAV 认证条件。
4. 确保百度应用目录存在。
5. 启动 WebDAV 服务并进入健康检查循环。
6. 关闭设置窗口时只隐藏；托盘退出时停止服务并释放端口。
7. 第二实例启动时聚焦现有实例。

## 状态规则

托盘状态以端到端连通性为准：

- 灰色：未配置或服务已停止。
- 黄色：等待授权、连接中或暂时断连。
- 绿色：授权有效、WebDAV 已监听且健康检查通过。
- 红色：配置、认证或服务错误。

本地端口监听成功不等于绿色；必须完成云端探针。

## 数据与配置

- Windows portable 模式的运行数据位于可执行文件同目录的 `data/`。
- macOS 运行数据位于应用数据目录，避免写入只读 `.app`。
- App Secret、Refresh Token 和 WebDAV 密码保存在系统凭据库。
- 普通配置文件仅保存非敏感设置。
- 上传临时文件位于运行数据目录的 `uploads/`，在成功或终止清理流程中删除。

## 非目标

- 不代理整个百度网盘，只映射开放平台允许的应用目录。
- 不提供多账号聚合。
- 不在预发布阶段承诺云端事务、跨设备状态或分布式锁。
- 不将具体生产环境配置写入通用代码或文档。
