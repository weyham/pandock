# ADR-105：Windows 开机启动改用用户 Startup 快捷方式

- 状态：接受
- 日期：2026-09-30
- 关联问题：Windows 已存在 `HKCU\...\Run` 项，但登录时未执行；产品更名和 `prod/` 到 `app/` 迁移后还可能出现旧路径残留

## 背景

Pandock 原先使用 `tauri-plugin-autostart` 在 Windows 写入
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。实际验证中：

- Run 项存在且 `StartupApproved` 状态为启用，但 Windows Shell 登录执行记录中没有对应命令；
- CC Switch 的既有 Run 项正常执行，但新建 Run 项不执行；
- 应用配置中的 `launch_at_login` 没有随 UI 开关持久化，产品更名和目录迁移后无法自愈；
- `CloudDock`、`Pandock` 两个名称的历史注册项可能产生歧义。

## 决策

在真实机上进行了两组 A/B 测试：

- Run：C:/F:、有引号/无引号、带/不带 StartupApproved 时间戳的新探针全部没有执行；
- Startup 文件夹：同一批 C:/F: 探针全部执行成功。

因此 Windows 平台改用用户 Startup 快捷方式：

`%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\Pandock.lnk`

快捷方式指向当前可执行文件，参数为 `--autostart`，工作目录为可执行文件所在目录。
启用状态同时写入 `config.json` 的 `launch_at_login`。

应用启动时执行一次协调：

1. 如果配置、现有快捷方式或历史 Run 项表明用户曾启用开机启动，则重建当前路径的快捷方式；
2. 清除 `CloudDock`、`Pandock` 的 Run/StartupApproved 项，避免双重启动；
3. 将迁移后的实际状态回写配置。

macOS 和 Linux 继续使用 `tauri-plugin-autostart`。Windows 不再依赖 Run 项。

## 结果

- 不需要管理员权限，不创建系统级服务、计划任务或 Startup 快捷方式；
- 不依赖本机异常的新 Run 项执行流程；
- 用户可在 Windows“启动应用”列表中看到并管理 `Pandock`；
- 程序移动目录或从旧版本升级后，下一次启动会修复快捷方式目标；
- 首次启用后需要通过一次重启完成真实登录路径验收；
- Windows 实现会调用系统 PowerShell 创建快捷方式并清理旧 Run 项，但不涉及 secrets。

## 验证

- Rust 与 UI 编译、单元测试和构建门禁；
- 迁移后确认 `Pandock.lnk` 的目标路径、`--autostart` 参数和工作目录；
- 确认旧 Run 项已移除且没有重复启动路径；
- 重启后确认进程、托盘图标和 WebDAV 监听同时恢复；
- 失败时保留现有 Run 项不变，并记录日志。
