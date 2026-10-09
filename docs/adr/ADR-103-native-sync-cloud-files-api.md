# ADR-103：NativeSync 采用 Windows Cloud Files API，第一阶段单向 remote -> local

- 状态：草案，待审核
- 日期：2026-09-24
- 关联：`docs/release/1.1.0-native-sync-plan.md`
- 关联既有决策：ADR-101（私有发布渠道）、ADR-102（portable updater）

## 背景

CloudDock 1.0.0 已交付 WebDAV 数据面。1.1.0 目标是为 Windows 用户提供 OneDrive 式体验：占位符、按需下载、释放空间、Explorer 原生状态。可选技术路线：

1. Windows Cloud Files API（cfapi/CldApi）；
2. 第三方虚拟文件系统（Dokan / WinFsp / ProjFS）；
3. 在 WebDAV 之上再包一层同步逻辑；
4. 自研 minifilter 驱动。

## 决策

1. **采用 Windows Cloud Files API 实现 NativeSync**，作为与 WebDAV 并列的第二个数据面，两者共享 `CloudFs`/`BaiduClient`/auth，NativeSync 不经过 WebDAV。
2. **第一阶段只做 remote -> local 单向**：占位符、hydration、释放空间、状态恢复；不上传、不传播本地删除/重命名/修改。
3. **本地索引使用 SQLite**（rusqlite bundled），存放于 `data/native-sync/sync.db`，记录远端 fs_id/路径/大小/mtime/etag 与本地 hydration 状态。
4. **非 packaged 优先**：1.1.0 不引入 MSIX/sparse package；自定义 Shell 扩展推迟。若 spike（V1/V4/V5）证明基本体验在非 packaged 下不可用，再回开发总监重新裁决。
5. **数据安全优先于同步及时性**：任何自动流程不得删除或覆盖含数据的本地文件；释放空间前强制 md5 校验；远端删除 hydrated 文件时只标记不删除。
6. 不改变 1.0.0 的 WebDAV、百度 auth flow、app/ 更新链与 GitHub updater；NativeSync 默认关闭，用户显式启用。

## 理由

- Cloud Files API 是 Windows 官方同步引擎接口（1709+，desktop apps），提供占位符、pin 状态、in-sync 状态与 Explorer 集成，OneDrive 与 Cloudreve Desktop 均基于此；
- Dokan/WinFsp 需要内核驱动签名与安装，违背 portable 绿色分发；自研驱动成本与风险不可接受；
- 经 WebDAV 包装会叠加 HTTP 往返与语义错配，且无法提供占位符/pin/状态图标；
- 单向先行把风险最高的“自动删除/覆盖用户数据”排除在第一版之外，符合不可恢复操作纪律；
- SQLite 满足索引/迁移/事务需求，与 1.0.0 技术栈一致，无额外运行时。

## 后果

正面：

- Explorer 原生 OneDrive 式体验，无需内核驱动，保持 portable；
- 与 WebDAV 共享 provider，维护单一百度适配层；
- 单向范围使 1.1.0 可在 ~6-8 周内交付且数据安全可控；
- SQLite 索引为 1.2.0 双向同步打底。

负面：

- 仅 Windows 1903+；macOS 需另起 File Provider 路线；
- 百度无增量事件，远端变更依赖轮询，时效性弱于 Cloudreve（SSE）；
- 非 packaged 下 shell 集成深度受限（待 spike 确认）；
- CF API 回调为原生 FFI，工程复杂度和测试成本高于纯 HTTP。

## 后续

- spike 报告作为本 ADR 的验证附件；
- 双向同步、MSIX/shell 深度集成、macOS File Provider 各自单独 ADR。
## 修订（2026-09-25）：D-1~D-6 裁决与 S0 spike 验证

- D-1~D-6 已由用户裁决（详见 `docs/release/1.1.0-native-sync-plan.md` §16）：sync root 默认“文档\CloudDock”（Known Folder API）；MSIX 推迟 1.2.0；文件名可逆编码；hydrated 文件远端删除只标记不删；默认不做全树后台轮询；NativeSync 默认关闭。
- S0 spike 完成（`docs/release/1.1.0-native-sync-s0-spike-report.md`）：V1/V2/V3/V6/V7/V8/V9/V10 全部通过，验证环境 Windows 11 25H2（build 26200）；非 packaged 注册/连接/占位符/hydration/dehydration/重连/失败快速返回/注销均成立。
- V4（Explorer 内置动词）与 V5（状态图标）未获证实，已按放行约束回报总监；若人工复核确认缺失，体验降级路径为设置页操作，sparse package 评估时点由总监裁决（默认维持 1.2.0）。
- 关键技术修正入库：`ParamSize = offsetof+sizeof`、目录句柄需 `FILE_FLAG_BACKUP_SEMANTICS`、provider 进程内完成占位符创建、释放空间用 `CfDehydratePlaceholder`（非 `CfRevertPlaceholder`）、失败应答用 `STATUS_CLOUD_FILE_UNSUCCESSFUL` + 请求区间、FETCH_PLACEHOLDERS 空应答需 `DISABLE_ON_DEMAND_POPULATION`、windows crate 固定 0.58。
## 修订（2026-09-26）：S1/D-7 管理台契约

S1 新增 `clouddock-nativesync` crate 与 `src-tauri/src/native_sync.rs`。管理台通过 D-7 Tauri commands 访问 NativeSync；`status/list` 不因尚未授权而无法渲染，`sync_now` 才在命令执行时注入百度只读 backend。D-7 的 camelCase 返回字段、action 安全门和 manager window label 以 `docs/release/1.1.0-native-sync-plan.md` §18 为准。

S1 的 Cloud Files callback 当前采用“安全失败”桥接：未完成百度异步 hydration 到 CF_OPERATION_TRANSFER_DATA 的非阻塞桥接前，不伪造文件内容；这避免阻塞 Explorer 或泄露 dlink。占位符注册、路径/卷校验、SQLite 索引、按需/手动枚举、管理台状态和安全 action 已实现。hydration async bridge 作为下一步必须完成的实现风险保留在提交材料中。

### S1 实现状态（2026-09-26）

已完成：`clouddock-nativesync` crate、Windows 0.58 Cloud Files API 注册/连接/占位符/pin/dehydrate/stub、Known Folder/卷校验、SQLite WAL schema/备份/分页、D-7 Tauri commands、路径净化和远端删除安全策略。NativeSync 不经过 WebDAV。

明确未宣称完成：CF `FETCH_DATA` 到百度 `read_at` 的生产 hydration worker。当前 callback 仍采用安全失败路径，避免阻塞 Explorer 或伪造内容；S1 审核必须将该项视为剩余阻塞，不能把“占位符可创建”当作“按需打开可用”。下一提交应完成 callback context/backend 注入、4 MiB Range 分块、取消、失败状态落库和真实 Windows 手工验收后，才可宣称 S1 完整完成。

## S1 审核状态补充（2026-09-26）

当前提交是 S1 骨架与 D-7 后端候选。最重要的未完成项仍为 Windows `FETCH_DATA` -> 百度只读 Range worker 的非阻塞桥接；在该项实现并经 Explorer 手工验证前，不应将 S1 标记完整/可发布。Known Folder fallback、远端目录含 hydrated 后代的删除保护、canonical path 越界防护均列为强制安全门。

## 修订（2026-09-26）：FETCH_DATA hydration worker 已接入

此前“真实百度 hydration worker 尚未完成”的审查结论已由下一提交修正：Windows platform 在连接上下文保存只读 `CloudBackend`、sync-root 映射和 `ResourceSnapshot`，`FETCH_DATA` 回调只复制请求值并投递后台 worker。worker 使用 `read_at` 4 MiB 分块、20 秒总等待上限、可取消读取、`CfExecute(TRANSFER_DATA)`、`hydration_job` 进度/终态落库和完整文件 md5/size 校验；只有完整请求才调用 `CfSetInSyncState`。

本修订不等同于真实 Windows Explorer/百度账号验收完成：本机未安装 Windows target 与 coverage runtime，手工验证仍需在 Windows 验证机按 S1 报告步骤执行。在该验证完成前不标记发布候选，不 merge/push/deploy。
