# 开发与测试

## 环境

- Rust stable
- Node.js 20+
- Windows WebView2 或 macOS WebKit 系统组件
- Windows 代码签名和 macOS 签名、公证材料仅用于正式发布，不进入仓库

## 首次准备

```powershell
npm --prefix ui ci
cargo fetch
```

`Cargo.lock` 和 `ui/package-lock.json` 必须提交，以保证可复现构建。

## 快速检查

```powershell
cargo fmt --all -- --check
cargo clippy -p pandock-core --all-targets -- -D warnings
cargo test -p pandock-core
cargo check -p pandock-app
npm --prefix ui run build
```

完整本地门禁：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\check.ps1
```

如依赖已经安装且不需要重新执行 `npm ci`：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\check.ps1 -SkipUiInstall
```

## 开发运行

```powershell
cargo run -p pandock-app
```

默认不自动显示设置窗口。可从托盘菜单打开，或通过开发环境变量控制启动行为，具体值不得写入仓库。

## 开发部署门禁（`app\` 运行目录）

`app\` 实例的更新只允许走 `scripts/deploy-dev.ps1`（原子、白名单、备份、精确路径停进程）。
在此基础上，每次部署必须**同时**满足：

1. **`SourceDir` 用绝对路径**，并记录源的「身份」：绝对路径 + 大小/mtime + `sha256`；
2. 部署后校验 `app\<exe>` 的 `sha256` **等于被审产物的 sha256**（不是"源=目标"式的自校验）；
3. 校验**只有新构建才满足的特征**，至少覆盖：
   - 运行日志出现新版本才有的日志行（例如 `native-sync notifications effective=`）；
   - `data/native-sync/sync.db` 的 `schema_migrations` 含本次迁移版本号、新表/新列存在；
   - 需要时在二进制里查特征串。

任一不满足即视为**部署失败**：停止、按回滚方案恢复、不要重试。
`data\` 永不被部署触碰；非白名单文件（`pandock-shell.dll` 等）不得在验收窗口内改动。
详见 `docs/问题与解决.md` 的「开发部署用相对路径命中陈旧构建」条目。

## 验证与凭据纪律

- **真实凭据（密码、Token、密钥）不得出现在对话、文档、日志或代码里**，也不向任何协作者索取。
- 需要凭据才能完成的验证，必须二选一：
  1. **由用户在自己终端执行**，只把**不涉密的结果**（哈希、大小、状态码、日志片段）回报；
  2. 改用**不涉密的替代路径**——典型做法是用「同一份内容的哈希前后对照」
     （例如"释放空间"前先记录本地内容哈希，召回后必须完全一致），
     而不是去比对云端凭据保护的接口。
- 验证方案设计阶段就要检查"这一步是否必须拿到凭据"；若是，先改方案再执行。

## 测试层次

1. Rust 单元测试：路径规范、错误分类、配置校验、状态映射和修订标识。
2. HTTP/API 契约测试：使用本地模拟服务和 `wiremock` 验证百度 API 交互。
3. WebDAV 语义测试：通过真实 HTTP 请求验证状态码、Range、ETag、条件请求和认证。
4. UI 构建测试：TypeScript 类型检查和 Vite 生产构建。
5. 人工端到端测试：真实百度账号、WebDAV 客户端和文件管理器。

## 覆盖率门禁

`1.0.0` 正式发布前必须安装并使用 `cargo-llvm-cov`，核心逻辑和关键契约路径覆盖率不得低于 80%。当前本地门禁尚未强制覆盖率，因为构建机未安装该工具；这是正式版前的明确缺口。

## 分支和合并

- 每个功能使用独立 worktree 和特性分支。
- 合并前 rebase 最新 `main`，重新执行完整门禁。
- PR 必须给出测试结果、覆盖率、风险和回滚方案。
- `1.0.0` 前由维护者本地完成检查和发布，不启用工作流。

### 谁能直接改 `main`（2026-10-12 立，长期有效）

- **纯文档**（`docs/**`，且**不含契约/行为描述变更**）可以经**开发总监单笔放行**后**直接提交到 `main`**；
- **任何代码 / 脚本 / 配置**（`nativesync/**`、`src-tauri/**`、`core/**`、`updater/**`、`ui/**`、`scripts/**`、CI 配置）
  **必须**走**三位一体**：worktree + 特性分支 + 全量门禁 + 总监终审 + 放行后 **`--ff-only` 合并**；
- 直接改 `main` 的文档提交，**也必须**满足"**落盘自检**（`git diff --stat` + 锚点 grep）+ **总监可复现的核对方式**"；
- 参考资料提交信息里注明"**纯文档，经总监单笔放行直改 main**"，并写清范围与自检结果——
  避免后人看到"`main` 上有一笔不来自 feature 分支的提交"时困惑。

## 版本管理

版本必须同时出现在：

- 根 `Cargo.toml` 的 workspace 版本；
- `src-tauri/tauri.conf.json`；
- 根 `package.json`；
- `ui/package.json` 与 `ui/package-lock.json`。

使用脚本统一更新：

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\set-version.ps1 -Version 0.1.0
```

## 测试与契约纪律（长期有效）

1. **纯函数 seam 的测试必须覆盖"生产实际传参形态"**，不得只测"看起来真实的值"。
   反例（真实事故）：`placeholder_repair_action` 的签名仍是 `attrs: u32`、而调用点传的是
   `if cf_placeholder { 1 } else { 0 }`，内部 `1 & 0x400 == 0` ⇒ **恒判"要转换"**。
   单测传的全是真实属性值（`0x420` 等），**从未覆盖生产实际传的 `1`/`0`**，因此全绿却漏掉了缺陷。
   **要求**：关键分支至少有一条用例走**调用点实际传的值/类型**；更理想是把"取真值 → 决策"
   用一个薄封装串起来测，而不是分别测两端。
2. **改动签名/调用契约时，交付材料必须附"调用点核验"**：贴出**定义处 + 所有调用点的实际传参**
   （形如 `file:line  definition/call-site  <实际实参>`），供评审逐条对账。
   本条与上一条配合，专治"测试全绿但生产传参错配"这类缺陷。
3. **每次编辑后先验证"改动真的落盘"，再提交**：`git diff --stat` 看行数变化，并 `grep` 新增的符号/锚点。
   理由（真实事故，两次）：补丁因锚点文本被 `rustfmt` 重排而**断言失败、静默未生效**，
   于是出现"以为改了、其实没改"。这与 int/bool 那次"补丁未落盘 ⇒ 测试全绿却行为错"**是同一类根因**。
   这条比"多写测试"更根本：**没有落盘的改动，任何测试都拦不住**。
4. **提交信息用单引号或 `-F <文件>`**（Windows PowerShell 会把 ASCII 双引号当引号解析，
   消息会被拆成 pathspec 导致提交失败或信息被截断）。
5. **观测不得改变被观测对象**：对 Cloud Files 占位符做**内容读取**（`sha256`/`Get-FileHash`/读字节）
   会触发水合，**验证工具可能成为被验证现象的成因**。
   反例（真实事故，2026-10-12）：部署快照对同步根里的三个文件读内容算 sha256，把"释放空间"刚脱水的
   文件立刻读了回来，随后被当成"外部神秘读回者"追查了两轮——身份日志（`placeholder opener … image=python.exe`）
   才证明那是我们自己的取证脚本。
   **判据**：任何"要读内容"的验证步骤，必须先确认目标**不是**已脱水的占位符，或改为只读元数据
   （`FileSize` / `LastWriteTime` / 属性探针 / `CfGetPlaceholderInfo` 走**属性句柄**），
   或对**副本**操作（先拷到 `%TEMP%` 再算哈希）。
6. **停进程 / 改文件一律按"精确路径"过滤；禁止按进程名批量操作。**
   反例（真实事故，2026-10-12）：一个临时夹具探针写了
   `Get-Process -Name pandock | Stop-Process -Force`，把**同机上并存的真实实例**（PID 64040）连同夹具一起杀掉
   —— 实例不可用 **10–16 秒**（已重启恢复，无数据损失）。
   本项目在同机上常有"夹具实例 + 真实实例"并存（演练、回归都会用到），按名操作必然误伤。
   **要求**：`Stop-Process` 前必须 `Where-Object { $_.ExecutablePath/Path -eq <精确路径> }`；
   文件操作同样按精确路径（`-LiteralPath`）；递归清理前**先断言目标在预期目录下**（例如必须位于 `%TEMP%`）。
7. **PowerShell 脚本必须存成 UTF-8（带 BOM）+ CRLF。**
   工具坑（2026-10-12 实测）：无 BOM 时 **Windows PowerShell 5.1** 会把中文注释/字符串按 ANSI(GBK) 解析，
   直接报语法错（`Unexpected token '}'`），而 `pwsh` 7 读同一文件却正常 —— 所以"在 7 上能跑"不代表 CI/脚本宿主（5.1）能跑。
   推论（已用在 `deploy-dev.ps1` 硬化里）：**脚本编码问题要在目标宿主上验证**，别只看现代宿主。
   附一条演练发现的工具约束：`deploy-dev.ps1` 的 pre-flight **不能**把"运行中实例自身的 exe"当成冲突
   （进程映像只允许读共享），否则会永远拒绝部署；exe 由"停后轮询解锁"把关。
## Cloud Files 元数据写操作：句柄与权限（权限验证表的依据）

**官方原文（MS Learn）**——两句合起来说明：**句柄本身可以只带属性访问**，
而 `READ_DATA / WRITE_DATA` 是对**调用者**的 ACL 要求（我们以文件属主身份运行 ⇒ 满足）：

- `CfSetInSyncState`：
  > "A handle to the placeholder. ... **An attribute or no-access handle is sufficient.**
  > The caller must have **WRITE_DATA or WRITE_DAC** access to the placeholder."
- `CfSetPinState`：
  > "... **An attribute or no-access handle is sufficient.**
  > The caller must have **READ_DATA or WRITE_DAC** access to the placeholder.
  > Otherwise, the operation fails with **STATUS_CLOUD_FILE_ACCESS_DENIED**."

**由此确立的规则（不要回退）**：
1. CF **元数据**调用（`CfGetPlaceholderInfo` / `CfSetInSyncState` / `CfSetPinState` / `CfDehydratePlaceholder` /
   `CfConvertToPlaceholder`）**只用属性访问句柄**：只读用 `FILE_READ_ATTRIBUTES`，要改属性用 `FILE_WRITE_ATTRIBUTES`；
   **禁止**改回 `FILE_GENERIC_READ`/`FILE_GENERIC_WRITE`——对**已脱水**的占位符做数据访问会让平台立刻把内容物化回来，
   等于把我们自己的"释放空间"当场撤销（真实事故 D1）。**写入类操作用可写属性句柄**（`pin` 曾误用只读句柄）。
2. **预期**：权限验证表 A–E 阶段里 `CfSetInSyncState` 与 `CfSetPinState` 都应成功（无 `ACCESS_DENIED`），
   且对**已脱水**文件每一步调用之后 `OnDiskDataSize` 仍为 0。
3. **若实测出现 `STATUS_CLOUD_FILE_ACCESS_DENIED`** ⇒ 与上述文档矛盾，**停下来回报**，
   **不要**就地改成 `GENERIC_*`（那会立刻把 D1 带回来），而是按"只调那一个调用点并说明理由"处理。