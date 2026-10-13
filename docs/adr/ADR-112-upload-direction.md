# ADR-112：上传方向（S2）设计——本地变更 → 云端自动跟上

- 状态：**草案（待开发总监终审）**
- 日期：2026-10-13
- 关联：ADR-109（本地变更捕获，已落地）、ADR-111（无感对账轮询，已终审）、
  ADR-110（退出不注销同步根）、`core/src/baidu.rs`（`upload_file` 三段式上传，WebDAV PUT 在用）、
  `nativesync/src/platform.rs`（NOTIFY_* 捕获与 pending_op）

## 1. 背景与用户决策（原话落档，2026-10-13 拍板）

S1 实现了单向 remote→local；S2 把同步根升级为**双向工作区**的第一步：本地的新建/修改/重命名/删除自动传播到云端。
用户对 4 个产品问题的裁决（原话）：

1. **冲突策略**：按照 OneDrive 的做法；
2. **删除语义**：移入回收站；
3. **同步根是不是双向工作区**：照 OneDrive；
4. **配额/限速**：不限制。

**范围纪律**：S2 只做 **本地→云端**（上传 + 本地删除传播为云端删除）；**云端删除→本地**归 S3，不在本 ADR。

## 2. OneDrive 官方口径（冲突/双向的依据）

- **冲突（非 Office 文件）**：OneDrive **保留双方**——云端版本**保留原文件名**，本地冲突副本**重命名为 `<原名>-<设备名>.<扩展名>`**。
  依据：Microsoft Learn《Resolve sync issues in OneDrive for work or school》原文："If a conflict occurred with a non-Office file type,
  OneDrive automatically keeps both versions. The online version keeps the original file name…"。
  Office 的「合并/保留双份」弹窗**不在范围**（Pandock 只做通用文件）。
  ⇒ Pandock **不做 last-writer-wins**：同路径双向并发修改 ⇒ 云端占原名，本地副本改名保留（设备名后缀），并进「需处理」集合可见。
- **双向工作区**：OneDrive 同步根**就是双向工作区**（本地新建/修改自动上传、云端变更自动下行；Files On-Demand 只是水合策略，不改变双向语义）。
  ⇒ Pandock 同步根按此升级。

## 3. 六个问题的逐项决策

### 3.1 上传捕获（本地新建/修改如何可靠识别）

**决策**：完全复用 ADR-109 已落地的 CF 通知捕获，不新增机制；**触发上传的判定 = close-completion + 快照比对**。

- 现有捕获面（`nativesync/src/platform.rs`）：`NOTIFY_RENAME(_COMPLETION)`、`NOTIFY_DELETE(_COMPLETION)`、
  `NOTIFY_FILE_OPEN_COMPLETION` / `NOTIFY_FILE_CLOSE_COMPLETION`；索引 `pending_op` 已落库。
- 「读≠改」已由 LocalSnapshot 修复并区分「**没有基线**」vs「**基线为 0**」（S1 已修）——上传判定沿用同一套快照比对：
  close 时快照与基线不一致 ⇒ `pending_op=upsert`；仅读取 ⇒ 快照一致 ⇒ 不标记。**绝不把打开/读取当修改上传**。
- **防抖**：编辑器（Office 类）一次保存会产生多次 open/close；close 触发后不立即上传，进**防抖窗口**（默认 3 秒，§3.6 决策点），
  窗口内再次变更则顺延；窗口结束文件无未关闭句柄才真正派发上传。
- rename/delete 的 completion 直接落 `pending_op`（无需防抖）。

### 3.2 上传执行（百度三段式 + 配额不限制的准确解读）

**决策**：复用 `core/src/baidu.rs` 的 `upload_file`（`precreate → superfile2 分片 → create`，WebDAV PUT 已在生产使用），
封装为 nativesync 的上传 worker（**串行队列**，§3.6 决策点）。

百度开放平台官方文档（2026-09-09 更新，链接见附录）核实：

1. **预上传**：`POST https://pan.baidu.com/rest/2.0/xpan/file?method=precreate`，
   参数 `path/size/isdir/block_list`（各分片 MD5 的 JSON 数组，32 位小写）`/autoinit=1/rtype`；
   返回 `uploadid` 与待传分片序号列表 `block_list`（秒传时为空）。
2. **分片上传**：`POST https://<UPLOAD_HOST>/rest/2.0/pcs/superfile2?method=upload&type=tmpfile`（multipart），
   ≤4MB 一次完成；>4MB 按分片逐个上传。**上传域名必须由 `locateupload` 动态获取**（`GET https://d.pcs.baidu.com/rest/2.0/pcs/file?method=locateupload`），
   文档原文："不要在生产代码中固定使用示例域名"——现有实现已遵守。
3. **创建文件**：`POST https://pan.baidu.com/rest/2.0/xpan/file?method=create`（合并分片），返回 `fs_id/md5/size/ctime/mtime`。
4. **关键错误码**：`-10` 容量不足；`31299` 首片小于 4MB；`31363` 分片缺失；`31364` 超出分片大小限制；`31024` 未开通上传权限；`-8` 文件已存在。

**rtype 的关键分叉**：WebDAV PUT 现用 `rtype=3`（覆盖同名）——那是 WebDAV 的语义契约，**不动**；
NativeSync 上传用 **`rtype=0`（冲突时失败）**，以便把「云端在我基线之后被改」暴露为可检测的冲突（§3.3），
否则 rtype=3 会把云端版本静默覆盖 = last-writer-wins，**违背用户「按 OneDrive」的裁决**。

**「配额不限制」的准确解读**：不设人为限速/并发配额/每日上限；但——
- API 自身的限流与错误（网络错误、5xx、`-10` 容量不足）必须有**退避**（沿用 ADR-111 节奏：2m→4m→8m，封顶 60m）；
- `-10` 容量不足是**硬错误**：不重试，进「需处理」集合（用户须清理空间或升级）；
- 单文件/分片上限按百度授权身份限制（文档 31364/31365），**不自我设限**。

### 3.3 冲突落地（保留双方 + 设备名后缀）

**决策**（按 §2 的 OneDrive 口径）：

- **检测**：上传前索引中该路径的基线 `(fs_id, size, md5/mtime)` 是「我上次见到的云端版本」。
  `rtype=0` 下 `create` 返回 `-8`（已存在）即说明云端在基线之后被改 ⇒ **冲突**。
  （`precreate` 的秒传命中不算冲突——内容完全一致时秒传成功即「双方已一致」，直接回写基线。）
- **执行**：
  1. **云端版本占原名**，不动；
  2. **本地副本改名**为 `<原名>-<设备名>.<扩展名>`（设备名 = Windows 计算机名，去非法字符），保留用户内容，状态 hydrated；
  3. 冲突事件进「需处理」集合（沿用现有 attention/需处理状态位），设置页可见，**不弹窗**。
- **改名副本是否随后回传云端**：OneDrive 的实际行为是回传（双方最终都在云端）。**默认跟随（回传）**，列为 §3.6 决策点。
- 双向并发删除 vs 修改（本地改了、云端被删）：本地修改版本按冲突副本改名保留并回传为新文件（等同于「保留双方」）——同一逻辑覆盖，不单列。

### 3.4 删除语义（本地删除 → 云端移回收站）

**决策**：本地删除（`NOTIFY_DELETE_COMPLETION` 落 `pending_op=delete`）⇒ 云端 `filemanager opera=delete`
（`POST /rest/2.0/xpan/file?method=filemanager&opera=delete`，与 WebDAV DELETE 同一接口，生产在用）。

**回收站语义的核实结果（如实标注）**：百度开放平台《管理文件》官方文档（附录链接）只写了 `opera=delete` 的
参数（`async`、`filelist`、`ondup` 不适用于 delete）与 `errno/info[].errno/taskid` 返回结构，
**没有写**「删除是否进回收站 / 保留期 / 是否有彻底删除之分」——**未能定位，需实测**（不编造）。
实测方案：用测试账号经 API 删除一个文件，在百度网盘网页/App 回收站查看是否出现，结果回写本 ADR。
- 若实测 `opera=delete` **进回收站** ⇒ 直接满足用户裁决；
- 若**不进** ⇒ 开放平台无「移入回收站」API（文档未见），如实上报总监裁决替代方案。
- 附注：个人版网盘回收站保留期（普通 10 天 / V1 15 天 / 超级会员 30 天，百度帮助页）是**网盘产品侧**口径，
  **不等于** API `opera=delete` 的语义，不能拿它当依据。

**防误删护栏**：删除传播**仅当索引中存在该条目的基线**时执行——「索引缺失」与「用户删除」必须区分
（否则索引损坏会被放大成云端批量删除）。占位符状态下删除 = 云端删除 + 移除占位符条目。

### 3.5 边界与 ADR-111 的衔接（回声抑制，本设计最难的一题）

**决策**：

- **S2 只做本地→云端**；云端删除→本地归 S3。S3 之前，轮询发现「云端缺失」的条目**不动本地**、只记差异（现状即如此）。
- **回声抑制（核心）**：我们刚上传/删除/改名成功的路径，下一轮 ADR-111 轮询会在云端看到——必须让它判定「一致」而不是「云端变更」：
  1. **上传成功即回写基线**：`create` 响应的 `(fs_id, md5, size, ctime/mtime)` 立即写入索引 `remote_entry`/`local_state`，
     轮询按 §2.2 比对键比对时即「一致」，不会误判为云端新增/变更；
  2. **删除传播成功即移除索引条目**（remote_entry + local_state），轮询看不到差异；
  3. **上传中**（pending_op=upsert 且 worker 在跑）的路径：`create` 完成前云端不可见或仍是旧版 ⇒
     轮询遇到「索引有 pending 上传」的路径**跳过裁决**（不判冲突、不下行），等上传完成回写基线后自然收敛；
  4. **改名**：本地 rename ⇒ 云端 `filemanager opera=move`（保留 fs_id、省流量），成功后回写索引的 remote_path；
     不走 delete+upload。
- **与轮询的互斥**：上传 worker 与对账引擎共享索引连接但**不共享锁跨网络调用**（沿用 S1 纪律）；
  对账层与上传队列各自单飞。

### 3.6 决策点清单（待总监裁决，默认值已给出）

| # | 决策点 | 默认 | 说明 |
|---|---|---|---|
| 1 | 上传触发防抖窗口 | **3 秒** | 编辑器连续保存合并为一次上传 |
| 2 | 上传并发 | **1（串行队列）** | 大文件分片串行；多文件排队，不与下载抢配额 |
| 3 | NativeSync 上传 rtype | **0（冲突失败）** | 冲突可检测；WebDAV PUT 保持 rtype=3 不动 |
| 4 | 冲突副本改名模板 | **`<原名>-<设备名>.<扩展名>`** | 设备名 = Windows 计算机名 |
| 5 | 冲突副本是否回传云端 | **回传（跟随 OneDrive）** | 关闭则副本仅本地保留 |
| 6 | 上传失败退避 | **2m→4m→8m，封顶 60m** | 沿用 ADR-111 节奏；连续 ≥5 次进「需处理」 |
| 7 | 删除传播开关 | **`native_sync_propagate_delete=true`** | 关闭则本地删除不影响云端 |
| 8 | 配额/限速 | **不限制**（用户裁决） | 仅 API 错误退避，不设人为上限 |

## 4. 实现序（提示，不在本 ADR 承诺交付）

1. 上传队列与 worker（串行、防抖、退避、回写基线）；
2. 冲突检测与改名保留（含「需处理」集合状态位）；
3. 删除/改名传播（护栏 + 索引回写）；
4. 回声抑制联调（与 ADR-111 调度器同库同事件面）；
5. **实测**回收站语义并回写 §3.4。

## 5. 不做的事（范围纪律）

- 不含云端删除→本地（S3）；不含 Office 式「合并」；不含多设备同时在线的实时冲突（无推送，只有轮询延迟内的并发窗口）；
- 不改 WebDAV 的上传/删除语义（rtype=3、同步 filemanager 均为既有契约）；
- 不设人为限速/配额（用户裁决），但保留 API 错误退避。

## 6. 风险与回滚

- **回声抑制失效 ⇒ 自我冲突风暴**（自己上传的版本被轮询判成「云端变更」再下行/报冲突）：
  缓解 = 上传成功**同步回写基线**后才算完成；回滚 = 配置关闭上传方向，退回 S1 单向。
- **误删传播**：护栏（无基线不传播）+ 若实测进回收站则有回收站兜底；`native_sync_propagate_delete=false` 可整体关闭。
- **配额**：不限制 ≠ 无退避；限流错误按 §3.2 退避，不会烧配额或刷日志。
- **秒传误判**：precreate 秒传命中即内容一致，不算冲突（§3.3），避免把「双方恰好相同」误报。

## 附录：官方文档证据链接（2026-10-13 抓取核实）

- 预上传：`https://pan.baidu.com/union/doc/基础网盘服务/上传/预上传/`
- 分片上传：`https://pan.baidu.com/union/doc/基础网盘服务/上传/分片上传/`
- 创建文件：`https://pan.baidu.com/union/doc/基础网盘服务/上传/创建文件/`
- 获取上传域名：`https://pan.baidu.com/union/doc/基础网盘服务/上传/获取上传域名/`
- 管理文件（filemanager，删除语义**未写明回收站**）：`https://pan.baidu.com/union/doc/基础网盘服务/管理文件`
- OneDrive 冲突口径：Microsoft Learn《Resolve sync issues in OneDrive for work or school》（开发总监提供）
