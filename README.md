# Pandock

Pandock 是一款常驻系统托盘的开源桌面工具，把百度网盘开放平台的应用目录带到你的本地工作流：

- **WebDAV 桥接**：把网盘应用目录映射为本地 WebDAV 服务，文件管理器、RaiDrive、rclone 等任意 WebDAV 客户端均可挂载；
- **NativeSync（Windows）**：基于 Windows Cloud Files API 的占位符同步——文件按需下载、可释放空间、带状态角标，并内置同步资源浏览窗口。

```text
文件管理器 / WebDAV 客户端              Windows 文件资源管理器
            |                                   |
        WebDAV 协议                        Cloud Files 占位符
            |                                   |
            +-----------+   Pandock   +---------+
                        |
              百度网盘开放平台 API
                        |
              /apps/<应用名称>/
```

> 本项目与百度无关，不是百度网盘官方客户端。详见文末「免责声明」。

## 功能

- Windows 系统托盘常驻，设置窗口集中管理连接、WebDAV 与同步选项。
- 百度开放平台设备码授权：扫码或访问设备码网址完成连接，无需本地回调服务。
- WebDAV：浏览、新建目录、上传、下载、Range 读取、移动、复制、删除；默认监听 `127.0.0.1:19090/dav/`。
- NativeSync（Windows）：占位符按需下载、固定到本地、释放空间、同步状态浏览与冲突提示。
- 应用内检查更新：安装版（Velopack）自动下载安装；便携版就地替换并带失败回滚。
- 凭据安全：App Secret、授权令牌、WebDAV 密码均存入系统凭据库，不写入配置文件。
- 开机启动为显式选项，默认关闭。

## 平台支持

| 平台 | WebDAV | NativeSync | 更新 |
|---|---|---|---|
| Windows 10/11 x64 | ✅ | ✅ | 应用内自动更新 |
| macOS | ✅ | 不提供（CFAPI 为 Windows 独占） | 新版本提醒 + 手动下载 |

macOS 可使用 Finder「前往 → 连接服务器」（⌘K）直接挂载 Pandock 的 WebDAV 地址。

## 快速开始

### 1. 准备百度开放平台应用

Pandock 不内置任何百度开发者凭证，你需要使用自己的百度账号创建一个开放平台应用：

1. 登录 [百度网盘开放平台](https://pan.baidu.com/union/main/nest#)（开发者中心）；
2. 创建应用，类型选择「软件」并开通网盘能力；
3. 记下 **App Key**、**Secret Key** 和 **应用名称**。

开放平台应用默认只能访问网盘中它自己的目录 `/apps/<应用名称>/`，Pandock 不会访问该目录之外的任何数据。

### 2. 安装

前往 [Releases](https://github.com/weyham/pandock/releases) 下载：

- **Pandock-win-Setup.exe**：安装版（推荐），支持应用内自动更新；
- **Pandock-win-Portable.zip**：便携版，解压即用，数据保存在 exe 旁的 `data\` 目录；
- macOS：`Pandock-macos-*.dmg`（未公证，首次请在 Finder 中右键 →「打开」放行）。

> Windows 安装包当前未做 Authenticode 签名，SmartScreen 可能提示「Windows 已保护你的电脑」，点「更多信息 → 仍要运行」即可。代码签名正在申请中。

### 3. 连接并挂载

1. 启动 Pandock，托盘菜单 →「设置」；
2. 在「百度网盘连接」中填入 App Key、Secret Key、应用名称，点击「连接百度网盘」；
3. 在弹出的对话框中扫码，或打开设备码网址完成授权；
4. 连接成功后，用任意 WebDAV 客户端挂载：

```text
http://127.0.0.1:19090/dav/
```

Windows 资源管理器可直接「映射网络驱动器」；macOS 用 Finder ⌘K。

## 从源码构建

环境：Rust stable、Node.js 20+、Windows 需 WebView2；macOS 需 Xcode Command Line Tools。

```powershell
npm --prefix ui ci
npm --prefix ui run build
cargo run -p pandock-app --features custom-protocol
```

完整本地门禁（格式、clippy、测试、覆盖率、UI 构建、密钥扫描）：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\check.ps1
```

Windows 发布产物（Setup.exe + 便携包 + Velopack 更新包）：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-velopack.ps1
```

## 项目结构

```text
core/           Rust 核心：OAuth、百度 API、资源模型、WebDAV 文件系统
nativesync/     NativeSync 同步引擎（Windows Cloud Files API）
src-tauri/      Tauri 桌面外壳：托盘、单实例、开机启动、系统凭据、更新运行时
updater/        便携版更新 helper（就地替换 + 失败回滚）
ui/             React + TypeScript 设置窗口
scripts/        本地检查、版本管理与发布脚本
packaging/      Velopack 打包资源
docs/           架构、WebDAV、安全、开发、发布与 ADR
```

## 文档

- [架构设计](docs/architecture.md)
- [WebDAV 行为](docs/webdav.md)
- [安全与保密](docs/security.md)
- [开发与测试](docs/development.md)
- [本地发布](docs/release.md)
- [路线图](docs/roadmap.md)
- [决策记录](docs/adr/)

## 免责声明

- Pandock 是独立的第三方开源工具，**与百度公司没有任何关联**，不是百度网盘的官方客户端，也未获得百度的认可或背书。
- Pandock 仅通过百度网盘**开放平台公开 API** 访问你自己创建的应用目录，不内置、不收集任何百度开发者凭证或用户数据。
- 使用 Pandock 产生的一切行为（包括百度账号侧的风险）由使用者自行承担。请在遵守百度网盘开放平台服务条款的前提下使用。
- 「百度网盘」等名称仅作描述性引用，相关商标归其权利人所有。

## 许可证

[MIT](LICENSE) © 2026 weyham
