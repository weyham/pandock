# ADR-106：安装与更新通道迁移到 Velopack

- 状态：接受
- 日期：2026-10-02
- 取代：ADR-101（自研私有二进制发布通道）、ADR-102（便携更新助手）中关于
  传输/校验/替换/回滚的实现约定；GitHub App Device Flow 授权继续保留

## 背景

1.0.0 建立的自研更新通道（`latest.json` + minisign 签名清单 + 自写下载替换 +
journal 回滚 + readiness 握手）完整可用，但存在三个问题：

- 安装侧长期只有 portable zip，1.1.0 尝试的 NSIS 向导本质仍是 Win32 对话框，
  皮肤化后仍无法达到现代安装体验（无动画、系统边框、老旧控件）；
- 自研更新管线的维护成本高：签名清单、暂存目录、helper 进程、就绪握手、
  journal 恢复，每一块都是自写代码和自写测试；
- 行业趋势是「无向导安装」（Chrome/Omaha、Discord/Slack/Squirrel、Velopack），
  对本项目这种单 exe 应用，向导本身就是多余交互。

## 决策

安装与更新通道整体迁移到 Velopack（MIT，活跃维护）：

1. **安装**：`vpk pack` 产出 `Pandock-win-Setup.exe`，双击后品牌闪屏即装，
   无向导；安装到 `%LOCALAPPDATA%\Pandock\`（`current\` 链接 + 版本目录 +
   `Update.exe`），每用户、免 UAC。
2. **更新**：应用内保留 GitHub App Device Flow 授权（不内嵌任何 token），
   授权拿到的用户 token 直接传给 Velopack `GithubSource`；
   发现/下载/校验/原子替换/重启全部由 Velopack `UpdateManager` 完成。
   minisign 清单、自写暂存/替换、journal 回滚、readiness 握手随之退役。
3. **启动钩子**：`main()` 首先调用 `velopack::VelopackApp::build().run()`，
   处理 `--veloapp-*` 生命周期参数与 Setup 完成握手。
4. **数据目录**：按优先级解析（ADR 决策独立于 Velopack）：
   `--data-dir` 参数 > `PANDOCK_DATA_DIR` 环境变量 > exe 旁 `data\`（portable）
   > `%APPDATA%\Pandock`（Velopack 安装的固定用户目录）。
   Velopack 包不含 `data\`，自然落到固定目录；portable zip 自带 `data\`，
   行为与 1.x 历史一致。
5. **非 Velopack 安装**（portable/开发目录）复用同一份发布产物自更新：
   读取 Release 中的 `releases.win.json` 清单，下载同一个 full nupkg，
   用清单中的 SHA256 + 文件大小校验完整性，仅抽取
   `clouddock.exe` / `WebView2Loader.dll` / `updater.exe` 三个应用文件
   （刻意不带入 Velopack 管道文件，避免 portable 目录被误判为安装版），
   再经 ADR-102 的 journal + helper 机制就地替换并保留回滚。
6. **发布**：`scripts/build-velopack.ps1` 构建 + `vpk pack`，
   `-Upload` 上传 GitHub Releases（默认草稿，`-Publish` 直接发布）。
   NSIS 脚本已整体移除——它从未进入任何已发布产物，属于废弃选型，不在仓库保留。

## 影响

- `CLOUDDOCK_UPDATE_PUBLIC_KEY` 不再注入二进制；minisign 密钥对仅存档，
  如未来对 Setup.exe 做代码签名再另行评估（signtool / Azure Trusted Signing）。
- 回滚语义变化：Velopack 保留上一版本目录，回滚由 `Update.exe` 支持；
  旧 `app\updates\` 下的自研回滚数据不再使用。
- 从 portable 迁移到 Velopack 安装版时，`%APPDATA%\Pandock` 为全新数据目录，
  需要重新完成一次百度授权（凭据在系统凭据管理器中，配置需重新填写或手工
  复制旧 `data\config.json`）。
- 1.1.1 双通道更新 e2e 通过后，旧 manifest/plan/minisign 管线、诊断二进制、
  旧签名工具和旧发布脚本已物理移除；install/journal 保留是因为 portable
  自更新仍在使用 ADR-102 的 helper 交换与回滚机制。

## 验证

- vpk pack → Setup.exe 安装 → 握手退出（exit 0）→ 应用从 `current\` 启动；
- 数据目录落到 `%APPDATA%\Pandock`，`current\` 旁无 `data\`；
- `Update.exe uninstall --silent` 清理彻底；
- 1.1.0 → 1.1.1 安装版应用内更新成功，运行文件哈希与 1.1.1 构建一致；
- 1.1.0 → 1.1.1 portable 应用内更新成功，`data\` 与 WebDAV 状态保留；
- helper/journal e2e 覆盖正常替换、启动失败回滚、readiness 超时回滚与显式失败路径；
- 清理旧管线后 `scripts/check.ps1` 全量通过：核心 102/102、UI 40/40、核心行覆盖率 87.29%；
- 应用内更新 UI 状态契约（UpdateStateView）不变，前端无修改。
