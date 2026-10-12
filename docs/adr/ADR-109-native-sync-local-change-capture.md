# ADR-109：NativeSync 本地变更捕获（S1，不上传）

- 状态：已实施（S1）
- 日期：2026-10-12
- 关联：ADR-103（Cloud Files API 接入）、ADR-108（同步根外壳注册）
- 范围：`nativesync` crate + `src-tauri` 事件面；**不改 `ui/`、不改版本号**

## 背景

ADR-108 确立了外壳注册，并明确记录了一条代价：

> 当前提供程序只注册 `FETCH_DATA`、`CANCEL_FETCH_DATA`、`FETCH_PLACEHOLDERS` 三个回调；
> `NOTIFY_DELETE`、`NOTIFY_RENAME`、`NOTIFY_DEHYDRATE`、`NOTIFY_FILE_OPEN_COMPLETION`、
> `NOTIFY_FILE_CLOSE_COMPLETION` 均未注册。因此资源管理器里的删除、改名、固定、释放只在
> 本地生效，云端不受影响；**应用仅在下一次 `sync_now` 对账时回读事实**。

这在产品上是不可接受的：OneDrive 式体验的关键不是「能不能同步」，而是**本地发生了什么
必须立刻可见**。用户删掉一个文件后，应用必须马上知道「这个条目本地已删除、云端仍在」，
否则既无法呈现状态，也无法为后续双向同步（1.3.0 候选）打底。

## 对本 ADR 与 ADR-108 的关系（追加式）

本 ADR **不改写 ADR-108 原文**，只修订上面那一条表述：

- 原表述：应用**仅在下一次 `sync_now` 对账时**回读事实。
- 修订为：应用**实时捕获**本地变更并落库为「本地已变更、尚未同步到云端」，
  但**仍然不向云端传播**（与 ADR-103 第一阶段「remote → local 单向」一致）。

`sync_now` 的对账职责本身不变；新增的是「事实到达得更早」以及随之而来的守卫。

## 决策

### 1. 回调注册：3 个只读召回 → 13 个回调 + NONE 终止项

只读召回三项保持不变（`FETCH_DATA`、`CANCEL_FETCH_DATA`、`FETCH_PLACEHOLDERS`），
新增十项：`CANCEL_FETCH_PLACEHOLDERS`、`VALIDATE_DATA`、
`NOTIFY_FILE_OPEN_COMPLETION`、`NOTIFY_FILE_CLOSE_COMPLETION`、
`NOTIFY_DEHYDRATE`、`NOTIFY_DEHYDRATE_COMPLETION`、`NOTIFY_DELETE`、
`NOTIFY_DELETE_COMPLETION`、`NOTIFY_RENAME`、`NOTIFY_RENAME_COMPLETION`。

（早期沟通稿写作「13 项（12 回调 + NONE）」；按 CFAPI 的实际回调类型表，
准确表述是 **13 个回调 + `CF_CALLBACK_TYPE_NONE` 终止项 = 14 个数组槽位**。）

### 2. CFAPI 回调纪律（违反会导致 Explorer 挂起或误删本地文件）

- **阻塞型回调**（`NOTIFY_DEHYDRATE` / `NOTIFY_DELETE` / `NOTIFY_RENAME`）必须**同步内联
  ACK**：平台收到成功 ACK 才真正落地删除/改名/脱水。因此这三个回调里 ACK 之前
  **不得**做任何 I/O、加锁等待或日志落盘。
- **非阻塞通知**一律快速投递到 worker，回调线程不查库、不联网、不落盘。
- `VALIDATE_DATA`：安全 ACK + warn（只观测；S1 不改动用户数据、不触发重新下载）。
- `CANCEL_FETCH_PLACEHOLDERS`：只记日志（S1 未启用按需填充，`FETCH_PLACEHOLDERS`
  恒返回空）。

### 3. 通知通道：独立有界队列 + 单消费者线程

`std::sync::mpsc::sync_channel(256)` + 单消费者线程 `pandock-native-sync-notify`。
**不复用 hydration 的 `try_begin_worker` 额度**：通知不能把召回挤掉，召回也不能让
通知堆积。队列满即丢弃并 `warn`——绝不阻塞 Explorer 回调线程。

### 4. 自杀回调按 `source` 去重

应用自身发起的 CF 操作（`dehydrate` / `remove_placeholder` / 召回水合）都会反过来触发
通知。若不去重，`sync_now` 的「远端删除 → 删本地占位符」会被误记成「用户删除了文件」。
实现：路径守卫 + fs_id 守卫（30s TTL），**一次应用操作只抵消一次通知**；命中的事件
`source="app"`，只留痕、**不产生** `pending_op`。

来源语义：`app`（命中守卫）、`explorer`（无守卫的意图型通知）、`system`（完成型通知，
平台侧发出，无法区分具体发起者）、`unknown`。

### 5. 紧急开关：配置字段 + 环境变量覆盖

- 配置：`config.json` 的 `native_sync_notifications`（默认 `true`）；
  `config.json` 全量使用 **snake_case** 键（`Config` 未加 `rename_all`，与 `app_key`/`launch_at_login` 一致）；
  camelCase 仅用于 Tauri 命令与事件的 payload。
- 环境变量 `PANDOCK_NATIVESYNC_NOTIFICATIONS`（`0/false/no/off` 关闭）**优先级最高**，
  用于回调卡死时免编辑恢复；
- 启动时把生效值写入日志（`native-sync notifications effective=… config=… env=…`）；
- 恢复步骤见 `docs/operations.md`；
- **设置页字段不在本切片**（避免与 UI 任务并行改 `ui/`），列为 S2 的 UI 事项。

### 6. H1：「不得无提示复活」

注册 `NOTIFY_DELETE` / `NOTIFY_RENAME` 之后，`sync_now` 会同时看到「本地已删除/改名」
与「云端仍在」。旧逻辑会把云端条目重新建回占位符，于是：

- 用户删掉的文件会在下次同步**无声复活**；
- 改名会多出一个**旧路径副本**。

修复：`sync_now` 遇到 `pending_op ∈ {delete, rename}` 的云端条目时
**跳过占位符重建**，保留 pending 标记并计入 `attentionEntries`；同时仍把它登记进
本轮 `seen`（否则会被 `missing_entries` 误判成远端删除而清掉本地记录）。

配套的健壮性修复：远端删除对账时，本地路径已不存在的条目**直接记账**，不再调用
平台删除接口（对不存在的路径调用会 `ENOENT` 并中断整轮同步）。

### 7. DB v3：本地变更与「待同步轴」

纯增量迁移（沿用 `pragma_table_info` 探测 + `ALTER TABLE`，打开前已生成 `.bak`）：

- 新表 `local_change`（`id, fs_id, kind, phase, from_relative, to_relative, is_dir,
  reason, source, detected_at, applied_to_cloud, error`）+ 索引
  `local_change_pending(applied_to_cloud, id)`。**S1 只写不消费**，S2/S3 才读取上传。
- `local_state` 增列 `pending_op` / `pending_since` / `snapshot_mtime` / `snapshot_size`。
  `pending_op` 是与 hydration 轴**正交**的第二条状态轴。
- `remote_entry` 增列 `local_only`。
- `missing_entries()` 增加 `AND r.local_only=0`：本地新建条目不得被判「远端删除」。
- `upsert` 的冲突子句改为 `pending_op IS NULL` 时才覆盖 `relative_path`，
  防止本地改名被下一次云端回写覆盖。
- `attention_entries` 纳入 `pending_op ∈ {delete, rename}`。

不存 token / secret / dlink（延续 ADR-103 的约束）。

### 8. 本地新建条目的表示与三条出口守卫

本地新建（云端尚无对应物）的条目与云端条目**同表**存放，占用**负数 fs_id 保留区间**
（从 -1 递减），`remote_path` 为不可解析的 `local:` 前缀（好处是不与云端路径争唯一约束）。
`local_only` 列是**唯一权威标记**，不得靠符号位推断。

结构性守卫（不是「靠约定」）——以下三条出口对本地新建条目**返回显式错误**，而不是构造
非法云端请求，并各有单元测试断言被拒绝：

1. `fs_id → RemotePath`（`IndexDb::mapping_by_fs_id`，召回解析的唯一入口）；
2. `fs_id → 占位符 identity`（`IndexDb::cloud_fs_id`）；
3. `fs_id → by_id 内存映射`（`IndexDb::all_mappings` 过滤 `local_only`，
   即内存映射预热的唯一来源）。

契约对齐：`NativeSyncEntryView.fsId` 改为 `Option<u64>` + `skip_serializing_if`，
**负数内部 id 不下发 UI**（UI 侧按数字读取的既有行为不受影响）。

本实现期间未出现「守卫点难以穷举」的情况，因此**不改为单独立表**。

### 9. 本地写入识别（关闭完成通知）

`NOTIFY_FILE_CLOSE_COMPLETION` 是「本地可能已被写入」的唯一可靠信号。用磁盘的
`mtime`/`size` 与占位符创建 / 上次水合时记录的快照比对：

- 有漂移 → `pending_op='upload'`（本地编辑）；
- 云端无对应条目且**不是云占位符** → 登记 `local_only` 条目（本地新建）；
- 水合完成后刷新快照，避免把平台自己的写入误判成本地编辑。

### 10. 事件契约（供 UI 接入）

- `pandock://nativesync/state`：payload 与 `native_sync_status` 返回同构。
  新增字段：`pendingEntries: number`、`notificationsEnabled: boolean`；
  `NativeSyncEntryView` 新增 `pendingOperation?: "upload"|"delete"|"rename"`、
  `pendingSince?: string`，`fsId` 改为可选。
- `pandock://nativesync/change`：payload 为 `NativeSyncChangeEvent`
  （`kind` / `phase` / `fsId?` / `fromRelative?` / `toRelative?` / `isDir` / `reason?` /
  `source` / `detectedAt` / `pendingCloud`）。变更事件立即推送；状态统计用
  **300ms last-wins 节流**重推，避免批量操作刷爆设置页。

### 11. 门禁变更（`chore(ci)` 单独提交）

- `scripts/check.ps1` 增 `cargo test -p pandock-nativesync`；
- `.github/workflows/build.yml` 的 **windows 作业** 增同一条（macOS 作业不涉及）；
- **不给 nativesync 设覆盖率阈值**：其主体在 `cfg(windows)` 下，跨平台统计会得到假指标。

### 12. 门禁漏检的归因纠正（重要）

在本次工作早期，我把「`cargo fmt` 漏检」错误归因为「nativesync 不在门禁范围内」。
**正确原因是：该分支从未执行过 `scripts/check.ps1`。** `cargo fmt --all` 覆盖全部
crate（含 nativesync），门禁一直是完整的；漏检纯粹是流程未执行。

## 影响

- 本地删除/改名/脱水/开闭完成**实时可见**（`pendingEntries` + `pandock://nativesync/change`），
  不再需要等下一次 `sync_now`。
- 用户删掉的文件**不再被下次同步无声复活**；改名**不再产生旧路径副本**。
- 仍然**不上传**：`pending` 只落库，S1 结束时云端无任何变更。
- 设置页的视觉呈现（条目徽标 / 顶部汇总 / 「本地已修改，尚未同步到云端」文案）
  由独立 UI 小切片实现，**不在本切片**。

## 替代方案

- 只注册 `NOTIFY_DELETE` / `NOTIFY_RENAME`，不注册开闭完成：无法识别本地编辑，放弃。
- 通知也在回调线程里落库：会阻塞 Explorer，违反 CFAPI 纪律，放弃。
- 本地新建条目单独立表：本切片守卫点只有三处、可控，同表 + `local_only` 更简单；若未来
  守卫点扩散再改判。
- 用 `#[allow(dead_code)]` 掩盖未接线的 API：不采用；未消费的 API 直接删除，
  由消费方（S2/S3）需要时再加。

## 实机验收清单（8 项，合并部署后执行）

1. 启用 → `sync_now` → 占位符状态/统计正确（`pendingEntries=0`）；
2. Explorer 双击小文件：打开成功、内容 md5 与远端一致；
3. Explorer 双击 >4MiB 文件：打开成功、内容 md5 与远端一致（分块召回）；
4. 断网后双击：快速失败（不挂起 60s）、错误可见；
5. 取消召回：不留 `running` job；
6. 删除文件后点「立即同步」：**不得复活**，`pendingEntries`/`attentionEntries` 增加；
7. 改名后点「立即同步」：**不得出现旧路径副本**，新路径为 `pending_operation=rename`；
8. 暂停/退出应用期间在 Explorer 操作：不得挂起（停止实例需用户授权）。

场景 6/7/8 的判定证据取自**命令层与数据库**（`nativesync_list` / `status` /
`pandock://nativesync/change` / `sync.db`），不依赖设置页视觉。
