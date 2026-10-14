# ADR-113：Windows 构建工具链 GNU → MSVC 迁移（去除 WebView2Loader.dll）

- 状态：**已终审**（2026-10-14，§3.9 七项全部按默认值定稿）
- 日期：2026-10-14
- 关联：ADR-104（Pandock 更名）、ADR-106（Velopack 更新通道）、ADR-107（开源迁移）、
  Halcyon 先例（`F:\Projects\Weyham\codex-responses-shim` CHANGELOG 1.0.1，2026-10-08）

## 1. 背景与目标

当前 Windows 发布构建使用 **GNU 工具链**（`x86_64-pc-windows-gnu`，独立 WinLibs 安装，
PATH 顺序遮蔽了 rustup）。它带来三件持续成本：

1. **必须随包携带 `WebView2Loader.dll`**：`webview2-com-sys 0.38.2` 的链接分支是——
   MSVC 静态链 `WebView2LoaderStatic.lib`、GNU 动态链 DLL（该 crate `src/lib.rs` 的
   `cfg_attr`，原文已核）。GNU 下 Tauri/WebView2 必须动态加载 DLL。
2. **本地 app lib 测试不可运行**：`cargo test -p pandock-app --lib` 的测试二进制在
   本机 GNU 下 `0xC0000139 STATUS_ENTRYPOINT_NOT_FOUND`（`docs/validation.md`
   「门禁覆盖面差异」已落档：本机 GNU 工具链固有、与代码无关，未能定位具体缺失符号）。
3. **部署/更新链为这一个 DLL 付出了全部特殊处理**：白名单、allowlist、打包清单、
   打包/部署脚本的专门行（见 §4 清单）。

**目标**：按 Halcyon 方案迁移到 MSVC 工具链——静态链接 WebView2，随包不再携带
`WebView2Loader.dll`，同时恢复本地 app lib 测试可运行。

## 2. 已核实的现状（全部为本机/本仓实测或原文核对）

1. **机制**：`webview2-com-sys 0.38.2`——MSVC → `WebView2LoaderStatic.lib`（静态）、
   GNU → `WebView2Loader.dll`（动态）。出处：该 crate `src/lib.rs` 的 `cfg_attr` 链接分支。
2. **Halcyon 先例**：`F:\Projects\Weyham\codex-responses-shim` CHANGELOG 1.0.1（2026-10-08）：
   「Windows 构建切换 MSVC 工具链：WebView2 静态链接，便携包/安装包/更新包不再携带
   `WebView2Loader.dll`；可复现性由 `/Brepro` 保证」。实现形态：`RUSTFLAGS = '-C link-arg=/Brepro'`
   + `dtolnay/rust-toolchain@<sha> # stable`（GitHub runner 默认 MSVC 主机工具链）。
3. **本机环境（什么都不用装）**：
   - VS Build Tools 2022（含 VC 工具）已装——vswhere 核实（`vswhere -latest -requires
     Microsoft.VisualStudio.Component.VC.Tools.x86.x64` 返回实例）；
   - rustup 默认工具链 = `stable-x86_64-pc-windows-msvc`，且 `x86_64-pc-windows-msvc`
     target 已安装（`rustup show active-toolchain` / `rustup target list --installed` 实测）；
   - 当前生效的是 `C:\Program Files\Rust stable GNU 1.97\bin` 独立 GNU（PATH 遮蔽 rustup）。
4. **门禁预验证（本 ADR 的关键证据）**：
   - 用 rustup MSVC cargo 在本 worktree 直接跑 `cargo test -p pandock-app --lib`：
     **8 passed; 0 failed**——**本机历史问题（0xC0000139）在 MSVC 下直接消失**；
   - 同一 worktree 用 GNU 复跑：`exit code: 0xC0000139, STATUS_ENTRYPOINT_NOT_FOUND`——
     与 `docs/validation.md` 的历史记录一致（对照组成立）。

## 3. 九个问题的逐项决策

### 3.1 切换机制（问题 1）

**决策**：**脚本显式 `rustup run stable-x86_64-pc-windows-msvc`**，不用 `rust-toolchain.toml`。

对比与理由：
- `rust-toolchain.toml`：能让**所有** cargo 调用（含 IDE、`cargo check` 习惯性手动跑）
  都走 MSVC——但风险是**全局改变开发体验**（开发者日常也想用 GNU 的就此消失，
  且它是「改仓库行为」的隐式开关，提交即生效，review 容易漏）。
- 脚本显式 `rustup run`：发布/CI 路径**显式、可审计、作用域最小**——只有
  `build-velopack.ps1` / release 流水线被钉到 MSVC；开发者日常 `cargo check/test` 不受影响
  （GNU 继续可用，包括**继续本地复现** validation.md 里那个 0xC0000139 的对照环境）。
- **推荐：脚本显式 `rustup run stable-x86_64-pc-windows-msvc`**（Halcyon 用 CI 默认
  主机工具链即隐式 MSVC；本机有独立 GNU 遮蔽，必须显式指定才能不被遮蔽——
  这是本机与 Halcyon 环境唯一实质差异）。
- **既有独立 GNU 安装：保留不碰**（它是 validation.md 对照环境与日常开发链路；
  卸载是不可逆且无收益的操作）。

### 3.2 CI 改造（问题 2）

- **`release.yml`**：删除「Rust GNU toolchain（与本地发布构建一致：windres/
  WebView2Loader/时间戳）」步骤（安装 GNU + `rustup default gnu` + PATH 加 MinGW）。
  windows-latest 默认主机工具链即 MSVC——**照抄 Halcyon `dtolnay/rust-toolchain@<sha>
  # stable` 的形态**（不指定 target 即主机默认 = MSVC；出处：Halcyon release.yml L48-50）。
  同时删除「`RUSTFLAGS=-Wl,--no-insert-timestamp`」与 WebView2Loader 相关注释/断言
  （见 §4 清单），`RUSTFLAGS` 改为 `-C link-arg=/Brepro`（同 Halcyon）。
- **`build.yml`**：`windows` job 不装 GNU、不显式设 target（默认 MSVC），显式
  `rustup component add rustfmt clippy` 不变。`macos-check` 不动。
- **`coverage` job（llvm-cov）**：**评估结论——MSVC 下 llvm-cov 照常用**。
  依据：llvm-cov 的 `llvm-cov`/`llvm-profdata` 是 **LLVM 官方工具（与目标三元组无关）**，
  对 Rust 的 `-C instrument-coverage` 产生的是 LLVM 覆盖率格式（与 PE 链接器无关，
  MSVC `link.exe` 支持该插桩）。本仓现有覆盖率脚本（`scripts/check-coverage.ps1` +
  WSL `cargo llvm-cov`）的护栏按 crate 名过滤，与目标三元组无关——**MSVC 下预期可直接跑**。
  唯一遗留点：本机 WSL 通道（开发自用的 llvm-cov 通道）目前是 **GNU 产物**——
  迁移后按 CI 主通道（MSVC）重测一遍并落档；不把 WSL 通道写进门禁（它本来就不是门禁）。

### 3.3 门禁变化（问题 3）

**已实证**：`cargo test -p pandock-app --lib` 在 MSVC 下本机通过（8 passed，见 §2.4）。
⇒ `docs/validation.md`「门禁覆盖面差异」一节**追加式修订**：保留原 GNU 失败记录
（历史事实），追加「2026-10-14 实测：MSVC 工具链下本机同样通过（8 passed）⇒
该步骤在 MSVC 迁移后可回归本地 `check.ps1`」。**建议把 `cargo test -p pandock-app --lib`
加回 `scripts/check.ps1`**（本地与 CI 同构门禁的最后一块缺口——这正是迁移的红利之一）。
若加回后出现任何失败，如实记录并退回（不编造）。

### 3.4 更新链兼容（问题 4，1.4.0 带 DLL → 1.4.1 无 DLL）

**Halcyon 的处理（照抄）**：`apply_staging_files` **只替换白名单内的文件、不删除
目录内其他文件**——staging 里没有 DLL 时**什么都不做**（旧 `WebView2Loader.dll`
留在目录里）。出处：`F:\Projects\Weyham\codex-responses-shim\core\src\update\install.rs`
（`apply_staging_files`：`if !source.is_file() { continue; }`，无任何 delete-else 逻辑）。

⇒ **本仓同样照抄**：1.4.1 staging 不含 DLL ⇒ 旧目录里的 `WebView2Loader.dll` **残留**
（文件死重、无加载副作用——MSVC 静态链后没有任何代码会再加载它）。
**评估**：残留无害但属"看着别扭"的遗留；按 ADR-107 的"部署不动用户数据/不留意外
文件"口径，建议**只记录、不主动清**（主动 delete 是我们自己的副作用，且一旦误删
同名用户文件就是事故；Halcyon 也是这么选的）。如用户/总监后续想要清理，单独排一个
"一次性无害残留清扫"小项（限定绝对路径 + 哈希核对 + 白名单外绝不碰）。

### 3.5 发布物一致性（问题 5：windres/图标/时间戳）

- **windres/图标**：`tauri-build`（`tauri.conf.json` 的 icon.ico）在 MSVC 下用
  `rc.exe`（VS Build Tools 已装）——与 GNU 的 `windres` 输出资源段**语义等价但字节
  不完全相同**（这正是 release.yml 注释强调"与本地一致"的原因）。
  **核对方案**：发布流水线构建后跑既有 `scripts/verify-windows-resource.ps1`
  （已存在、MSVC/GNU 通用）+ Setup.exe 属性页/图标目检（人工验收项）。
- **时间戳/可复现性**：GNU 用 `-Wl,--no-insert-timestamp`；MSVC 用 **`/Brepro`**
  （Halcyon 同款，确定性 PE 输出）。**直接照抄 Halcyon**：`RUSTFLAGS = '-C link-arg=/Brepro'`。
  核对：同一提交连续两次 release 构建的 `pandock.exe` sha256 相等（CI 内可留一行
  `Get-FileHash` 对照，或纳入 build-velopack 自检）。

### 3.6 测试矩阵（问题 6，逐项验收判据）

| # | 项 | 判据（客观） |
|---|---|---|
| 1 | 全量门禁（MSVC） | `scripts/check.ps1` 全绿；`cargo test -p pandock-app --lib` 8 passed 本地通过 |
| 2 | build-velopack | 成功；stage 目录**无** `WebView2Loader.dll`；`releases.win.json` 资产清单无 DLL |
| 3 | deploy-dev | 成功；`app\` 部署后 `WebView2Loader.dll` 按 §3.4 残留（或按裁决清扫） |
| 4 | 设置页 WebView2 渲染 | 干净目录（无 DLL）启动 → 设置页正常渲染、NativeSync 区块可见（截图/人工） |
| 5 | updater 行为 | 1.4.0 → 1.4.1 delta 更新成功；旧目录 DLL 残留但不影响启动；updater 自举正常 |
| 6 | app lib 测试 | 本地 + CI 均 8 passed |
| 7 | 便携包 | 干净目录解压（无 DLL）→ 首启正常 → WebDAV/NativeSync 正常 |

### 3.7 /Brepro（问题 7）

**判断**：**引入，成本一行，收益直接**——`RUSTFLAGS = '-C link-arg=/Brepro'`。
我们当前不做严格 bit-for-bit 要求，但 stage 与部署哈希对比本来就存在
（`deploy-dev.ps1` 已有 `Get-FileHash` 对比），`/Brepro` 让它从"通常相等"变成
"确定相等"，且与 Halcyon 完全一致——**不需要额外的流程负担，直接引入**。
（不做完整可复现构建声明，只享受确定性输出。）

### 3.8 回滚（问题 8）

**回滚路径（最小动作）**：还原 `release.yml` 的 GNU toolchain 步骤 + 还原
`build-velopack.ps1` / `deploy-dev.ps1` / `core/src/update/{verify,velopack_feed}.rs`
的 DLL 条目（即 git revert 本 ADR 的实现提交），重新打包即回到「GNU + DLL」
发布形态。**回滚不需要还原工具链安装**（GNU 保留不碰，随时可切回）。

### 3.9 决策点清单（待总监裁决，默认值已给出）

| # | 决策点 | 默认 | 说明 |
|---|---|---|---|
| 1 | 切换机制 | **脚本显式 `rustup run stable-x86_64-pc-windows-msvc`** | 不用 rust-toolchain.toml（全局隐式行为） |
| 2 | 独立 GNU 安装 | **保留不碰** | validation 对照环境 + 日常链路 |
| 3 | app lib 测试 | **加回 `scripts/check.ps1`** | MSVC 下已实证 8 passed |
| 4 | 旧 DLL 残留 | **只记录、不主动清**（照抄 Halcyon） | 另排一次性清扫小项（可选） |
| 5 | `RUSTFLAGS` | **`-C link-arg=/Brepro`**（替换 `--no-insert-timestamp`） | Halcyon 同款 |
| 6 | CI rust 步骤 | **照抄 Halcyon `dtolnay/rust-toolchain@sha # stable`**（不显式 target） | 主机默认 = MSVC |
| 7 | llvm-cov | **MSVC 下照常使用**（LLVM 覆盖率与链接器无关） | WSL 通道迁移后重测落档 |

## 4. 迁移要逐项处理的 DLL 特殊点清单（实施清单）

| 位置 | 现状 | 迁移动作 |
|---|---|---|
| `scripts/build-velopack.ps1` L46-48 | 断言 `$builtDll` 存在 | 删除 DLL 断言与拷贝 |
| `scripts/build-velopack.ps1` L100 便携包注释 | "请保持 pandock.exe、WebView2Loader.dll 和 data\ 在一起" | 改为 "pandock.exe 与 data\" |
| `scripts/deploy-dev.ps1` L66 白名单 | 含 `WebView2Loader.dll` | 移除该项（残留 DLL 不在白名单 = 不被部署覆盖、自然残留） |
| `scripts/deploy-dev.ps1` L82 注释 | "实例把它映射进自己的进程" 的占用说明 | 追加式注释（DLL 不再随包，但历史残留仍可能被旧实例映射） |
| `core/src/update/verify.rs` L40 allowlist | 含 `WebView2Loader.dll` | 移除；更新 `allowlist_accepts_app_files_and_helper` 测试（L68） |
| `core/src/update/velopack_feed.rs` L28 APP_FILES | `["lib/app/pandock.exe", "lib/app/WebView2Loader.dll", "lib/app/updater.exe"]` | 移除 DLL 条目（3→2）；`extract_app_files_*` 测试同步（L205 断言 `len()==3` → 2） |
| `scripts/test-updater-windows.ps1` L54/L63 | 模拟写 old/new DLL | 删除模拟 DLL 的两行 |
| `docs/release.md` L54 | "打包目录只包含 pandock.exe、WebView2Loader.dll、updater.exe" | 改为 "只包含 pandock.exe、updater.exe" |
| `docs/validation.md`「门禁覆盖面差异」 | GNU 失败史 + "不要加进 check.ps1" | 追加式修订（见 §3.3），保留历史不篡改 |
| 各 ADR 历史引用 | 只追加 | 不篡改（追加式纪律） |

## 5. 风险与回滚

- **风险 1**：windres/资源段字节差异导致发布物哈希与历史记录不同 ⇒ 这是预期差异
  （工具链变更本身），不是缺陷；用 verify-windows-resource.ps1 + 目检把关，不以
  历史哈希做等值断言。
- **风险 2**：CI `dtolnay/rust-toolchain` 不指定 target 时，若 GitHub 把 windows-latest
  默认主机工具链改为非 MSVC（极低概率）⇒ 发布物意外变回 GNU/带 DLL。缓解：
  在 release.yml 构建后**显式断言** `pandock.exe` 不依赖 `WebView2Loader.dll`
  （用 `dumpbin /dependents` 或 PowerShell 读 PE 导入表）——一行断言，CI 直接失败显形。
- **回滚**：git revert 本 ADR 实现 + 恢复 GNU CI 步骤即可（见 §3.8），
  工具链两边都保留在机器上，随时可切。
