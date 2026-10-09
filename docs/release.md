# 本地发布

## 发布策略

项目以公开仓 `weyham/pandock` 的 GitHub Releases 分发。当前由维护者在本地执行检查与构建（`scripts/check.ps1` → `scripts/build-velopack.ps1`）；GitHub Actions 远端流水线（含 release 人工闸门）在 1.2.0 批次 E 落地后成为主通道。

## 版本要求

发布前统一更新版本：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\set-version.ps1 -Version 1.1.1
```

版本必须与以下位置一致：

- 根 `Cargo.toml`；
- `src-tauri/tauri.conf.json`；
- 根 `package.json`；
- `ui/package.json`；
- `ui/package-lock.json`。

发布前运行：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\check.ps1
```

## 构建期环境变量

1.2.0 起更新链路零 API 化（ADR-107）：发布构建**不再需要**任何 GitHub 凭据或
`option_env!` 注入。更新检查与下载全部走公开仓的
`releases/latest/download/` CDN 路由。

严禁把任何 token、Client Secret、签名私钥写入命令行、脚本、文档或仓库。
代码签名（SignPath）获批前产物不签名；获批后签名步骤必须位于
`vpk pack`（生成 releases.win.json）**之前**，否则 feed 内哈希与制品不符。

## Windows x64 发布（Velopack 安装包 + 便携包）

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-velopack.ps1
# 上传 GitHub Releases（默认草稿）：加 -Upload；直接发布：再加 -Publish
```

脚本执行顺序：

1. 构建 UI、`pandock-app`（`--features custom-protocol`）与 `pandock-updater`；
3. 执行 Windows 资源校验；
4. `vpk pack` 产出 `Pandock-win-Setup.exe`（无向导安装）、full nupkg、`releases.win.json`；
5. 构造规范便携包 `Pandock-win-Portable.zip`（自带 `data\README.txt` 与 `updater.exe`，触发 portable 数据目录与应用内自更新）；
6. 生成两个产物的 `.sha256` 校验文件。

产物位于 `dist\velopack\`（不提交）。`vpk pack` 的打包目录只包含 `pandock.exe`、`WebView2Loader.dll`、`updater.exe`，严禁混入 `target` 中间产物。

发布构建使用独立的 `target/release-package`，避免正在运行的开发实例锁定默认 `target/release`。`--no-insert-timestamp` 链接参数保持启用。

## 发布前人工验收

- Setup.exe：双击后品牌闪屏完成安装并启动，首启自动打开设置；
- 便携 ZIP：干净目录解压后数据写入 exe 旁 `data\`；
- 验证系统托盘、授权、重启恢复和退出；
- 验证 WebDAV 基础读写和错误状态；
- 验证包内不存在 `config.json`、Token、日志、缓存或测试文件；
- 在上一正式版本上执行应用内更新并确认版本、数据保留与回滚能力；
- 记录 SHA-256；
- 如对外分发，完成代码签名和时间戳。

## macOS

在 macOS 构建机上执行：

```bash
npm --prefix ui ci
npm --prefix ui run build
cargo tauri build --bundles app,dmg
```

正式分发需要 Apple Developer ID 签名和公证。签名材料不得提交到仓库。

## 回滚

- 代码回滚：恢复到上一个已验证提交。
- Velopack 安装版：由 Velopack 保留的上一版本目录和 `Update.exe` 支持回滚。
- 便携版：由 `updater.exe` 的 journal 备份支持回滚。
- 数据回滚：配置写入前保留备份；凭据由系统凭据库管理。
- 如发布后发现高危问题，撤下制品并阻止继续分发，再修复和重新构建版本号。