# ADR-104：Pandock 完整更名迁移（内部标识一并更名）

- 状态：**已批准实施**（2026-09-28 开发总监与用户四裁决落档，见 §10）

> **实施状态修订（2026-10-10，ADR-107 关联）**：本 ADR 的更名实施未在 1.1.1 落地
>（1.1.1 实际只交付了打包层命名），**在 1.2.0 开源迁移中完成**：crate 名
>（pandock-core/app/nativesync/updater）、exe（pandock.exe / pandock-updater.exe）、
> Tauri identifier（com.weyham.pandock）、keyring 服务名迁移（读回退 + 一次性复制删除）、
> 日志 target、事件名、环境变量全部落地。因 1.2.0 起更新链在新公开仓重新开始
>（ADR-107 决策 8：存量用户唯一，手动安装），§3.2 的两跳过渡链与双名白名单
> **不再需要并已移除**；首 run 旧 exe 清理保留实施。§5 的同步根目录名裁决修订为：
> 新安装默认 `Pandock`，已启用用户的 db 存根路径不受影响。
- 日期：2026-09-28
- 关联：用户已裁决改版方案乙（连内部标识一起更名）；本 ADR 只覆盖更名迁移设计，不改版功能本身
- 影响面：构建/发布/更新链/凭据/安装目录——高风险，必须先评审后实施

## 1. 背景与裁决

产品更名为 **Pandock**。用户裁决采用方案乙：不仅显示层，连可执行文件名、Tauri identifier、keyring 服务名等内部标识一并更名。风险集中在两点：

1. **更新链**：现有用户安装的是 `clouddock.exe` + 内置更新器白名单替换机制，跨 exe 名的自动升级需要专门设计；
2. **凭据**：keyring 中的百度授权 token 若丢失会导致用户重新走设备码授权，体验不可接受。

## 2. 更名清单（事实核对自当前代码）

| 类别 | 现值 | 目标值 | 位置 |
| --- | --- | --- | --- |
| 可执行文件 | `clouddock.exe` | `pandock.exe` | release 脚本 staging、更新 zip、lib.rs 自重启 `--launch`（硬编码，已核实 lib.rs:915） |
| 更新器 helper | `clouddock-updater.exe` | `pandock-updater.exe` | updater crate（package `clouddock-updater`）、manifest helper.path、zip 条目 |
| Tauri identifier | `com.weyham.clouddock` | `com.weyham.pandock` | tauri.conf.json |
| 产品名/窗口标题 | `CloudDock` / `CloudDock 设置` | `Pandock` / `Pandock 设置` | tauri.conf.json productName、窗口 title、托盘/关于/日志 target |
| keyring 服务名 | `com.weyham.clouddock` | `com.weyham.pandock` | lib.rs KEYRING_SERVICE、update_runtime.rs KEYRING_SERVICE |
| keyring 键名 | `refresh-token` / `app-secret` / `webdav-password` / `github-token` | 不变 | 仅服务名变化 |
| 数据目录 | `<exe_dir>\data\` | **不变** | data_dir() = current_exe().parent()/data（lib.rs:129-139），exe 改名后逻辑不变，**零迁移** |
| 日志目录 | `%LOCALAPPDATA%\com.weyham.clouddock\logs\` | `%LOCALAPPDATA%\com.weyham.pandock\logs\` | tauri-plugin-log 派生自 identifier；历史日志留在旧目录 |
| journal/readiness | `app/updates/update-journal.json` 等 | 不变 | journal_path() 派生自 app_dir，已核实与 exe 名无关 |
| 环境变量 | `CLOUDDOCK_GITHUB_CLIENT_ID` / `CLOUDDOCK_UPDATE_PUBLIC_KEY` | `PANDOCK_*`，同时保留旧名读取回退 | release 脚本、update_runtime.rs option_env! |
| 日志 target | `clouddock::update` 等 | `pandock::update` 等 | 排查问题时注意新旧日志 target 差异 |
| Release 制品 | `clouddock-v{ver}-windows-x64-portable.zip` | `pandock-v{ver}-windows-x64-portable.zip` | release-windows.ps1、GitHub Release assets |
| Rust crate 名 | `clouddock-app` / `clouddock-core` / `clouddock-nativesync` / `clouddock-updater` | 代码内 crate 名可分期（见 §8） | 不用户可见，不阻断发布 |

## 3. 更新链兼容（重点风险）

### 3.1 现状机制

- 安装白名单 `default_install_allowlist`（core/src/update/verify.rs:91）：`clouddock.exe`、`WebView2Loader.dll`、`VERSION.txt`、`LICENSE.txt` + manifest 中的 helper 路径；
- 更新流程：旧 app 下载 zip → 校验 SHA-256 + minisign 签名清单 → 解包到 staging → 写 `update-journal.json` → 启动 helper（`clouddock-updater.exe`）→ 备份/替换白名单文件 → 按 journal 的 `--launch` 重启；
- helper 名由更新清单 `helper.path` 指定，**与 helper 二进制内部逻辑无关**，可自由改名。

### 3.2 跨名升级方案（核心设计）

**裁决落档（2026-09-28）**：线上仅 v1.0.0 公开发布，1.1.0 尚未发布——过渡支持**直接内建进 1.1.0**，不再设独立过渡版。版本序列：1.1.0 = 最后一批 `clouddock.exe`（内建过渡兼容，不更名）；1.1.1 = 完整更名（`pandock.exe`，见 §7）。

1.1.0 只做两件兼容事：

1. **放宽白名单**：`default_install_allowlist` 同时接受 `clouddock.exe` 与 `pandock.exe`（helper 名由更新清单 helper.path 指定，天然兼容，无需双名）；
2. **动态重启路径**：`--launch` 目标从硬编码 `app_dir.join("clouddock.exe")` 改为「zip 内含 `pandock.exe` 则启动它，否则回退 `clouddock.exe`」。

升级路径由此形成两跳：v1.0.0 → 1.1.0（zip 仍含 `clouddock.exe`，v1.0.0 白名单兼容）→ 1.1.1（zip 含 `pandock.exe`，由 1.1.0 的双名白名单放行、动态 launch 按新名重启）。卡在 v1.0.0 未走 1.1.0 的用户若直接收到 1.1.1 更新，会在校验期被白名单拒绝（安全拒绝，不半成品安装），需先经 1.1.0。

**旧 exe 残留清理**：`clouddock.exe` 不在新白名单（Pandock 版本的白名单只含新名），更新器不会自动删除它。策略：**Pandock 首次运行自检**——若 app_dir 存在 `clouddock.exe` 且自身为 `pandock.exe`，删除之（同时清理旧 `clouddock-updater.exe`）。理由：更新器 allowlist 语义是"替换白名单文件"，不适合承担删除职责；应用首 run 清理简单、可日志化、失败可重试。若用户手动回滚到旧目录版本，`clouddock.exe` 已被删则不满足回滚前提，回滚目录在 `updates/rollback/` 仍有完整备份，helper 回滚逻辑不受首 run 清理影响（清理只发生在正常启动且非回滚流程时）。

**回滚**：journal 的 rollback 流程恢复备份文件。过渡更新后的回滚会恢复 `clouddock.exe` 并需要重启它——helper 的 `--launch` 决策改为**按文件存在性选择**（`pandock.exe` 存在则启动，否则 `clouddock.exe`），回滚删除 `pandock.exe` 后自然回落旧 exe。该决策内建于 1.1.0 的 helper 调用参数（或 helper 逻辑），1.1.1 沿用。

**minisign 签名**：签名密钥对**不换**（换 key 会让所有旧版本无法校验新清单，等于切断更新链）；只改签名脚本/制品命名与内嵌公钥的环境变量名（`PANDOCK_UPDATE_PUBLIC_KEY`，旧名读取回退）。`scripts/sign-update-release.ps1` 同步改名输出清单中的 artifact 路径。

### 3.3 过渡失败兜底

- 若未走 1.1.0 过渡的旧版本（白名单只认 `clouddock.exe`，即 v1.0.0）直接收到 1.1.1 Pandock 更新：校验阶段即拒绝（artifact 不在白名单），**不会半成品安装**——这是安全特性，不是缺陷；这些用户需先经 1.1.0 过渡跳或全量重装；
- 灰度策略（§7）确保主流用户经过 1.1.0 过渡跳。

## 4. 凭据迁移（keyring）

策略：**读回退旧服务名，写只写新服务名**，并在首次成功读到旧服务名凭据时执行一次性复制+删除（渐进迁移）。

- 读取序：`com.weyham.pandock` → 未命中则回退 `com.weyham.clouddock`；
- 迁移时机：应用启动读取到旧服务名中的 `refresh-token`/`app-secret`/`webdav-password`/`github-token` 任一存在且新服务名缺失时，复制到新服务名并删除旧条目；
- 回退读取保留**至少一个大版本**（1.1.x 全周期），1.2.0 起评估移除；
- 理由：复制+删除在首次运行时原子化完成，避免长期双写漂移；保留回退读取防止迁移窗口内崩溃导致凭据不可达。

## 5. 数据目录与同步状态

- `data_dir = current_exe().parent()/data`（Windows），exe 改名**不改变该逻辑**，`data\config.json`、`data\native-sync\sync.db`、同步根（如 `Documents\CloudDock`）全部零迁移；
- 同步根目录名 `CloudDock`（Known Folder 下）是否改名：**不改**——已启用用户的 sync root 路径存于 db，改名会造成 disconnected（根目录不存在策略，ADR-103 P1 已定"不改挂"）。新用户的默认根目录名建议保持 `CloudDock` 或在后续版本中作为独立产品决策（本 ADR 不动）；
- 日志目录随 identifier 变为 `com.weyham.pandock\logs`，历史日志留旧目录，排障时注意。

## 6. identifier 变更影响

| 项 | 影响 | 处置 |
| --- | --- | --- |
| Windows 数据目录 | 无（exe 旁 data\） | 无需处置 |
| macOS/Linux app_data_dir | 路径随 identifier 变化 | 非 Windows 尚未发布正式版，直接切换 |
| 单实例 | 互斥名派生自 identifier | 新旧实例不互斥（并行运行互不知晓）；过渡期内属可接受 |
| 开机启动（Windows Run 键） | 旧注册项可能指向已不存在的 clouddock.exe | Pandock 首 run：若 autostart 开关为开，重新注册（autostart 插件按当前 exe 写入），并尝试删除指向旧 exe 的残留 Run 项 |
| WebView2 / Shell | 无标识依赖 | 无需处置 |

## 7. 发布与灰度

- **切换版本：1.1.1**（裁决落档）；1.1.0 为过渡兼容版（仍 `clouddock.exe`，内建 §3.2 双名白名单 + 动态 launch）；
- 发布序列：
  1. `1.1.0`（CloudDock 名，exe 不改）：NativeSync 等功能 + 过渡兼容；v1.0.0 用户经此跳；
  2. `1.1.1`（Pandock 名，完整更名）：`pandock.exe` + 凭据迁移 + 首 run 旧 exe 清理 + autostart 重注册 + crate 库名同步改（不分期，裁决）+ 日志 target 前缀改 `pandock::`（立即执行，裁决）；
  3. GitHub Release 资产命名从 1.1.1 起用 `pandock-v*`；旧 Release 保留；
- 升级路径：v1.0.0 → 1.1.0 → 1.1.1（两跳，§3.2）；全新用户 ≥1.1.1 直接装 Pandock；
- 灰度：1.1.1 先内部/总监 dogfood，演练完整 v1.0.0→1.1.0→1.1.1 自动升级链，再全量发布。
## 8. 实施步骤（已批准，按版本分工）

1. **1.1.0（过渡兼容）**：allowlist 双名 + 动态 launch（helper 按文件存在性选择重启目标），更新链单测补齐；
2. **1.1.1（完整更名）**：tauri.conf identifier/productName、exe/helper 名、crate 库名（clouddock-app/core/nativesync/updater → pandock-*，同步改不分期）、日志 target 前缀、发布脚本、更新清单、环境变量（`PANDOCK_*`，旧名读取回退）、keyring 服务名 + 迁移逻辑、首 run 旧 exe 清理 + autostart 重注册；
3. 文档：README、release-protocol.md、问题与解决；
4. 测试：更新链单测（双名/动态 launch/回滚路径）、keyring 迁移单测（mock 层）、首 run 清理单测。
## 9. 风险与回滚

- 风险：过渡版未覆盖的极旧版本无法自动升级（§3.3 兜底）；keyring 迁移窗口崩溃（回退读取兜底）；首 run 误删（仅当自身为 pandock.exe 且目标为 clouddock.exe 时触发，日志记录）；
- 回滚：1.1.1 发布后发现阻断问题 → 下架 Release asset + 发布 1.1.2 修复；已升级用户经更新链回滚到 1.1.0（恢复 clouddock.exe，凭据已从旧服务名复制到新服务名，回退读取仍兼容，不丢授权）；
- 更名后**不再**保留 `clouddock.exe` 长期并存，避免双实例/双更新源混乱。

## 10. 裁决落档（2026-09-28，开发总监与用户）

1. 切换版本 = **1.1.1**；1.1.0 内建过渡兼容（线上仅 v1.0.0，两跳升级）；
2. 同步根目录名 `Documents\CloudDock` = **不改**；
3. crate 库名 = **1.1.1 同步改**（不分期）；
4. 日志 target 前缀 = **立即执行**（随 1.1.1 更名一起改 `pandock::`）。
