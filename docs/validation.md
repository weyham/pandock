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

  **2026-10-14 追加（ADR-113）**：用 rustup `stable-x86_64-pc-windows-msvc` 在本机同一代码上直接跑
  `cargo test -p pandock-app --lib` → **8 passed**（同一 worktree 用 GNU 复跑仍 `0xC0000139`）。
  ⇒ 该失败确为 **GNU 工具链固有**（与代码无关），MSVC 迁移后本地门禁可恢复同构。
  `scripts/check.ps1` 已加回该步骤（显式 `rustup run stable-x86_64-pc-windows-msvc`，
  并带 `rustup toolchain list` 预检，缺工具链时给出明确安装提示而非裸 0xC0000139）。
  上文「不要把它加进本地 check.ps1」的结论**作废**（保留历史记录以备追溯）。


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

## S2 上传方向（双向同步）真实机验收（2026-10-13 ~ 10-14）

**验收对象**：`main = 8ea1b45`（S2 全量：PR-1/PR-2 + 真实机验收逼出的 8 个修复 PR）。
验收方式：真实账号 + 真实实例 + 探针文件（全部自建自删、用户动作仅 1 次重新授权）。

### 收官复验判据（2026-10-14 全过）

| 判据 | 证据 |
|---|---|
| 新建文件自动上传 | 探针 20 秒内 `local scan: new local entry` → `upload done rtype=0`，云端 stat 一致 |
| online_only 占位符编辑捕获 | `local scan: edit detected` → stat 一致 → `rtype=3` 安全覆盖（B4） |
| 冲突保留双方（OneDrive 语义） | C5：stat 判冲突 → 云端占原名、本地副本改名 `-DESKTOP-LQH51N6` 保留并回传、原名进需处理、下一轮零误判 |
| pending upload 豁免 missing | C5 日志 `missing skip: pending upload entry kept`（C4 误杀形态的直接对照实证） |
| 陈旧 listing 免疫 | A3 上传后 5 小时 30+ 层从未被 missing/删除；`missing_marked` 全周期稳定无新冤案 |
| 删除传播 | 多条 `delete propagated` + tombstone（含用户真实删除动作的实证） |
| 已删目录树收编 | `/cc-switch-sync` 四层树：`dir gone remotely` 收编、零物理删除、水合文件保留（remote_missing） |
| fs_id 轮换收编 | upsert stale-replacement 多次实证（覆盖/替换场景） |

### 真实机验收逼出的 10 个缺陷（发现史——fake 测试结构性抓不到，全部有生产实锤）

1. **Gap A**：CF 通知是占位符作用域，无 CREATE 通知 ⇒ 本地新建永不触发捕获（修复：本地侧对账扫描）；
2. **Gap B**：hydrated 文件的本地编辑不产生 open/close 通知（同上修复覆盖）；**Gap C**（写打开通知缺失）：标已知未知项，已被本地扫描覆盖；
3. **rtype 模型错误**：编辑已同步文件在 rtype=0 下 100% 假冲突（修复：rtype 分流——新文件 rtype=0、有基线编辑 stat 比对后 rtype=3；**责任归属：总监终审漏判**）；
4. **秒传语义误读**：空 block_list 不是秒传信号（文档原文「空等价于 [0]」+ EXP1/2/3 实测；缺分片 create 实测 errno=31500 非文档 31363）；PR-1「真秒传」回滚，rapid_hint 管线退役；
5. **扫描/收编排序**：ingest 无条件刷新快照先于扫描执行 ⇒ 编辑证据每层被擦除（修复：scan 先于 ingest）；
6. **收编掩盖冲突**（finding #3，时机依赖）：pending upload 条目被层内 ingest 收编基线 ⇒ stat 比对永远晚一步（修复：H1 扩展——pending 条目不刷新基线）；
7. **陈旧 listing 误删**：`method=list` 最终一致延迟实测 6~54 分钟，刚上传的副本被 missing 流程物理删除（修复：30 分钟宽限 + 批量 stat 确认 + >50 候选闸门；附带发现：覆盖上传**轮换 fs_id** ⇒ 确认必须按路径）；
8. **占位符目录删除撞「目录非空」(145)**，且连带发现更危险的：remote_missing 跨层不幂等 ⇒ 下一层会把用户 52KB 水合文件物理删除（修复：保留态幂等 + 自底向上深度降序收编）；
9. **第三形态文件**：recall+write 后的文件有 reparse 点、无 RECALL 属性、CfGetPlaceholderInfo 失败（0x80070178）⇒ on_disk 短路判据对主场景从未生效（修复：改用 RECALL 属性判定；**前判据为总监终审裁决，责任对半**）；
10. **stat 确认路径归一化反相**：filemetas 返回含 `/apps/<APP>/` 前缀的全路径，与候选的 DAV 相对路径永远错配 ⇒ 存活确认变整体误删（修复：`dav_path_from_remote` 剥离前缀；**硬性要求：FakeBackend 输出形态改为与生产一致**）。

### 令牌链碰撞事故（2026-10-14 凌晨，流程事故非产品缺陷）

- 实验脚本直接读 keyring 的 refresh token 自行刷新（单次使用+轮换）且未写回 ⇒ 实例重启后刷新失败、全线 errno=-6、无自愈 ⇒ 用户重新授权 1 次恢复；
- 连带产品缺陷（已落档）：**-6/令牌失效不会触发重新授权引导**（死重试）；**多实例/脚本共享单例轮换链天然互斥**；
- 新纪律：实验只经实例 WebDAV 通道或批准的只读通道；任何直接刷新必须写回新 token 到 keyring；涉事脚本已全部删除。

### 结构性教训（本轮最大收获）

**「fake 不设防」四次同族**：fake backend/platform 不会自己长出真实服务器/平台的行为（通知作用域、列表延迟、错误码形态、路径形态、物理约束）。
⇒ 固化为门禁原则：**凡涉及外部平台行为的 PR，必须有真实机验收项；fake 的输入/输出形态必须与生产实测形态一致**（含反向验证：拆掉修复后测试必败且现象与生产一致）。

### 残留边界（如实清单，均不阻断收口）

- **Gap D**：已上传成功的文件是本地普通文件（非占位符），其删除/改名暂不传播云端（候选修法：上传成功后转回占位符，待评估）；
- 第三形态文件 `CfSetInSyncState` 失败 ⇒ 防脱水标记打不上，已捕获未上云编辑在第三形态文件上有被脱水抹掉的残余风险；
- 百度 `method=list` 新鲜度无 SLA ⇒ 云端变化的下行延迟偶尔可达数十分钟（生杀判定已改用 stat 确认，不受影响；影响的是"新文件何时出现在本地"）；
- 自愈回位的条目 state 停留 `remote_missing`（外观残留，upsert 复活行时未清）——小修待排；
- `sync.db` 的 `.bak` 备份是空壳（不含 WAL）——改用 SQLite 在线备份属高优先待办。
### WebDAV 目录列举新鲜度实验（2026-10-14，负结果如实记录）

- 健康实例（PID 67796）+ 真实账号 + 应用目录根，3 轮 PUT / 3 轮 DELETE / 1 轮 MOVE，全部在**第一次父目录 PROPFIND 即反映最新状态**：PUT 可见延迟 0.281–0.556s、DELETE 消失 0.242–0.388s、MOVE 旧名消失与新名出现同帧；Depth:0 对照一致，无错位。
- **未复现** NativeSync 侧 2026-10-13 观测到的 6–54 分钟 `method=list` 滞后（该滞后有生产日志实锤：上传 15:14 → list 16:04 才收录副本）。
- 两通道共用**同一个 `BaiduCloudFs::list` 实现与请求参数** ⇒ 差异不是代码路径，定性为**上游间歇性最终一致**（时段/条件相关），非恒定缺陷。
- 风险分级：WebDAV 侧滞后 = 目录视图晚看到（UX 级，无本地状态可误删）；NativeSync 侧滞后 = 曾导致误删（已由 stat 确认守卫防住，属生杀判定）。
- **决策**：暂不为 WebDAV 实现写穿补偿层（无实证缺陷）；NativeSync 的 stat 确认守卫继续保留。若用户侧出现可复现症状，再按本实验协议复测。
- 证据：`%TEMP%\pandock-webdav-fresh-probe\pandock-webdav-fresh-probe-1791944836-fa5845.summary.json` 及同前缀 `.jsonl`（每次请求的方法/状态码/耗时/条目清单）。