# 验证矩阵

## 自动化基线

| 类别 | 覆盖内容 | 本地门禁 |
| --- | --- | --- |
| Rust 格式 | workspace 全量格式 | `cargo fmt --all -- --check` |
| Rust 静态检查 | 核心库全部 target | `cargo clippy -p pandock-core --all-targets -- -D warnings` |
| 核心单元测试 | 配置、路径、错误、资源版本和状态 | `cargo test -p pandock-core` |
| 百度 API 契约 | 授权、刷新、列表、元数据、上传、下载、移动、复制、删除 | 模拟 HTTP 服务 |
| WebDAV 契约 | 认证、PROPFIND、GET、HEAD、PUT、MKCOL、Range、条件请求和关闭 | 本地真实 HTTP 服务 |
| 桌面层 | Tauri 应用编译和配置一致性 | `cargo check -p pandock-app` |
| UI | TypeScript 与 Vite 生产构建 | `npm --prefix ui run build` |
| 保密 | 常见凭据模式与禁用文件 | `scripts/check-secrets.ps1` |

## 覆盖率门禁

`1.0.0` 前使用 `cargo-llvm-cov` 统计核心逻辑和关键契约路径，门禁阈值为 80%。当前基线未安装覆盖率工具，因此该项仍为未完成状态。

## 人工发布验证

正式发布前至少完成：

1. Windows portable ZIP 在干净目录解压并启动。
2. 首次配置、设备码授权、重启恢复和重新授权。
3. Windows 资源管理器挂载、浏览、上传、覆盖、下载、移动和删除。
4. 大文件分片上传和 Range 下载。
5. 中文、空格和长路径文件名。
6. 网络中断、端口冲突和 Token 刷新失败路径。
7. macOS 对应平台构建和 Finder 基本挂载（如发布 macOS）。
8. 验证结束后清理所有测试云端文件。

## 客户端矩阵

发布前按变更风险选择验证：

- Windows 资源管理器；
- macOS Finder；
- rclone；
- Cyberduck；
- 其他支持 WebDAV 的客户端。

至少记录客户端版本、操作、预期、实际结果、清理结果和脱敏日志。真实账号测试不得操作用户正式同步目录。

## 尚未自动化

以下项目当前不是持续自动化门禁：

- 真实百度账号端到端测试；
- 多客户端并发冲突；
- 长时间运行和内存稳定性；
- 100 MiB 以上文件；
- 百度上游故障注入；
- macOS 签名和公证。

这些项目必须在 `1.0.0` 前补充到本地发布清单或后续 CI。

## NativeSync 无用户参与的客观验收（2026-10-12）

用户不在场时，用**客观等价实验**取代"人工双击"完成验收：**双击 = shell 走文件关联打开它 = 对该文件发起一次数据打开**，
因此可由外部进程做一次**内容读取**等价替代，且能控测量、不弹编辑器窗口。

**探测纪律**：读内容前**先记录状态探针**（`CfGetPlaceholderInfo` 走**属性句柄**：`pin` / `inSync` / `FileSize` / `OnDisk` / `attrs`，
外加 `fsutil reparsepoint query` 的 tag）；需要内容哈希时**先判定 `OnDisk == FileSize`（已完全水合）**，
再对 `%TEMP%` **副本**计算 —— 绝不直接读已脱水占位符的内容（读它会立刻把它水合回来）。

### 三次实测（同一台真机）
| 实验 | 条件 | 结果 |
|---|---|---|
| 提供程序不在时读取 `db.sql` | 应用已停（精确路径停止，0.5 s 后文件解锁） | **快速失败 23 ms**；`ERROR_CLOUD_FILE_PROVIDER_TERMINATED`（"云文件提供程序意外退出"）；**无 `fetch` 日志**；状态零变化（`OnDisk` 仍 0、tag 仍 `0x9000401a`、`pending=0`、`running=0`） |
| 提供程序在运行时读取 `db.sql` | 应用已重启（`autostart reconnect: connected total=7`） | **成功按需水合 1071 ms**，`bytes=52699`；日志 `fetch start`/`fetch done … full_file=true`；`OnDisk == FileSize = 52699`、tag 回到 `0x9000601a`；**受控哈希 MATCH**（`838e0e19…5aeb2`）；读取者身份被记录（`placeholder opener … image=powershell.exe`） |
| 提供程序在回调里自行完成脱水（乙-①，已废弃） | 外部发起脱水 | **能完成，但耗时 53,597 ms** —— 判为不可接受，已回退并改走方案甲（见 `问题与解决.md`） |

### 两条必须写下来的口径
1. **`LastAccessTime` 只在成功的数据访问后更新**：⑧ 那次**失败**读取**没有**改动它（仍为旧值），
   ② 那次成功读取把它更新到访问时刻。⇒ 该锚点可用于"成功打开/下载"的客观时间戳，
   **不能**给"失败的点击"打时间戳。
2. **错误码要写清来源**：本次探针拿到的是 .NET 包装码 `0x80131501`，机制错误只体现为消息文本
   （"云文件提供程序意外退出"）——若要机制码本身（`ERROR_CLOUD_FILE_PROVIDER_TERMINATED`，Win32 404 / `0x80070194`），
   需改用 Win32 原语取值，**不要**把包装码当成机制码写进材料。

### 终审补录：验收结论与合并对象是同一份代码（2026-10-12）

实机验收（⑧/②/⑥/⑦/C）之后，我方**再无任何影响产物的改动**，因此本轮验收结论直接适用于要合并的提交：

```
$ git log --oneline df67a22..HEAD
0be5da8 docs: 记录 D2 机理更正、观测纪律第 5 条、部分在本地进路线图
2e58732 docs: 外部发起的释放收手定稿（三形态实测表 + 更正甲的说法）
76b4161 docs: 客观验收方法论与三次实测数字入档（validation.md 新节）
e86c495 chore(deploy): deploy-dev.ps1 硬化（pre-flight / 轮询解锁 / 失败先恢复服务）
36a2ddd docs: 追加两条长期纪律（精确路径停进程/改文件、PS 脚本 BOM+CRLF）

$ git diff --name-only df67a22..HEAD
docs/development.md
docs/roadmap.md
docs/validation.md
docs/问题与解决.md
scripts/deploy-dev.ps1
```

⇒ **`df67a22`（甲方案：不注册脱水回调 + NOT_IN_SYNC 守卫）之后只改了文档与部署脚本**，
应用产物代码与验收时跑的那份**逐字节相同**。

**覆盖率的证据来源**：`nativesync` 总行 90.11% 由**模块提供**（`cargo llvm-cov`／WSL Ubuntu-24.04），
**非门禁项、未经总监独立复跑**，记录时按"模块提供"标注。

3. **解读本地变更事件时的证据边界**：`source`（`explorer`/`app`/`system`）是**我们代码的启发式标签**
   （由"是否为阻塞型回调"等规则推导），`reason`（如 `user_manual`）是**平台语义**（人工发起），
   **两者都不能作为"是谁发起"的证据**；能证明发起者身份的只有 open/close 通知里的 `ProcessInfo`（`image`/`pid`）。
   （教训：2026-10-12 我们先后据此怀疑"外部程序读回"与"用户点击"，两次都错——先错在没看 `ProcessInfo`，后错在把启发式标签当事实。）

**边界**：本节证明的是"提供程序不在时被拒绝且不挂起"与"提供程序在时会按需下载、内容逐字节一致"；
"用户双击后能否被 shell 关联/编辑器打开"属于**体验层**，需用户在场时补充确认。

### 门禁覆盖面差异（2026-10-12 实测）

- 本地 `scripts/check.ps1` **不含** `cargo test -p pandock-app --lib`；该步骤由 **CI**（`windows-latest`／**MSVC**）执行并通过。
- 本机为 **`x86_64-pc-windows-gnu`**（WinLibs UCRT，rustc 1.97.1 / LLVM 22.1.6），该测试二进制在本机**加载失败**：
  `0xC0000139 STATUS_ENTRYPOINT_NOT_FOUND`（缺 `WebView2Loader.dll` 时先报 `0xC0000135`）。
- **在合并前的 `8718b54` 上、以全新独立 `CARGO_TARGET_DIR` 同样复现** ⇒ **本机 GNU 工具链固有、与代码无关、不影响发布产物**。
- ⇒ 该步骤**只由 CI 把关**；**不要**把它加进本地 `check.ps1`，否则本地门禁会常量失败。
- 机制层已排除：缺 `WebView2Loader.dll`（那是另一个错误码）、PATH 污染（干净 PATH 仍复现）、
  `combase.dll`/`rpcrt4.dll` 导入缺失（导入符号均在其导出表中）、`WebView2Loader.dll` 符号不匹配（导入↔导出完全对应）。
  **未能定位到具体缺失的导出符号**——继续深入需要加载器跟踪（通常需管理员）或 MSVC 对照（本机没有），
  代价与收益不成比例，故**停止追查**（不编造机理）。


## S1.5 无感同步（ADR-111）真实机验收（2026-10-13）

**验收对象**：`main = a642d20`（PR-1 引擎/调度 `8a8dbc0` + PR-2 UI 状态面 `e894568`/`a642d20`）。
部署：`build-velopack.ps1`（1.3.0）→ `deploy-dev.ps1`；`app\pandock.exe` sha256 与构建产物逐字节一致；
实例 PID 5976，启动 4 行齐全，`native-sync auto-sync scheduler spawned` 在日志中确认。

### 自动对账实测（实例日志 + sync.db 只读快照 + 磁盘元数据探针）

| 判据（ADR-111 定稿） | 实测 | 结果 |
|---|---|---|
| 首层 = 启动后 60s | 启动 09:21:30 UTC → 首层 09:22:31（61 秒） | ✓ |
| 层间隔 10 分钟 | run=2 09:32:33 / run=3 09:42:35（10m±3s） | ✓ |
| 每层调用 ≤25 | 三层均 calls=5 | ✓ |
| 顶层新增 1 层内发现 | 探针文件写入后 5m52s 自动落占位符（run=2 `added=1`） | ✓ |
| 顶层删除 1 层内收敛 | 删除后 ~9m50s 占位符自动移除（run=3 `removed=1`） | ✓ |
| 干净树零写入 | run=1（探针前）added/changed/removed=0；探针前后 db 快照仅探针行变化 | ✓ |
| 验收窗口告警 | 09:26–09:43 UTC 0 条 WARN/ERROR | ✓ |

**探针方式**：`pandock-autosync-probe-20261013-172641.txt`（124 B，/apps 应用目录根下），
经实例自身 WebDAV 通道写入/删除（与 API 同后端同语义；避免 token 进入验证进程）。
db 快照：写入后新增行（fs_id=75416668168886，online_only，md5 有值）；删除后该行 tombstone=1，其余条目零变化。
磁盘佐证：占位符属性含 RECALL_ON_DATA_ACCESS+OFFLINE；删除后文件不存在。

**遗留一项（ADR-112 §3.4 回收站语义）**：`opera=delete` 是否进百度回收站，开放平台文档未写明，
本轮无合规观察通道（不碰用户凭据）⇒ 待用户在网页/App 回收站目测一次后回写 ADR-112。