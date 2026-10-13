use crate::backend::CloudBackend;
use crate::db::IndexDb;
use crate::model::{
    EntryAction, EntryRecord, NativeSyncAction, NativeSyncEntryView, NativeSyncEvent,
    NativeSyncListView, NativeSyncState, NativeSyncStatusView, PinState,
};
use crate::path::{default_root_path, sanitize_component, validate_root_path};
use crate::platform::{new_platform, PlatformError, PlatformSync};
use crate::scheduler::AutoSyncParams;
use crate::upload::{self, UploadDrainReport, UploadParams};
use pandock_core::cloud::{CloudError, RemotePath, ResourceSnapshot};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum NativeSyncError {
    #[error("NativeSync 不可用: {0}")]
    Unsupported(String),
    #[error("NativeSync: {0}")]
    Message(String),
    #[error("百度网盘: {0}")]
    Backend(String),
}
impl From<PlatformError> for NativeSyncError {
    fn from(e: PlatformError) -> Self {
        Self::Message(e.to_string())
    }
}

/// 一次自动对账层（增量）的报告：日志汇总与调度器决策的依据。
#[derive(Clone, Debug, Default)]
pub struct AutoLayerReport {
    /// 折算后的 API 调用数（`count / LIST_PAGE_SIZE + 1`，翻页计入）。
    pub calls: u32,
    pub added: u64,
    pub changed: u64,
    /// 远端删除 → 本地移除占位符并 tombstone 的条数。
    pub removed: u64,
    /// 远端删除但本地 hydrated/有水合后代 → 仅标 remote_missing 的条数。
    pub remote_missing_marked: u64,
    pub drilled: u32,
    pub deep_scanned: u32,
    /// 本地侧对账（Gap A/B/C 捕获）：本层新发现的本地新建条数。
    pub local_new: u32,
    /// 本地侧对账：本层新发现的本地编辑条数（md5 漏斗判定为真改动）。
    pub local_edited: u32,
    /// 因预算/上限被推迟到下一层的目录数。
    pub deferred: u32,
    /// 未执行的原因（no_root / disabled / paused）；None = 执行了。
    pub skipped: Option<&'static str>,
    /// 被手动「立即对账」抢占：部分完成，不算失败。
    pub preempted: bool,
    /// 本层失败后的连续失败数（成功层为 0）。
    pub consecutive_failures: u32,
}

/// 一次目录列举折算的 API 调用数：PAGE_SIZE 整页时会多一次空页请求（core baidu.rs 的循环）。
fn list_call_cost(entry_count: usize) -> u32 {
    (entry_count / pandock_core::baidu::LIST_PAGE_SIZE + 1) as u32
}

/// 云端条目相对索引基线是否变化（ADR-111 比对键）：size 不同 ⇒ 变；
/// md5 双侧都有 ⇒ 比 md5；缺任一侧 ⇒ 退化为 mtime（定稿：可接受残余风险）。
fn remote_content_changed(
    prior: Option<(u64, Option<String>, Option<i64>)>,
    size: u64,
    md5: &Option<String>,
    modified: Option<i64>,
) -> bool {
    match prior {
        None => true,
        Some((prior_size, prior_md5, prior_mtime)) => {
            if prior_size != size {
                return true;
            }
            match (prior_md5, md5) {
                (Some(a), Some(b)) => a != *b,
                _ => prior_mtime != modified,
            }
        }
    }
}

/// 上传任务效果（report 计数用）。
enum UploadTaskEffect {
    Uploaded,
    DirCreated,
    Conflicted,
    Deleted,
    Renamed,
    Skipped,
}

/// 上传任务失败：Hard = 只影响该条目（继续下一个）；Retryable = 中断本层（退避交给调度层）。
enum UploadTaskError {
    Hard(String),
    Retryable(String),
}
impl UploadTaskError {
    fn into_message(self) -> String {
        match self {
            Self::Hard(message) | Self::Retryable(message) => message,
        }
    }
}
impl From<NativeSyncError> for UploadTaskError {
    fn from(error: NativeSyncError) -> Self {
        Self::Retryable(error.to_string())
    }
}

/// 上传目标远端路径：本地新建（或 remote_path 还是 local: 前缀）⇒ 按相对路径推导；
/// 云端已知条目 ⇒ 用索引里的真实云端路径（大小写/净化差异以云端为准）。
fn remote_for_upload(
    relative: &str,
    remote_path: &str,
    local_only: bool,
) -> Result<RemotePath, String> {
    let candidate = if local_only || remote_path.starts_with("local:") {
        format!("/{relative}")
    } else {
        remote_path.to_string()
    };
    RemotePath::parse(&candidate).map_err(|e| format!("远端路径非法: {e}"))
}

/// SystemTime → epoch 秒（索引 modified_at 契约）。
fn epoch_of(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// 单个目录列举的消化结果（全量 BFS 与增量层共用）。
struct DirIngestOutcome {
    seen: Vec<u64>,
    subdirs: Vec<RemotePath>,
    /// 根快查中「新增或内容变化」的顶层目录（定向下钻候选）。
    changed_top_dirs: Vec<RemotePath>,
    added: u64,
    changed: u64,
}

pub struct NativeSyncManager {
    db: IndexDb,
    backend: Option<Arc<dyn CloudBackend>>,
    platform: Box<dyn PlatformSync>,
    /// 配置文件里的 `native_sync_notifications`（默认 true；config.json 用 snake_case）。
    notifications_config: bool,
    /// 生效值 = 环境变量 `PANDOCK_NATIVESYNC_NOTIFICATIONS` 覆盖配置字段。
    notifications_enabled: bool,
}
impl NativeSyncManager {
    pub fn new(
        data_dir: &Path,
        backend: Option<Arc<dyn CloudBackend>>,
    ) -> Result<Self, NativeSyncError> {
        let notifications_config = true;
        let notifications_enabled =
            crate::notify::resolve_notifications_enabled(notifications_config);
        let mut platform = new_platform();
        platform.set_notifications_enabled(notifications_enabled);
        // 启动时记录生效值：卡死时用户需要知道往哪看。
        log::info!(
            "native-sync notifications effective={} config={} env={} (env 优先，置 0 可整体关闭捕获)",
            notifications_enabled,
            notifications_config,
            std::env::var(crate::notify::NOTIFICATIONS_ENV).unwrap_or_else(|_| "<unset>".into())
        );
        Ok(Self {
            db: IndexDb::open(data_dir).map_err(NativeSyncError::Message)?,
            backend,
            platform,
            notifications_config,
            notifications_enabled,
        })
    }
    /// 应用配置文件里的 `native_sync_notifications`；环境变量优先级更高。
    pub fn set_notifications_config(&mut self, value: bool) {
        self.notifications_config = value;
        self.apply_notifications_switch();
    }
    fn apply_notifications_switch(&mut self) {
        let effective = crate::notify::resolve_notifications_enabled(self.notifications_config);
        if effective != self.notifications_enabled {
            log::info!("native-sync notifications effective={effective}");
        }
        self.notifications_enabled = effective;
        self.platform.set_notifications_enabled(effective);
    }
    pub fn set_backend(&mut self, backend: Arc<dyn CloudBackend>) {
        self.backend = Some(backend);
    }
    pub fn set_event_sink(&mut self, sink: Option<Arc<dyn Fn(NativeSyncEvent) + Send + Sync>>) {
        self.platform.set_event_sink(sink);
    }
    /// 本地变更事件出口（`pandock://nativesync/change`）。
    pub fn set_change_sink(
        &mut self,
        sink: Option<Arc<dyn Fn(crate::model::NativeSyncChangeEvent) + Send + Sync>>,
    ) {
        self.platform.set_change_sink(sink);
    }
    /// 状态查询显式传播内部错误：db 损坏/不可读时返回 Err，
    /// 调用方（Tauri 命令/事件推送）应把真实错误呈现给 UI，而不是伪装成"未启用"。
    pub fn status(&self) -> Result<NativeSyncStatusView, NativeSyncError> {
        let supported = self.platform.supported();
        let (root, enabled, state) = self
            .db
            .root()
            .map_err(NativeSyncError::Message)?
            .map(|(p, e, s)| (Some(p), e, s))
            .unwrap_or((None, false, "disabled".into()));
        let (total, placeholders, hydrated, attention, jobs) =
            self.db.counts().map_err(NativeSyncError::Message)?;
        let (failures, paused, backoff_until, last_layer_at, last_ok_at, last_error) =
            self.db.auto_counters().map_err(NativeSyncError::Message)?;
        let auto_sync_state = if !enabled {
            "disabled"
        } else if paused {
            "paused"
        } else if failures > 0 {
            "backing_off"
        } else {
            "ok"
        };
        Ok(NativeSyncStatusView {
            enabled,
            supported: supported.is_ok(),
            support_reason: supported.err(),
            root_path: root.map(|p| p.to_string_lossy().to_string()),
            state,
            total_entries: total,
            placeholder_entries: placeholders,
            hydrated_entries: hydrated,
            attention_entries: attention,
            pending_entries: self.db.pending_count().map_err(NativeSyncError::Message)?,
            notifications_enabled: self.notifications_enabled,
            active_jobs: jobs,
            last_sync_at: last_ok_at,
            last_error,
            auto_sync_state: auto_sync_state.into(),
            consecutive_failures: failures as u64,
            backoff_until,
            last_layer_at,
        })
    }
    pub fn enable(
        &mut self,
        root: Option<PathBuf>,
    ) -> Result<NativeSyncStatusView, NativeSyncError> {
        if self.platform.supported().is_err() {
            return Err(NativeSyncError::Unsupported(
                "Windows 10 1903+ Cloud Files API".into(),
            ));
        }
        let root = match root {
            Some(p) => p,
            None => default_root_path().map_err(NativeSyncError::Message)?,
        };
        let same = self
            .db
            .root()
            .ok()
            .flatten()
            .map(|(p, _, _)| p == root)
            .unwrap_or(false);
        if !same {
            validate_root_path(&root).map_err(NativeSyncError::Message)?;
        }
        self.platform.register(&root)?;
        self.platform.connect()?;
        self.restore_platform_mappings();
        self.db
            .set_root(&root, true, "connected")
            .map_err(NativeSyncError::Message)?;
        self.status()
    }
    /// 启动自动重连（方案 §7.3）：db 中 enabled=1 时恢复注册/连接并预热映射。
    /// 同步根目录不存在时进入 disconnected 并保留提示，不自动改挂到其他路径。
    pub fn reconnect_from_db(&mut self) -> Result<NativeSyncStatusView, NativeSyncError> {
        let Some((root, enabled, _)) = self.db.root().map_err(NativeSyncError::Message)? else {
            return self.status();
        };
        if !enabled {
            return self.status();
        }
        if !root.exists() {
            self.db
                .set_root(&root, false, "disconnected")
                .map_err(NativeSyncError::Message)?;
            return self.status();
        }
        self.platform.register(&root)?;
        self.platform.connect()?;
        if let Some(backend) = self.backend.clone() {
            self.platform.set_hydration_backend(backend);
        }
        self.restore_platform_mappings();
        self.db
            .set_root(&root, true, "connected")
            .map_err(NativeSyncError::Message)?;
        // 授权恢复 = 自动对账的恢复信号（ADR-111 §2.3：清退避/暂停，调度器自行复位）。
        self.db
            .clear_auto_failures()
            .map_err(NativeSyncError::Message)?;
        self.status()
    }
    fn restore_platform_mappings(&mut self) {
        self.platform.set_state_db(self.db.path());
        if let Ok(mappings) = self.db.all_mappings() {
            for (fs_id, remote_path, relative) in mappings {
                if let Ok(remote) = RemotePath::parse(&remote_path) {
                    self.platform.set_remote_mapping(&relative, remote, fs_id);
                }
            }
        }
    }
    pub fn disable(&mut self, unregister: bool) -> Result<NativeSyncStatusView, NativeSyncError> {
        if unregister {
            self.platform.unregister()?;
        } else {
            self.platform.disconnect()?;
        }
        if let Some((root, _, _)) = self.db.root().map_err(NativeSyncError::Message)? {
            self.db
                .set_root(&root, false, "disabled")
                .map_err(NativeSyncError::Message)?;
        }
        self.status()
    }
    pub async fn sync_now(&mut self) -> Result<NativeSyncStatusView, NativeSyncError> {
        // 每轮一个 run id：串联该轮全部相关日志，便于判定“一次运行 vs 两次运行”。
        static RUN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let run_id = RUN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        self.platform.set_run_id(run_id);
        let Some((root, enabled, _)) = self.db.root().map_err(NativeSyncError::Message)? else {
            return Err(NativeSyncError::Message("请先启用 NativeSync".into()));
        };
        if !enabled {
            return Err(NativeSyncError::Message("NativeSync 未启用".into()));
        }
        let backend = self
            .backend
            .as_ref()
            .ok_or_else(|| NativeSyncError::Message("请先完成百度网盘授权".into()))?
            .clone();
        self.platform.set_hydration_backend(backend.clone());
        self.platform.set_state_db(self.db.path());
        let mut queue = vec![RemotePath::root()];
        let mut seen = Vec::new();
        while let Some(remote) = queue.pop() {
            let entries = backend
                .list(&remote)
                .await
                .map_err(|e| NativeSyncError::Backend(e.to_string()))?;
            let outcome = self.ingest_dir_entries(&root, &remote, entries)?;
            seen.extend(outcome.seen);
            queue.extend(outcome.subdirs);
        }
        // 每轮对账一行 info 汇总：一眼看出"有没有重复转换"，便于与 fsutil 对照。
        let (checked, placeholders, converted, convert_failed) = self.platform.take_probe_summary();
        if checked > 0 {
            log::info!(
                "native-sync existing-path probe run={run_id} checked={checked} placeholders={placeholders} converted={converted} convert_failed={convert_failed}"
            );
        }
        let missing = self
            .db
            .missing_entries(&seen)
            .map_err(NativeSyncError::Message)?;
        self.apply_missing(&root, missing, &backend).await?;
        // 手动「立即对账」成功 = 自动对账的恢复信号（ADR-111 §2.3：清退避/暂停）。
        self.db
            .note_auto_success(false)
            .map_err(NativeSyncError::Message)?;
        self.status()
    }
    /// 消化一个目录的列举结果（全量 BFS 与增量自动层**同一条路径**，ADR-111 §2.4：
    /// 不许两套逻辑分叉）：upsert、占位符、in-sync 自愈、pin/水合对账、pending 守卫。
    fn ingest_dir_entries(
        &mut self,
        root: &Path,
        remote: &RemotePath,
        entries: Vec<ResourceSnapshot>,
    ) -> Result<DirIngestOutcome, NativeSyncError> {
        let mut outcome = DirIngestOutcome {
            seen: Vec::new(),
            subdirs: Vec::new(),
            changed_top_dirs: Vec::new(),
            added: 0,
            changed: 0,
        };
        for snapshot in entries {
            let fs_id = snapshot
                .fs_id
                .ok_or_else(|| NativeSyncError::Message("远端条目缺少 fs_id".into()))?;
            // 云端仍存在：先登记，否则下面的 missing 对账会把它误判成远端删除。
            outcome.seen.push(fs_id);
            // H1：本地已删除/改名、尚未同步到云端的条目不得被重建，
            // 否则用户删掉的文件会在下次同步无声复活、改名会多出旧路径副本。
            let mut preserve_baseline = false;
            if let Some((pending_fs_id, op)) = self
                .db
                .pending_op_for_remote(snapshot.path.as_str())
                .map_err(NativeSyncError::Message)?
            {
                if op == "delete" || op == "rename" {
                    log::info!(
                        "native-sync sync_now keeps local pending entry fs_id={pending_fs_id} op={op}"
                    );
                    continue;
                }
                if op == "upload" {
                    // H1 扩展（finding #3 防御）：pending 上传的条目**不得被 ingest
                    // 收编基线**——否则 worker 的 stat 比对永远落在收编之后：
                    // 基线被刷成云端新版 ⇒ 一致 ⇒ rtype=3「安全覆盖」变「静默吃掉
                    // 别人的云端版本」。保留最后一致版本，冲突才能被 stat 看见。
                    preserve_baseline = true;
                }
            }
            let name = sanitize_component(
                snapshot
                    .server_filename
                    .as_deref()
                    .or_else(|| snapshot.path.file_name())
                    .unwrap_or("unnamed"),
            );
            let parent_remote = remote.as_str().trim_start_matches('/');
            // 父目录在本轮枚举中必然已先入库（目录出队后才枚举其子项），
            // 此处回填 parent_fs_id，修复 has_children 恒 false 的根因；
            // upsert 的 ON CONFLICT 子句已会更新该列，老库随下次 sync_now 自愈。
            let parent_fs_id = if remote.is_root() {
                None
            } else {
                self.db
                    .fs_id_for_remote(parent_remote)
                    .map_err(NativeSyncError::Message)?
            };
            let parent_local = if remote.is_root() {
                String::new()
            } else {
                self.db
                    .relative_for_remote(parent_remote)
                    .map_err(NativeSyncError::Message)?
                    .unwrap_or_else(|| parent_remote.to_string())
            };
            let relative = if parent_local.is_empty() {
                name.clone()
            } else {
                format!("{parent_local}/{name}")
            };
            let local = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
            // 变化检测必须在 upsert 之前读基线（之后就是自证）。
            let new_modified = snapshot
                .modified
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|x| x.as_secs() as i64);
            let prior = self
                .db
                .remote_meta_by_fs_id(fs_id)
                .map_err(NativeSyncError::Message)?;
            let prior_absent = prior.is_none();
            let is_changed =
                remote_content_changed(prior, snapshot.size, &snapshot.md5, new_modified);
            let record = crate::model::EntryRecord {
                fs_id: fs_id as i64,
                local_only: false,
                parent_fs_id,
                remote_path: snapshot.path.as_str().to_string(),
                relative_path: relative.clone(),
                name,
                is_dir: snapshot.is_dir(),
                size: snapshot.size,
                md5: snapshot.md5.clone(),
                modified_at: new_modified,
                state: NativeSyncState::OnlineOnly,
                pin_state: PinState::Unpinned,
                error: None,
                pending_op: None,
                pending_since: None,
                has_children: snapshot.is_dir(),
            };
            if preserve_baseline {
                log::info!("native-sync ingest keeps baseline for pending upload fs_id={fs_id}");
            } else {
                self.db.upsert(&record).map_err(NativeSyncError::Message)?;
                if prior_absent {
                    outcome.added += 1;
                } else if is_changed {
                    outcome.changed += 1;
                }
            }
            self.platform
                .set_remote_mapping(&relative, snapshot.path.clone(), fs_id);
            self.platform.create_placeholder(
                &local,
                snapshot.is_dir(),
                snapshot.size,
                &fs_id.to_le_bytes(),
            )?;
            // 先算 in-sync 自愈的前置条件（**必须在写入新快照之前**，否则就是自证。
            let (pending_change, snapshot_matches) = if let Ok(meta) = std::fs::metadata(&local) {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                let pending = self
                    .db
                    .pending_op_of(fs_id as i64)
                    .map_err(NativeSyncError::Message)?
                    .is_some();
                let matches = matches!(
                    self.db
                        .snapshot_for(fs_id as i64)
                        .map_err(NativeSyncError::Message)?,
                    Some(snap) if snap.size == Some(meta.len()) && snap.mtime == mtime
                );
                // 记录本地快照作为基线：关闭完成通知靠它与磁盘现状比对来识别本地编辑。
                let _ = self.db.set_snapshot(fs_id as i64, mtime, meta.len());
                (pending, matches)
            } else {
                (true, false)
            };
            // 占位符被平台标成"未同步"且内容确实与云端一致时才恢复 in-sync。
            self.platform.repair_in_sync_if_needed(
                &local,
                fs_id,
                snapshot.is_dir(),
                pending_change,
                snapshot_matches,
            );
            // 回读文件系统上的真实 pin 状态：只跟自己的操作走会与磁盘不一致
            // （实测：磁盘 PINNED、数据库 unpinned，界面与 Explorer 互相矛盾）。
            if !snapshot.is_dir() {
                if let Some(pinned) = self.platform.pin_state(&local) {
                    let _ = self.db.set_pin_state(fs_id, pinned);
                }
                // 平台侧可能已经把文件水合回来或释放掉了：每轮用磁盘真值对账，
                // 免得管理台显示的状态与 Explorer 看到的实际情况互相矛盾。
                self.reconcile_hydration_with_disk(fs_id, &relative, &local, snapshot.size)?;
                // 甲方案守卫（收敛路径）：库里仍有未同步本地改动的条目，重新标回
                // NOT_IN_SYNC，让平台（及外部工具）**拒绝对它脱水**。捕获那一刻已标过一次，
                // 这里覆盖「老库行 / 重启后遗留」的情形。S1 无上传方向 ⇒ 该标记可能长期保持，
                // 这是诚实状态（S2 上传成功后再清回 IN_SYNC）。
                if self
                    .db
                    .pending_op_of(fs_id as i64)
                    .map_err(NativeSyncError::Message)?
                    .is_some()
                {
                    self.platform.mark_not_in_sync(&local);
                }
            }

            if snapshot.is_dir() {
                if remote.is_root() && is_changed {
                    outcome.changed_top_dirs.push(snapshot.path.clone());
                }
                outcome.subdirs.push(snapshot.path.clone());
            }
        }
        Ok(outcome)
    }

    /// 远端删除对账（全量=全树 missing，增量=单目录 scoped missing，同一处理逻辑）：
    /// hydrated/有水合后代 ⇒ 仅标 remote_missing；否则移除占位符并 tombstone。
    /// 返回 (tombstoned, preserved_remote_missing)。
    ///
    /// **陈旧 listing 守卫**（2026-10-13 实测：method=list 最终一致延迟 >24 分钟，
    /// 曾把刚上传 10 分钟的冲突副本误判 remote_missing 并物理删除）：list 管广度、
    /// **stat 管生杀**——
    /// ① 宽限：**我方刚上传**（uploaded_at < 30 分钟）的条目不进候选；
    /// ② 候选 > 50 ⇒ 不是滞后是事故，本层不做任何 missing 动作（防 API 打爆 + 防误删）；
    /// ③ 其余候选一次**批量 stat** 确认：路径存在 ⇒ 保留并记 stale listing 日志；
    ///    查无 ⇒ 才走 remote_missing/删除流程；确认调用本身失败 ⇒ 整层报错（不拿半信证据动刀）。
    async fn apply_missing(
        &mut self,
        root: &Path,
        missing: Vec<(u64, String, String, bool)>,
        backend: &Arc<dyn CloudBackend>,
    ) -> Result<(u64, u64), NativeSyncError> {
        const MISSING_GRACE_SECS: i64 = 30 * 60;
        const MISSING_CONFIRM_MAX: usize = 50;
        // ① 宽限。
        let mut candidates = Vec::new();
        for row in missing {
            if self
                .db
                .uploaded_recently(row.0 as i64, MISSING_GRACE_SECS)
                .map_err(NativeSyncError::Message)?
            {
                log::info!(
                    "native-sync missing grace fs_id={} (uploaded <30min ago)",
                    row.0
                );
                continue;
            }
            candidates.push(row);
        }
        // ② 异常闸门。
        if candidates.len() > MISSING_CONFIRM_MAX {
            log::warn!(
                "native-sync missing candidates={} exceeds {}: skip ALL actions this layer (anomaly, not lag)",
                candidates.len(),
                MISSING_CONFIRM_MAX
            );
            return Ok((0, 0));
        }
        // ③ 批量 stat 确认（一次 filemetas 调用）。
        let mut alive: std::collections::HashSet<u64> = std::collections::HashSet::new();
        if !candidates.is_empty() {
            let mut paths = Vec::with_capacity(candidates.len());
            for (fs_id, _, _, _) in &candidates {
                let remote = self
                    .db
                    .remote_path_by_fs_id(*fs_id as i64)
                    .map_err(NativeSyncError::Message)?
                    .ok_or_else(|| {
                        NativeSyncError::Message("missing 候选缺 remote_path".into())
                    })?;
                paths.push(
                    RemotePath::parse(&remote)
                        .map_err(|e| NativeSyncError::Message(format!("remote_path 非法: {e}")))?,
                );
            }
            let hits = backend
                .stats(&paths)
                .await
                .map_err(|e| NativeSyncError::Backend(e.to_string()))?;
            for snapshot in &hits {
                if let Some(fs_id) = snapshot.fs_id {
                    alive.insert(fs_id);
                }
            }
        }
        let mut tombstoned = 0u64;
        let mut preserved = 0u64;
        let mut stale_kept = 0u64;
        for (fs_id, relative, state, is_dir) in candidates {
            if alive.contains(&fs_id) {
                stale_kept += 1;
                continue;
            }

            let local = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
            let preserve = state == "hydrated"
                || (is_dir
                    && self
                        .db
                        .has_hydrated_descendant(&relative)
                        .map_err(NativeSyncError::Message)?);
            if preserve {
                self.db
                    .set_state_by_fs_id(
                        fs_id,
                        "remote_missing",
                        Some("远端条目已删除，保留本地内容"),
                    )
                    .map_err(NativeSyncError::Message)?;
                preserved += 1;
            } else if local.exists() {
                self.platform.remove_placeholder(&local, is_dir)?;
                self.db.tombstone(fs_id).map_err(NativeSyncError::Message)?;
                tombstoned += 1;
            } else {
                // 本地已不存在（用户在 Explorer 删除、或父目录本轮被跳过枚举）：
                // 直接记账，不再调用平台删除接口（对不存在的路径会 ENOENT 而中断整轮）。
                self.db.tombstone(fs_id).map_err(NativeSyncError::Message)?;
                tombstoned += 1;
            }
        }
        if stale_kept > 0 {
            log::info!(
                "native-sync stale listing observed: {stale_kept} entries kept alive by stat confirm"
            );
        }
        Ok((tombstoned, preserved))
    }

    /// 调度器记账：reconcile 未执行到的失败（如 backend 缺失）由运行循环代记。
    /// reconcile 内部的层失败已自行记账，**不得**再经这里重复计。
    pub fn note_auto_failure(
        &mut self,
        message: &str,
        pause_threshold: u32,
    ) -> Result<u32, NativeSyncError> {
        self.db
            .note_auto_failure(message, pause_threshold)
            .map_err(NativeSyncError::Message)
    }
    /// 大树降级的间隔覆盖秒数（ADR-111 §2.7：D>500 ⇒ 15m）；无覆盖 ⇒ None。
    pub fn auto_interval_override_secs(&self) -> Option<u64> {
        self.db
            .auto_state_get("interval_override_secs")
            .ok()
            .flatten()
            .and_then(|value| value.parse().ok())
    }
    /// 退避截止时间由调度器（src-tauri 运行循环）计算后回写，供 status/UI 展示。
    pub fn set_backoff_until(&mut self, until: Option<String>) -> Result<(), NativeSyncError> {
        self.db
            .auto_state_set("backoff_until", until.as_deref().unwrap_or(""))
            .map_err(NativeSyncError::Message)
    }

    /// 自动对账一层（增量，ADR-111 §2.2）：根目录快查 + 定向下钻 ≤ drilldown_max +
    /// 交错深扫 ≤ deep_scan_batch，全程受 layer_budget 调用预算约束。
    ///
    /// 与手动「立即对账」**同一条引擎路径**（`ingest_dir_entries`/`apply_missing`），
    /// 差别只在范围：手动 = 全树 BFS + 全树 missing；自动 = 有界的每层子集 + scoped missing。
    /// 被手动抢占（`cancel` 置位）⇒ 在目录列举边界停止，`preempted=true`，**不算失败**。
    pub async fn reconcile_incremental(
        &mut self,
        params: &AutoSyncParams,
        cancel: &Arc<AtomicBool>,
    ) -> Result<AutoLayerReport, NativeSyncError> {
        static AUTO_RUN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let run_id = AUTO_RUN_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        self.platform.set_run_id(run_id);
        let mut report = AutoLayerReport::default();
        let Some((root, enabled, _)) = self.db.root().map_err(NativeSyncError::Message)? else {
            report.skipped = Some("no_root");
            return Ok(report);
        };
        if !enabled {
            report.skipped = Some("disabled");
            return Ok(report);
        }
        let (_, paused, ..) = self.db.auto_counters().map_err(NativeSyncError::Message)?;
        if paused {
            report.skipped = Some("paused");
            return Ok(report);
        }
        let Some(backend) = self.backend.clone() else {
            return Err(NativeSyncError::Message("请先完成百度网盘授权".into()));
        };
        self.platform.set_hydration_backend(backend.clone());
        self.platform.set_state_db(self.db.path());
        // ADR-111 §2.7 大树降级：目录总数 > 500 ⇒ 深扫批量减半、间隔拉长（运行循环读
        // `interval_override_secs`；小树时清除覆盖）。
        let dir_total = self.db.count_dirs().map_err(NativeSyncError::Message)?;
        let mut effective = params.clone();
        if dir_total > 500 {
            effective.deep_scan_batch = (params.deep_scan_batch / 2).max(1);
            self.db
                .auto_state_set("interval_override_secs", "900")
                .map_err(NativeSyncError::Message)?;
        } else {
            self.db
                .auto_state_set("interval_override_secs", "")
                .map_err(NativeSyncError::Message)?;
        }
        let outcome = self
            .run_auto_layer(&root, &backend, &effective, cancel, &mut report)
            .await;
        match &outcome {
            Ok(()) if report.preempted => {
                log::info!(
                    "native-sync auto layer run={run_id} preempted calls={} added={} changed={} removed={}",
                    report.calls, report.added, report.changed, report.removed
                );
            }
            Ok(()) => {
                self.db
                    .note_auto_success(true)
                    .map_err(NativeSyncError::Message)?;
                log::info!(
                    "native-sync auto layer run={run_id} ok calls={} added={} changed={} removed={} missing_marked={} drilled={} deep={} deferred={} dirs={dir_total} local_new={} local_edited={}",
                    report.calls, report.added, report.changed, report.removed,
                    report.remote_missing_marked, report.drilled, report.deep_scanned, report.deferred,
                    report.local_new, report.local_edited
                );
            }
            Err(error) => {
                let fails = self
                    .db
                    .note_auto_failure(&error.to_string(), params.pause_threshold)
                    .map_err(NativeSyncError::Message)?;
                report.consecutive_failures = fails;
                log::warn!(
                    "native-sync auto layer run={run_id} failed consecutive={fails} calls={} error={error}",
                    report.calls
                );
            }
        }
        outcome.map(|_| report)
    }

    /// 一层的三段执行（根快查 → 定向下钻 → 交错深扫）；失败即中断，剩余目录自然推迟。
    async fn run_auto_layer(
        &mut self,
        root: &Path,
        backend: &Arc<dyn CloudBackend>,
        params: &AutoSyncParams,
        cancel: &Arc<AtomicBool>,
        report: &mut AutoLayerReport,
    ) -> Result<(), NativeSyncError> {
        // 1) 根目录快查：顶层新增/删除/改名/内容变化在 1 层内被发现；翻页折算计入预算。
        let root_remote = RemotePath::root();
        let entries = backend
            .list(&root_remote)
            .await
            .map_err(|e| NativeSyncError::Backend(e.to_string()))?;
        report.calls += list_call_cost(entries.len());
        // 本地侧对账（Gap A/B/C）**必须先于 ingest**：scan 的语义是「磁盘 vs 本层之前
        // 的基线」——ingest 会无条件 set_snapshot 把基线刷成磁盘现状，先收编就会把
        // 本地编辑的证据当场擦除（2026-10-13 真实机实证：先 ingest 时 local_edited 恒 0）。
        self.scan_local_dir(root, "", report)?;
        let outcome = self.ingest_dir_entries(root, &root_remote, entries)?;
        report.added += outcome.added;
        report.changed += outcome.changed;
        let (removed, marked) = self.scoped_missing(root, "", &outcome.seen, backend).await?;
        report.removed += removed;
        report.remote_missing_marked += marked;
        // 本层已覆盖的目录（定向下钻/深扫），游标走位时跳过不重列。
        let mut covered: HashSet<String> = HashSet::new();

        // 2) 定向下钻：根查发现的可疑顶层目录（新增或元数据变化），≤ drilldown_max。
        for dir in &outcome.changed_top_dirs {
            if report.drilled >= params.drilldown_max || report.calls >= params.layer_budget {
                report.deferred += 1;
                continue;
            }
            if cancel.load(Ordering::Relaxed) {
                report.preempted = true;
                return Ok(());
            }
            let entries = backend
                .list(dir)
                .await
                .map_err(|e| NativeSyncError::Backend(e.to_string()))?;
            report.calls += list_call_cost(entries.len());
            report.drilled += 1;
            covered.insert(dir.as_str().to_string());
            let rel = self.relative_of(dir);
            self.scan_local_dir(root, &rel, report)?;
            let sub = self.ingest_dir_entries(root, dir, entries)?;
            report.added += sub.added;
            report.changed += sub.changed;
            let (removed, marked) = self.scoped_missing(root, &rel, &sub.seen, backend).await?;
            report.removed += removed;
            report.remote_missing_marked += marked;
        }

        // 3) 交错深扫：游标按 remote_path 字典序轮转，把全量目录覆盖摊到 ceil(D/K) 层。
        let mut cursor = self
            .db
            .auto_state_get("deep_scan_cursor")
            .map_err(NativeSyncError::Message)?
            .filter(|value| !value.is_empty());
        let dir_total = self.db.count_dirs().map_err(NativeSyncError::Message)?;
        let mut walked = 0u64;
        // 游标走位是本地索引查询（免费），但必须有界：覆盖集 + 全圈检测防死循环。
        let walk_cap = dir_total + params.drilldown_max as u64 + 2;
        while report.deep_scanned < params.deep_scan_batch && report.calls < params.layer_budget {
            if cancel.load(Ordering::Relaxed) {
                report.preempted = true;
                break;
            }
            if walked >= walk_cap {
                break;
            }
            walked += 1;
            let next = self
                .db
                .dirs_after(cursor.as_deref(), 1)
                .map_err(NativeSyncError::Message)?
                .into_iter()
                .next();
            let dir = match next {
                Some(dir) => dir,
                None => {
                    // 一轮覆盖完成：回到开头；若开头即本层已覆盖 ⇒ 整圈走完。
                    match self
                        .db
                        .dirs_after(None, 1)
                        .map_err(NativeSyncError::Message)?
                        .into_iter()
                        .next()
                    {
                        Some(dir) if !covered.contains(&dir) => dir,
                        _ => break,
                    }
                }
            };
            cursor = Some(dir.clone());
            if covered.contains(&dir) {
                continue;
            }
            let remote = RemotePath::parse(&dir)
                .map_err(|e| NativeSyncError::Message(format!("索引目录路径非法: {e}")))?;
            let entries = backend
                .list(&remote)
                .await
                .map_err(|e| NativeSyncError::Backend(e.to_string()))?;
            report.calls += list_call_cost(entries.len());
            report.deep_scanned += 1;
            covered.insert(dir.clone());
            let rel = self.relative_of(&remote);
            self.scan_local_dir(root, &rel, report)?;
            let sub = self.ingest_dir_entries(root, &remote, entries)?;
            report.added += sub.added;
            report.changed += sub.changed;
            let (removed, marked) = self.scoped_missing(root, &rel, &sub.seen, backend).await?;
            report.removed += removed;
            report.remote_missing_marked += marked;
        }
        if let Some(cursor) = cursor {
            self.db
                .auto_state_set("deep_scan_cursor", &cursor)
                .map_err(NativeSyncError::Message)?;
        }
        Ok(())
    }

    /// 目录 remote → 本地相对路径（scoped missing 的参数）；索引缺失时退化为路径本身。
    fn relative_of(&self, remote: &RemotePath) -> String {
        let trimmed = remote.as_str().trim_start_matches('/');
        self.db
            .relative_for_remote(trimmed)
            .ok()
            .flatten()
            .unwrap_or_else(|| trimmed.to_string())
    }

    /// 单目录 scoped 远端删除对账：只处理该目录的**直接子项**。
    /// 增量层绝不能按全树判 missing——未扫到的目录会被误判成远端删除。
    async fn scoped_missing(
        &mut self,
        root: &Path,
        parent_relative: &str,
        seen: &[u64],
        backend: &Arc<dyn CloudBackend>,
    ) -> Result<(u64, u64), NativeSyncError> {
        let missing = self
            .db
            .missing_children_of(parent_relative, seen)
            .map_err(NativeSyncError::Message)?;
        self.apply_missing(root, missing, backend).await
    }

    /// S2 上传方向（ADR-112）：传播一轮待同步变更（本地 → 云端）。
    ///
    /// 与自动对账共用 src-tauri 的 engine 互斥；文件间检查 `cancel`（手动对账抢占 ⇒
    /// `interrupted=true` 让位，不算失败）。失败纪律：硬错误（配额/权限/参数/冲突逃逸）只影响
    /// 该条目、进「需处理」；可重试错误计次退避（≥5 次进「需处理」）并中断本层（不烧配额）。
    pub async fn drain_upload_queue(
        &mut self,
        params: &UploadParams,
        cancel: &Arc<AtomicBool>,
    ) -> Result<UploadDrainReport, NativeSyncError> {
        let mut report = UploadDrainReport::default();
        let Some((root, enabled, _)) = self.db.root().map_err(NativeSyncError::Message)? else {
            return Ok(report);
        };
        if !enabled {
            return Ok(report);
        }
        let Some(backend) = self.backend.clone() else {
            return Err(NativeSyncError::Message("请先完成百度网盘授权".into()));
        };
        let queue = self.db.upload_queue().map_err(NativeSyncError::Message)?;
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        for (fs_id, relative, op, is_dir, local_only, remote_path, pending_since) in queue {
            if cancel.load(Ordering::Relaxed) {
                report.interrupted = true;
                break;
            }
            // 防抖（ADR-112 §3.6：3s）：窗口内的条目留给下一次 drain（编辑器连续保存合并）。
            if let Some(since) = pending_since
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok())
            {
                if now_secs.saturating_sub(since) < params.debounce.as_secs() {
                    report.deferred += 1;
                    continue;
                }
            }
            let outcome = match op.as_str() {
                "upload" => {
                    self.exec_upload(
                        &root,
                        &backend,
                        params,
                        fs_id,
                        &relative,
                        is_dir,
                        local_only,
                        &remote_path,
                    )
                    .await
                }
                "delete" => {
                    self.exec_delete(&backend, params, fs_id, local_only, &remote_path)
                        .await
                }
                "rename" => {
                    self.exec_rename_propagate(&backend, params, fs_id, &relative, &remote_path)
                        .await
                }
                _ => Ok(UploadTaskEffect::Skipped),
            };
            match outcome {
                Ok(UploadTaskEffect::Uploaded) => report.uploaded += 1,
                Ok(UploadTaskEffect::DirCreated) => report.dirs_created += 1,
                Ok(UploadTaskEffect::Conflicted) => report.conflicts += 1,
                Ok(UploadTaskEffect::Deleted) => report.deleted += 1,
                Ok(UploadTaskEffect::Renamed) => report.renamed += 1,
                Ok(UploadTaskEffect::Skipped) => report.skipped += 1,
                Err(error) => {
                    let retryable = matches!(error, UploadTaskError::Retryable(_));
                    report.failed += 1;
                    let message = error.into_message();
                    log::warn!(
                        "native-sync upload task failed fs_id={fs_id} op={op} retryable={retryable} error={message}"
                    );
                    if report.first_error.is_none() {
                        report.first_error = Some(message);
                    }
                    if retryable {
                        // 可重试错误（疑似网络/限流）：本层到此为止，退避交给调度层——
                        // 继续打只会把整队文件都变成失败计次并烧配额。
                        break;
                    }
                }
            }
        }
        if report.uploaded
            + report.dirs_created
            + report.conflicts
            + report.deleted
            + report.renamed
            + report.failed
            + report.skipped
            + report.deferred
            > 0
        {
            log::info!(
                "native-sync upload drain uploaded={} dirs={} conflicts={} deleted={} renamed={} skipped={} deferred={} failed={} interrupted={}",
                report.uploaded, report.dirs_created, report.conflicts, report.deleted,
                report.renamed, report.skipped, report.deferred, report.failed, report.interrupted
            );
        }
        Ok(report)
    }

    /// 单条 upload 任务：本地新建（local_only）或云端条目的本地编辑。
    #[allow(clippy::too_many_arguments)]
    async fn exec_upload(
        &mut self,
        root: &Path,
        backend: &Arc<dyn CloudBackend>,
        params: &UploadParams,
        fs_id: i64,
        relative: &str,
        is_dir: bool,
        local_only: bool,
        remote_path: &str,
    ) -> Result<UploadTaskEffect, UploadTaskError> {
        let local = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        let meta = match std::fs::metadata(&local) {
            Ok(meta) => meta,
            Err(_) => {
                // 本地已消失：本地新建从未上云 ⇒ 清索引；云端条目 ⇒ 删除是否传播由
                // delete pending 另行裁决，这里只清 upload 标记防卡死（无证据不传播）。
                let _ = self.db.set_pending_op(fs_id, None, None);
                if local_only {
                    let _ = self.db.tombstone_i64(fs_id);
                }
                log::info!("native-sync upload skipped: local file gone fs_id={fs_id}");
                return Ok(UploadTaskEffect::Skipped);
            }
        };
        let remote =
            remote_for_upload(relative, remote_path, local_only).map_err(UploadTaskError::Hard)?;
        if is_dir {
            let created = match backend.create_dir(&remote).await {
                Ok(()) => true,
                // 云端已存在（-8）⇒ 视为成功（目录语义幂等）。
                Err(error) if upload::is_conflict_error(&error) => false,
                Err(error) => return Err(self.fail_entry(fs_id, &error, params)),
            };
            if local_only {
                // create_collection 不回 fs_id：stat 一次拿真身（仅新建目录路径，+1 次调用）。
                let snap = backend
                    .stat(&remote)
                    .await
                    .map_err(|e| self.fail_entry(fs_id, &e, params))?;
                let Some(real_id) = snap.fs_id else {
                    return Err(UploadTaskError::Retryable(
                        "目录上传后 stat 缺少 fs_id".into(),
                    ));
                };
                let record = self.uploaded_record(
                    real_id,
                    relative,
                    &remote,
                    true,
                    snap.size,
                    snap.md5.clone(),
                    epoch_of(snap.modified),
                )?;
                self.db
                    .rekey_local_only(fs_id, &record)
                    .map_err(UploadTaskError::Hard)?;
            }
            self.db
                .set_pending_op(fs_id, None, None)
                .map_err(UploadTaskError::Hard)?;
            let _ = self.db.mark_local_changes_applied(fs_id);
            let _ = self.db.clear_error(fs_id);
            let _ = self.platform.mark_in_sync(&local);
            if let Some(real) = self.db.entry_by_relative(relative).ok().flatten() {
                self.platform
                    .set_remote_mapping(relative, remote.clone(), real.0 as u64);
            }
            log::info!("native-sync upload dir done fs_id={fs_id} created={created}");
            return Ok(if created {
                UploadTaskEffect::DirCreated
            } else {
                UploadTaskEffect::Uploaded
            });
        }
        let size = meta.len();
        if local_only {
            // local_only 新建：rtype=0（撞名即失败——-8 是撞名的唯二信息源）。
            match backend.upload(&remote, &local, size, 0).await {
                Ok(outcome) => {
                    let real_id = outcome
                        .fs_id
                        .ok_or_else(|| UploadTaskError::Retryable("上传回执缺少 fs_id".into()))?;
                    self.finish_upload_success(
                        fs_id,
                        real_id,
                        relative,
                        &remote,
                        &local,
                        &meta,
                        true,
                        false,
                        outcome.size,
                        outcome.md5.clone(),
                        outcome.mtime,
                    )?;
                    log::info!("native-sync upload done fs_id={real_id} size={size} rtype=0");
                    Ok(UploadTaskEffect::Uploaded)
                }
                Err(error) if upload::is_conflict_error(&error) => {
                    // 撞名未必撞内容：stat 比 md5（仅限此罕见分支，+1 次调用）——
                    // 一致 ⇒ 双方已一致，直接回写基线，不走冲突流程（ADR-112 §3.3 修订）。
                    let local_md5 =
                        upload::local_md5(&local).map_err(UploadTaskError::Retryable)?;
                    let snap = backend
                        .stat(&remote)
                        .await
                        .map_err(|e| self.fail_entry(fs_id, &e, params))?;
                    if snap.md5.as_deref() == Some(local_md5.as_str()) {
                        let real_id = snap
                            .fs_id
                            .ok_or_else(|| UploadTaskError::Retryable("stat 缺少 fs_id".into()))?;
                        self.finish_upload_success(
                            fs_id,
                            real_id,
                            relative,
                            &remote,
                            &local,
                            &meta,
                            true,
                            false,
                            snap.size,
                            snap.md5.clone(),
                            epoch_of(snap.modified),
                        )?;
                        log::info!(
                            "native-sync upload already consistent after -8 fs_id={real_id}"
                        );
                        return Ok(UploadTaskEffect::Uploaded);
                    }
                    self.exec_conflict(root, backend, params, fs_id, relative, &local, true, size)
                        .await
                }
                Err(error) => Err(self.fail_entry(fs_id, &error, params)),
            }
        } else {
            // 有基线的编辑（ADR-112 §3.3 修订：rtype 分流——stat 比对即冲突检测主路径）。
            // 云端 md5 == 基线 md5 ⇒ 我们知道自己覆盖的是哪个版本 ⇒ rtype=3 安全覆盖；
            // 不一致 ⇒ 云端真被别人改了 ⇒ 直接进冲突流程（上传反而可能覆盖他人版本）。
            let baseline_md5 = self
                .db
                .remote_meta_by_fs_id(fs_id as u64)
                .map_err(UploadTaskError::Hard)?
                .and_then(|(_, md5, _)| md5);
            let snap = backend
                .stat(&remote)
                .await
                .map_err(|e| self.fail_entry(fs_id, &e, params))?;
            let cloud_changed = match (&baseline_md5, &snap.md5) {
                (Some(base), Some(current)) => base != current,
                // 缺 md5 无法判定 ⇒ 保守视为未变（编辑的本来就是索引里我们自己的文件）。
                _ => false,
            };
            if cloud_changed {
                log::info!(
                    "native-sync conflict detected by stat fs_id={fs_id}: cloud md5 moved past baseline"
                );
                return self
                    .exec_conflict(root, backend, params, fs_id, relative, &local, false, size)
                    .await;
            }
            match backend.upload(&remote, &local, size, 3).await {
                Ok(outcome) => {
                    let real_id = outcome
                        .fs_id
                        .ok_or_else(|| UploadTaskError::Retryable("上传回执缺少 fs_id".into()))?;
                    self.finish_upload_success(
                        fs_id,
                        real_id,
                        relative,
                        &remote,
                        &local,
                        &meta,
                        false,
                        false,
                        outcome.size,
                        outcome.md5.clone(),
                        outcome.mtime,
                    )?;
                    log::info!("native-sync upload done fs_id={real_id} size={size} rtype=3");
                    Ok(UploadTaskEffect::Uploaded)
                }
                Err(error) if upload::is_conflict_error(&error) => {
                    // -8 兜底：stat 之后云端又被并发改了（真实竞态）⇒ 进冲突流程。
                    self.exec_conflict(root, backend, params, fs_id, relative, &local, false, size)
                        .await
                }
                Err(error) => Err(self.fail_entry(fs_id, &error, params)),
            }
        }
    }

    /// 上传成功（或秒传守卫判定双方已一致）的回声抑制基线回写：
    /// create/stat 的 (fs_id, md5, size, mtime) 立即入索引 + 本地快照 + 清 pending——
    /// 下一轮 ADR-111 对账必须判「一致」（ADR-112 §3.5 核心）。
    #[allow(clippy::too_many_arguments)]
    fn finish_upload_success(
        &mut self,
        pending_fs_id: i64,
        real_fs_id: u64,
        relative: &str,
        remote: &RemotePath,
        local: &Path,
        meta: &std::fs::Metadata,
        local_only: bool,
        fresh_insert: bool,
        size: u64,
        md5: Option<String>,
        mtime: Option<i64>,
    ) -> Result<(), UploadTaskError> {
        let record = self.uploaded_record(real_fs_id, relative, remote, false, size, md5, mtime)?;
        if local_only {
            self.db
                .rekey_local_only(pending_fs_id, &record)
                .map_err(UploadTaskError::Hard)?;
        } else if fresh_insert {
            // 全新行（冲突副本）：本地是完整内容 ⇒ placeholder=0、hydrated。
            self.db
                .insert_uploaded_entry(&record)
                .map_err(UploadTaskError::Hard)?;
        } else {
            self.db.upsert(&record).map_err(UploadTaskError::Hard)?;
        }
        // 陈旧 listing 守卫的宽限来源：这是「我方刚上传」的条目（ingest 的 upsert 不写此列）。
        let _ = self.db.mark_uploaded(real_fs_id as i64);
        let local_mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        let _ = self
            .db
            .set_snapshot(real_fs_id as i64, local_mtime, meta.len());
        self.db
            .set_pending_op(pending_fs_id, None, None)
            .map_err(UploadTaskError::Hard)?;
        let _ = self.db.mark_local_changes_applied(pending_fs_id);
        let _ = self.db.clear_error(real_fs_id as i64);
        let _ = self.platform.mark_in_sync(local);
        self.platform
            .set_remote_mapping(relative, remote.clone(), real_fs_id);
        Ok(())
    }

    /// 冲突（ADR-112 §3.3，OneDrive 口径）：云端版本占原名不动；
    /// 本地副本改名 `<原名>-<设备名>` 保留并回传；原名条目进「需处理」。
    /// 改名循环终止规则（总监终审补）：副本上传再撞 -8 只加序号重试**一次**，
    /// 仍撞则副本留本地、进「需处理」、**不再自动重传、不递归冲突流程**。
    #[allow(clippy::too_many_arguments)]
    async fn exec_conflict(
        &mut self,
        root: &Path,
        backend: &Arc<dyn CloudBackend>,
        params: &UploadParams,
        fs_id: i64,
        relative: &str,
        local: &Path,
        _local_only: bool,
        size: u64,
    ) -> Result<UploadTaskEffect, UploadTaskError> {
        let device = upload::device_name();
        let name = relative.rsplit('/').next().unwrap_or(relative);
        let dir_rel = relative.rsplit_once('/').map(|(dir, _)| dir);
        let join_rel = |file_name: &str| match dir_rel {
            Some(dir) => format!("{dir}/{file_name}"),
            None => file_name.to_string(),
        };
        // 1) 副本名：本地磁盘与索引都不许撞。
        let mut pick = 1u32;
        let (mut copy_rel, mut copy_local) = loop {
            let candidate_rel = join_rel(&upload::conflict_copy_name(name, &device, pick));
            let candidate_local =
                root.join(candidate_rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            let index_hit = self
                .db
                .entry_by_relative(&candidate_rel)
                .map_err(UploadTaskError::Hard)?
                .is_some();
            if !candidate_local.exists() && !index_hit {
                break (candidate_rel, candidate_local);
            }
            pick += 1;
            if pick > 99 {
                return Err(UploadTaskError::Hard("冲突副本命名空间耗尽".into()));
            }
        };
        // 2) 自杀回调守卫（RENAME 记源路径、RENAME_COMPLETION 记目标路径——两个都要登记）+ 本地改名。
        self.platform
            .note_self_path_ops(&[local.to_path_buf(), copy_local.clone()]);
        std::fs::rename(local, &copy_local)
            .map_err(|e| UploadTaskError::Hard(format!("冲突副本改名失败: {e}")))?;
        log::info!(
            "native-sync conflict fs_id={fs_id}: cloud version keeps the name, local copy renamed with device suffix"
        );
        // 3) 原名条目：进「需处理」、清 pending；云端版本不动（下一轮自动对账把原名带回来）。
        //    local_only（负数 fs_id）是本地新建撞名：原路径行**必须 tombstone**——
        //    文件已改名离开原路径，留着它 ⇒ 下一轮对账把云端版本落到同一 relative，
        //    同路径两行（幽灵行 + 云端真身），幽灵行永不再被任何流程触碰。
        if fs_id > 0 {
            let _ = self.db.set_state_by_fs_id(
                fs_id as u64,
                "error",
                Some("与云端版本冲突：云端版本保留原名，本地副本已改名保留"),
            );
        } else {
            self.db
                .tombstone_i64(fs_id)
                .map_err(UploadTaskError::Hard)?;
        }
        self.db
            .set_pending_op(fs_id, None, None)
            .map_err(UploadTaskError::Hard)?;
        let _ = self.db.mark_local_changes_applied(fs_id);
        // 4) 副本上传（终止规则：再撞 -8 加序号只重试一次）。
        for upload_attempt in 1..=2u32 {
            let copy_remote = RemotePath::parse(&format!("/{copy_rel}"))
                .map_err(|e| UploadTaskError::Hard(format!("副本远端路径非法: {e}")))?;
            match backend.upload(&copy_remote, &copy_local, size, 0).await {
                Ok(outcome) => {
                    let real_id = outcome.fs_id.ok_or_else(|| {
                        UploadTaskError::Retryable("副本上传回执缺少 fs_id".into())
                    })?;
                    let meta = std::fs::metadata(&copy_local)
                        .map_err(|e| UploadTaskError::Retryable(e.to_string()))?;
                    self.finish_upload_success(
                        real_id as i64,
                        real_id,
                        &copy_rel,
                        &copy_remote,
                        &copy_local,
                        &meta,
                        false,
                        true,
                        outcome.size,
                        outcome.md5.clone(),
                        outcome.mtime,
                    )?;
                    return Ok(UploadTaskEffect::Conflicted);
                }
                Err(error) if upload::is_conflict_error(&error) && upload_attempt == 1 => {
                    let new_rel = join_rel(&upload::conflict_copy_name(name, &device, 2));
                    let new_local = root.join(new_rel.replace('/', std::path::MAIN_SEPARATOR_STR));
                    self.platform
                        .note_self_path_ops(&[copy_local.clone(), new_local.clone()]);
                    std::fs::rename(&copy_local, &new_local)
                        .map_err(|e| UploadTaskError::Hard(format!("冲突副本再改名失败: {e}")))?;
                    copy_rel = new_rel;
                    copy_local = new_local;
                }
                Err(error) if upload::is_conflict_error(&error) => {
                    if fs_id > 0 {
                        let _ = self.db.set_state_by_fs_id(
                            fs_id as u64,
                            "error",
                            Some("冲突副本云端同名，已保留在本地，未自动重传"),
                        );
                    }
                    log::warn!(
                        "native-sync conflict copy collided twice fs_id={fs_id}: kept local, NOT re-uploaded"
                    );
                    return Ok(UploadTaskEffect::Conflicted);
                }
                Err(error) => return Err(self.fail_entry(fs_id, &error, params)),
            }
        }
        unreachable!("副本上传至多两次")
    }

    /// 删除传播（ADR-112 §3.4）：**仅当索引有基线**（云端已知条目）才执行——
    /// 「索引缺失」与「用户删除」必须区分（护栏，防索引损坏放大成云端批量删除）。
    async fn exec_delete(
        &mut self,
        backend: &Arc<dyn CloudBackend>,
        params: &UploadParams,
        fs_id: i64,
        local_only: bool,
        remote_path: &str,
    ) -> Result<UploadTaskEffect, UploadTaskError> {
        let has_baseline = !local_only
            && !remote_path.starts_with("local:")
            && self
                .db
                .remote_meta_by_fs_id(fs_id as u64)
                .map_err(UploadTaskError::Hard)?
                .is_some();
        if !has_baseline {
            log::info!("native-sync upload delete blocked: no baseline fs_id={fs_id}");
            self.db
                .set_pending_op(fs_id, None, None)
                .map_err(UploadTaskError::Hard)?;
            if local_only {
                let _ = self.db.tombstone_i64(fs_id);
            }
            return Ok(UploadTaskEffect::Skipped);
        }
        let remote = RemotePath::parse(remote_path)
            .map_err(|e| UploadTaskError::Hard(format!("远端路径非法: {e}")))?;
        backend
            .remove(&remote)
            .await
            .map_err(|e| self.fail_entry(fs_id, &e, params))?;
        self.db
            .tombstone_i64(fs_id)
            .map_err(UploadTaskError::Hard)?;
        self.db
            .set_pending_op(fs_id, None, None)
            .map_err(UploadTaskError::Hard)?;
        let _ = self.db.mark_local_changes_applied(fs_id);
        log::info!("native-sync delete propagated fs_id={fs_id}");
        Ok(UploadTaskEffect::Deleted)
    }

    /// 改名传播：opera=move 保 fs_id（**不走 delete+upload**），成功后回写云端事实。
    async fn exec_rename_propagate(
        &mut self,
        backend: &Arc<dyn CloudBackend>,
        params: &UploadParams,
        fs_id: i64,
        relative: &str,
        remote_path: &str,
    ) -> Result<UploadTaskEffect, UploadTaskError> {
        // local_state.relative_path 捕获时已改写为新名；remote_path 仍是旧云端路径。
        let from = RemotePath::parse(remote_path)
            .map_err(|e| UploadTaskError::Hard(format!("远端路径非法: {e}")))?;
        let to = RemotePath::parse(&format!("/{relative}"))
            .map_err(|e| UploadTaskError::Hard(format!("远端路径非法: {e}")))?;
        backend
            .rename(&from, &to)
            .await
            .map_err(|e| self.fail_entry(fs_id, &e, params))?;
        let new_name = relative.rsplit('/').next().unwrap_or(relative);
        self.db
            .apply_remote_rename(fs_id, to.as_str(), new_name)
            .map_err(UploadTaskError::Hard)?;
        self.db
            .set_pending_op(fs_id, None, None)
            .map_err(UploadTaskError::Hard)?;
        let _ = self.db.mark_local_changes_applied(fs_id);
        self.platform
            .set_remote_mapping(relative, to.clone(), fs_id as u64);
        log::info!("native-sync rename propagated fs_id={fs_id}");
        Ok(UploadTaskEffect::Renamed)
    }

    /// 失败分类（ADR-112 §3.6）：硬错误 ⇒ 条目进「需处理」并清 pending（不盲重）；
    /// 其余计次退避，≥ 阈值（5 次）同样进「需处理」。
    fn fail_entry(
        &mut self,
        fs_id: i64,
        error: &CloudError,
        params: &UploadParams,
    ) -> UploadTaskError {
        if upload::is_hard_error(error) {
            if fs_id > 0 {
                let _ =
                    self.db
                        .set_state_by_fs_id(fs_id as u64, "error", Some(error.message.as_str()));
            }
            let _ = self.db.set_pending_op(fs_id, None, None);
            UploadTaskError::Hard(error.message.clone())
        } else {
            let count = self.db.bump_error_count(fs_id).unwrap_or(1);
            if count >= params.error_threshold {
                if fs_id > 0 {
                    let _ = self.db.set_state_by_fs_id(
                        fs_id as u64,
                        "error",
                        Some(error.message.as_str()),
                    );
                }
                // 阈值清理对所有条目生效（local_only 负数 fs_id 也在内）——否则无限重试。
                let _ = self.db.set_pending_op(fs_id, None, None);
            }
            UploadTaskError::Retryable(error.message.clone())
        }
    }

    /// 构造上传成功后的索引记录（rekey 或 insert/upsert 共用）。
    #[allow(clippy::too_many_arguments)]
    fn uploaded_record(
        &self,
        fs_id: u64,
        relative: &str,
        remote: &RemotePath,
        is_dir: bool,
        size: u64,
        md5: Option<String>,
        mtime: Option<i64>,
    ) -> Result<EntryRecord, UploadTaskError> {
        let parent_fs_id = match relative.rsplit_once('/') {
            Some((dir, _)) => self
                .db
                .entry_by_relative(dir)
                .map_err(UploadTaskError::Hard)?
                .map(|row| row.0 as u64),
            None => None,
        };
        Ok(EntryRecord {
            fs_id: fs_id as i64,
            local_only: false,
            parent_fs_id,
            remote_path: remote.as_str().to_string(),
            relative_path: relative.to_string(),
            name: relative.rsplit('/').next().unwrap_or(relative).to_string(),
            is_dir,
            size,
            md5,
            modified_at: mtime,
            state: NativeSyncState::Hydrated,
            pin_state: PinState::Unpinned,
            error: None,
            pending_op: None,
            pending_since: None,
            has_children: is_dir,
        })
    }

    /// 本地侧对账（Gap A/B/C 捕获，ADR-112 §3.1 修订）：
    /// CF 通知是占位符作用域——新建普通文件无 CREATE 通知、hydrated 文件编辑无
    /// open/close 通知（真实机实测）。本函数在自动对账层覆盖到的每个目录上，
    /// 用「索引 vs 磁盘」补齐捕获：新建 ⇒ insert_local_only（pending upload）；
    /// 已水合条目 mtime/size 变 ⇒ **md5 漏斗**（一致只更新基线，不一致才记 upload）。
    /// 纯本地 stat，零 API 配额成本。删除/改名不在这里处理
    /// （placeholder 的 DELETE/RENAME 通知是生产验证过的可靠通道）。
    fn scan_local_dir(
        &mut self,
        root: &Path,
        dir_relative: &str,
        report: &mut AutoLayerReport,
    ) -> Result<(), NativeSyncError> {
        let dir_local = if dir_relative.is_empty() {
            root.to_path_buf()
        } else {
            root.join(dir_relative.replace('/', std::path::MAIN_SEPARATOR_STR))
        };
        let Ok(read_dir) = std::fs::read_dir(&dir_local) else {
            // 目录本地不存在（纯云端目录、尚未有本地内容）：无本地侧可扫。
            return Ok(());
        };
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let relative = if dir_relative.is_empty() {
                name.clone()
            } else {
                format!("{dir_relative}/{name}")
            };
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let indexed = self
                .db
                .entry_by_relative(&relative)
                .map_err(NativeSyncError::Message)?;
            let Some((fs_id, hydration_state, remote_md5, _, _, local_only)) = indexed else {
                // 索引无此行：本地新建（云占位符竞态保险除外）。
                if self.platform.is_cloud_placeholder(&entry.path()) {
                    continue;
                }
                let size = if meta.is_dir() { 0 } else { meta.len() };
                let next_id = self
                    .db
                    .next_local_fs_id()
                    .map_err(NativeSyncError::Message)?;
                self.db
                    .insert_local_only_entry(next_id, &relative, &name, meta.is_dir(), size)
                    .map_err(NativeSyncError::Message)?;
                report.local_new += 1;
                log::info!(
                    "native-sync local scan: new local entry fs_id={next_id} dir={}",
                    meta.is_dir()
                );
                continue;
            };
            if local_only {
                continue; // 已捕获，不重复记账（与 close-completion 路径同原则）。
            }
            if hydration_state == "online_only" {
                continue; // 占位符内容不在本地，谈不上本地编辑。
            }
            if meta.is_dir() {
                continue; // 目录没有内容比对；改名/删除不在本通道（范围纪律）。
            }
            // 已水合条目：mtime/size vs 快照基线。
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let baseline = self
                .db
                .snapshot_for(fs_id)
                .map_err(NativeSyncError::Message)?;
            let Some(snap) = baseline else {
                // 无基线（老库行）：只补记，不判修改（ADR-109「读≠改」老原则）。
                let _ = self.db.set_snapshot(fs_id, mtime, meta.len());
                continue;
            };
            if snap.size.is_none() {
                let _ = self.db.set_snapshot(fs_id, mtime, meta.len());
                continue;
            }
            if snap.size == Some(meta.len()) && snap.mtime == mtime {
                continue;
            }
            // mtime/size 不一致 ⇒ md5 漏斗：内容其实没变就只更新基线。
            let local_md5 = upload::local_md5(&entry.path()).map_err(NativeSyncError::Message)?;
            if Some(local_md5.as_str()) == remote_md5.as_deref() {
                let _ = self.db.set_snapshot(fs_id, mtime, meta.len());
                continue;
            }
            // 真编辑：记 upload + 防脱水（未上云期间被脱水 = 编辑丢失）。
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string();
            self.db
                .set_pending_op(fs_id, Some("upload"), Some(&now_secs))
                .map_err(NativeSyncError::Message)?;
            self.platform.mark_not_in_sync(&entry.path());
            report.local_edited += 1;
            log::info!("native-sync local scan: edit detected fs_id={fs_id}");
        }
        Ok(())
    }

    /// 用**磁盘真值**校正 `hydration_state`（面板必须与磁盘一致）。
    ///
    /// 水合/脱水都会被外部驱动（Explorer、平台后台、第三方程序），只在自家动作里写库就
    /// 会留下错值——真实事故：库里 `online_only`，磁盘上却是完整数据。`on_disk_data_size`
    /// 只读占位符信息、**不触发水合**，正好适合做每轮对账。
    ///
    /// 口径：
    ///   * `OnDisk == 0`       → `online_only`
    ///   * `OnDisk >= size`    → `hydrated`
    ///   * `0 < OnDisk < size` → **部分区间**：不改状态（S1 硬规则：部分区间不得标记
    ///     hydrated），只落一条 warn，等水合完成后的下一轮自然纠正。
    ///   * `remote_missing` / `error` / `stale` 有独立语义，一律不动。
    ///
    /// **绝不**改 `pin_state`：释放空间与固定/取消固定是两件事，pin 只跟着用户走。
    fn reconcile_hydration_with_disk(
        &self,
        fs_id: u64,
        relative: &str,
        local: &Path,
        size: u64,
    ) -> Result<(), NativeSyncError> {
        let Some(on_disk) = self.platform.on_disk_data_size(local) else {
            return Ok(());
        };
        let Some(current) = self
            .db
            .entry_by_relative(relative)
            .map_err(NativeSyncError::Message)?
            .map(|row| row.1)
        else {
            return Ok(());
        };
        if current != "online_only" && current != "hydrated" {
            return Ok(());
        }
        let truth = if on_disk <= 0 {
            "online_only"
        } else if on_disk as u64 >= size {
            "hydrated"
        } else {
            log::warn!(
                "native-sync hydration reconcile sees partial data fs_id={fs_id} on_disk_bytes={on_disk} size={size}; keeping state={current}"
            );
            return Ok(());
        };
        if current == truth {
            return Ok(());
        }
        self.db
            .set_state_by_fs_id(fs_id, truth, None)
            .map_err(NativeSyncError::Message)?;
        log::info!(
            "native-sync hydration reconciled from disk fs_id={fs_id} from={current} to={truth} on_disk_bytes={on_disk} size={size}"
        );
        Ok(())
    }
    /// `EntryRecord` → `NativeSyncEntryView` 的统一映射（`list()` 与 `list_actionable()` 共用）。
    fn entry_views(
        &self,
        entries: Vec<crate::model::EntryRecord>,
    ) -> Result<Vec<NativeSyncEntryView>, NativeSyncError> {
        entries
            .into_iter()
            .map(|e| {
                let cloud_fs_id = if e.local_only { None } else { Some(e.fs_id) };
                Ok(NativeSyncEntryView {
                    fs_id: cloud_fs_id.map(|v| v as u64),
                    parent_fs_id: e.parent_fs_id,
                    name: e.name,
                    relative_path: e.relative_path,
                    is_dir: e.is_dir,
                    size: e.size,
                    modified_at: e.modified_at,
                    state: e.state,
                    pin_state: e.pin_state,
                    progress: match cloud_fs_id {
                        Some(id) if id >= 0 => self
                            .db
                            .hydration_progress_for(id as u64)
                            .map_err(NativeSyncError::Message)?,
                        // 本地新建条目没有 hydration job。
                        _ => None,
                    },
                    error: e.error,
                    pending_operation: e.pending_op,
                    pending_since: e.pending_since,
                    has_children: e.has_children,
                })
            })
            .collect()
    }
    pub fn list(
        &self,
        parent: Option<String>,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<NativeSyncListView, NativeSyncError> {
        let parent = parent.unwrap_or_else(|| "/".into());
        let limit = limit.clamp(1, 1000);
        let offset = cursor
            .as_deref()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let entries = self
            .db
            .entries(&parent, limit, offset)
            .map_err(NativeSyncError::Message)?;
        let views = self.entry_views(entries)?;
        let next_cursor = if views.len() == limit {
            Some((offset + views.len()).to_string())
        } else {
            None
        };
        let total = self
            .db
            .count_entries(&parent)
            .map_err(NativeSyncError::Message)?;
        Ok(NativeSyncListView {
            entries: views,
            next_cursor,
            total,
            parent_path: parent,
        })
    }
    /// 「可操作」视图（设置页默认）：全树扁平、不按父目录——**已下载/已固定/需处理/本地新建**。
    ///
    /// 契约：返回的 `parent_path` 恒为 `"/"`（该视图没有父目录概念）；
    /// 分页语义与 `list()` 相同（`cursor` 是偏移量）；该视图**不读网络**（SQLite 查询）。
    pub fn list_actionable(
        &self,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<NativeSyncListView, NativeSyncError> {
        let limit = limit.clamp(1, 1000);
        let offset = cursor
            .as_deref()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let entries = self
            .db
            .actionable_entries(limit, offset)
            .map_err(NativeSyncError::Message)?;
        let views = self.entry_views(entries)?;
        let next_cursor = if views.len() == limit {
            Some((offset + views.len()).to_string())
        } else {
            None
        };
        let total = self
            .db
            .count_actionable()
            .map_err(NativeSyncError::Message)?;
        Ok(NativeSyncListView {
            entries: views,
            next_cursor,
            total,
            parent_path: "/".into(),
        })
    }
    pub fn action(
        &mut self,
        action: NativeSyncAction,
    ) -> Result<NativeSyncStatusView, NativeSyncError> {
        let Some((root, enabled, _)) = self.db.root().map_err(NativeSyncError::Message)? else {
            return Err(NativeSyncError::Message("请先启用 NativeSync".into()));
        };
        if !enabled {
            return Err(NativeSyncError::Message("NativeSync 未启用".into()));
        }
        let local = root.join(
            action
                .relative_path
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        );
        match action.action {
            EntryAction::Pin => {
                let fs_id = self.cloud_fs_id_for(&action.relative_path)?;
                self.platform.pin(&local, true)?;
                self.db
                    .set_pin_state(fs_id, true)
                    .map_err(NativeSyncError::Message)?;
            }
            EntryAction::Unpin => {
                let fs_id = self.cloud_fs_id_for(&action.relative_path)?;
                self.platform.pin(&local, false)?;
                self.db
                    .set_pin_state(fs_id, false)
                    .map_err(NativeSyncError::Message)?;
            }
            EntryAction::Dehydrate => {
                let Some((fs_id, state, remote_md5, _, _, local_only)) = self
                    .db
                    .entry_by_relative(&action.relative_path)
                    .map_err(NativeSyncError::Message)?
                else {
                    return Err(NativeSyncError::Message("找不到同步条目".into()));
                };
                if local_only {
                    return Err(NativeSyncError::Message(
                        crate::db::LOCAL_ONLY_NO_REMOTE.into(),
                    ));
                }
                if state != "hydrated" {
                    return Err(NativeSyncError::Message(
                        "只有已完整下载且已同步的文件才能释放空间".into(),
                    ));
                }
                if let Some(expected) = remote_md5 {
                    // 百度 md5 不保证 32 位十六进制（历史数据为其它编码），非十六进制跳过比对。
                    let hex =
                        expected.len() == 32 && expected.bytes().all(|b| b.is_ascii_hexdigit());
                    // ③：**先看磁盘真值**——若该条目已经脱水（OnDiskDataSize == 0），
                    // 绝不做 std::fs::read：对已脱水占位符的读访问会立刻把内容水合回来，
                    // 等于白释放一次（事故 D1 的同类）。索引里的 state 可能滞后于磁盘，故以磁盘为准。
                    let already_dehydrated = self.platform.on_disk_data_size(&local) == Some(0);
                    if hex && already_dehydrated {
                        log::debug!(
                            "native-sync dehydrate: skip md5 verification (already dehydrated on disk)"
                        );
                    } else if hex {
                        let bytes = std::fs::read(&local).map_err(|e| {
                            NativeSyncError::Message(format!("读取本地文件失败: {e}"))
                        })?;
                        let actual = format!("{:x}", md5::compute(bytes));
                        if !actual.eq_ignore_ascii_case(&expected) {
                            return Err(NativeSyncError::Message(
                                "本地内容与远端 md5 不一致，拒绝释放空间".into(),
                            ));
                        }
                    }
                }
                // 先释放空间，再标记 in-sync —— 使平台侧状态与我们索引里的 hydration_state 保持一致。
                // （早期注释把"顺序会改变图标"当成实测结论；该解释**已被 2026-10-12 的证据取代**：
                //  蓝色循环箭头来自"显式 UNPINNED 的语义 = 已登记待脱水"，见 docs/问题与解决.md 相应条目。
                //  仅保留可验证的事实描述，避免误导后来者。）
                self.platform.dehydrate(&local)?;
                self.platform.mark_in_sync(&local)?;
                self.db
                    .set_state_by_fs_id(fs_id as u64, "online_only", None)
                    .map_err(NativeSyncError::Message)?;
            }
            EntryAction::Retry => {
                if let Some((fs_id, _, _, _, _, local_only)) = self
                    .db
                    .entry_by_relative(&action.relative_path)
                    .map_err(NativeSyncError::Message)?
                {
                    if local_only {
                        return Err(NativeSyncError::Message(
                            crate::db::LOCAL_ONLY_NO_REMOTE.into(),
                        ));
                    }
                    self.db
                        .set_state_by_fs_id(fs_id as u64, "online_only", None)
                        .map_err(NativeSyncError::Message)?;
                }
            }
            EntryAction::AckRemoteMissing => {
                let Some((fs_id, state, _, _, _, local_only)) = self
                    .db
                    .entry_by_relative(&action.relative_path)
                    .map_err(NativeSyncError::Message)?
                else {
                    return Err(NativeSyncError::Message("找不到同步条目".into()));
                };
                if local_only {
                    return Err(NativeSyncError::Message(
                        crate::db::LOCAL_ONLY_NO_REMOTE.into(),
                    ));
                }
                if state != "remote_missing" {
                    return Err(NativeSyncError::Message("条目不是远端删除状态".into()));
                }
                self.db
                    .acknowledge_remote_missing(fs_id as u64)
                    .map_err(NativeSyncError::Message)?;
            }
        }
        self.status()
    }
    /// 云端操作（pin / dehydrate / retry / ack）前的 fs_id 出口守卫：
    /// 本地新建条目没有云端对应物，必须显式拒绝，不构造非法云端请求。
    fn cloud_fs_id_for(&self, relative: &str) -> Result<u64, NativeSyncError> {
        let Some((fs_id, _, _, _, _, local_only)) = self
            .db
            .entry_by_relative(relative)
            .map_err(NativeSyncError::Message)?
        else {
            return Err(NativeSyncError::Message("找不到同步条目".into()));
        };
        if local_only {
            return Err(NativeSyncError::Message(
                crate::db::LOCAL_ONLY_NO_REMOTE.into(),
            ));
        }
        self.db.cloud_fs_id(fs_id).map_err(NativeSyncError::Message)
    }
    pub fn open_path(&self, relative: &str) -> Result<PathBuf, NativeSyncError> {
        let Some((root, _, _)) = self.db.root().map_err(NativeSyncError::Message)? else {
            return Err(NativeSyncError::Message("同步根未配置".into()));
        };
        let rel = Path::new(relative);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(NativeSyncError::Message("路径必须位于同步根内".into()));
        }
        let full = root.join(rel);
        // 归一化统一剥离 Windows canonicalize 的 \\?\ verbatim 前缀，
        // 保证词法/物理两种来源可比较。
        fn normalize(path: &Path) -> PathBuf {
            let text = path.to_string_lossy();
            let stripped = text.strip_prefix(r"\\?\").unwrap_or(&text);
            PathBuf::from(stripped)
        }
        // 对（可能尚不存在的）候选做等价词法归一：自最深存在祖先 canonicalize，
        // 再拼回剩余段。修复符号链接/8.3 短名/大小写差异下的合法路径误判越界
        // （如 macOS /var→/private/var、CI runner temp 短名）。
        fn resolve_lexical(path: &Path) -> Result<PathBuf, NativeSyncError> {
            let mut tail: Vec<std::ffi::OsString> = Vec::new();
            let mut existing = path;
            while !existing.exists() {
                match existing.parent() {
                    Some(parent) => {
                        tail.push(
                            existing
                                .file_name()
                                .ok_or_else(|| NativeSyncError::Message("路径前缀解析失败".into()))?
                                .to_os_string(),
                        );
                        existing = parent;
                    }
                    None => break,
                }
            }
            let mut out = if existing.exists() {
                std::fs::canonicalize(existing)
                    .map_err(|e| NativeSyncError::Message(e.to_string()))?
            } else {
                existing.to_path_buf()
            };
            for segment in tail.iter().rev() {
                out.push(segment);
            }
            Ok(out)
        }
        let root_abs = normalize(&std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone()));
        let candidate = normalize(&resolve_lexical(&full)?);
        if !candidate.starts_with(&root_abs) {
            return Err(NativeSyncError::Message("路径越界".into()));
        }
        Ok(candidate)
    }
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    use crate::model::{EntryRecord, NativeSyncState, PinState};
    use crate::platform::PlatformError;
    use futures_util::FutureExt;
    use pandock_core::cloud::{CloudError, ResourceSnapshot};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use tempfile::{tempdir, TempDir};

    #[derive(Default)]
    struct FakePlatform {
        calls: Arc<Mutex<Vec<String>>>,
        /// 磁盘真值表：路径 → 本地实际占用字节（生产来自 CFAPI 的 OnDiskDataSize）。
        /// 缺省为空 ⇒ `on_disk_data_size` 返回 None，既有用例行为不变。
        on_disk: Arc<Mutex<HashMap<String, i64>>>,
        /// 经 create_placeholder 创建的路径（is_cloud_placeholder 的判定集）。
        placeholder_paths: Arc<Mutex<std::collections::HashSet<String>>>,
    }
    impl PlatformSync for FakePlatform {
        fn supported(&self) -> Result<(), String> {
            Ok(())
        }
        fn register(&mut self, root: &Path) -> Result<(), PlatformError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("register:{}", root.display()));
            Ok(())
        }
        fn connect(&mut self) -> Result<(), PlatformError> {
            self.calls.lock().unwrap().push("connect".into());
            Ok(())
        }
        fn disconnect(&mut self) -> Result<(), PlatformError> {
            self.calls.lock().unwrap().push("disconnect".into());
            Ok(())
        }
        fn unregister(&mut self) -> Result<(), PlatformError> {
            self.calls.lock().unwrap().push("unregister".into());
            Ok(())
        }
        fn set_hydration_backend(&mut self, _backend: Arc<dyn CloudBackend>) {
            self.calls.lock().unwrap().push("set_backend".into());
        }
        fn set_remote_mapping(&mut self, relative: &str, remote: RemotePath, fs_id: u64) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("mapping:{relative}:{}:{fs_id}", remote.as_str()));
        }
        fn set_event_sink(&mut self, _sink: Option<Arc<dyn Fn(NativeSyncEvent) + Send + Sync>>) {
            self.calls.lock().unwrap().push("set_event_sink".into());
        }
        fn create_placeholder(
            &mut self,
            path: &Path,
            is_dir: bool,
            _size: u64,
            _identity: &[u8],
        ) -> Result<(), PlatformError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("placeholder:{}:dir={is_dir}", path.display()));
            // 与生产一致：占位符在磁盘上真实存在（否则本地存在的判断会失真）。
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if is_dir {
                let _ = std::fs::create_dir_all(path);
            } else if !path.exists() {
                std::fs::write(path, b"").expect("fake placeholder file");
            }
            self.placeholder_paths
                .lock()
                .unwrap()
                .insert(path.to_string_lossy().to_string());
            Ok(())
        }
        fn pin(&mut self, _path: &Path, pinned: bool) -> Result<(), PlatformError> {
            self.calls.lock().unwrap().push(format!("pin:{pinned}"));
            Ok(())
        }
        fn dehydrate(&mut self, path: &Path) -> Result<(), PlatformError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("dehydrate:{}", path.display()));
            Ok(())
        }
        fn pin_state(&self, _path: &Path) -> Option<bool> {
            None
        }
        fn is_cloud_placeholder(&self, path: &Path) -> bool {
            self.placeholder_paths
                .lock()
                .unwrap()
                .contains(&path.to_string_lossy().to_string())
        }
        fn on_disk_data_size(&self, path: &Path) -> Option<i64> {
            self.on_disk
                .lock()
                .unwrap()
                .get(&path.to_string_lossy().to_string())
                .copied()
        }
        fn mark_in_sync(&mut self, _path: &Path) -> Result<(), PlatformError> {
            self.calls.lock().unwrap().push("mark_in_sync".into());
            Ok(())
        }
        fn mark_not_in_sync(&mut self, path: &Path) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("mark_not_in_sync:{}", path.display()));
        }
        fn remove_placeholder(&mut self, path: &Path, is_dir: bool) -> Result<(), PlatformError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("remove:{}:dir={is_dir}", path.display()));
            let _ = if is_dir {
                std::fs::remove_dir(path)
            } else {
                std::fs::remove_file(path)
            };
            Ok(())
        }
    }

    struct FakeBackend {
        listings: Mutex<HashMap<String, Vec<ResourceSnapshot>>>,
        fail_list: AtomicBool,
        /// 记录每次 list 的目录（调用预算/游标断言用）。
        list_calls: Mutex<Vec<String>>,
        /// 写方向调用记录与脚本（S2 drain 测试）：(remote, size, rtype)。
        uploads: Mutex<Vec<(String, u64, u32)>>,
        upload_script: Mutex<Vec<Result<pandock_core::cloud::UploadOutcome, CloudError>>>,
        removes: Mutex<Vec<String>>,
        renames: Mutex<Vec<(String, String)>>,
        created_dirs: Mutex<Vec<String>>,
        /// 批量 stat 的独立视图（模拟「listing 滞后但 stat 新鲜」，默认空 = 查无）。
        stat_listings: Mutex<HashMap<String, ResourceSnapshot>>,
        /// 批量 stat 调用记录（每次调用的路径数）。
        stats_calls: Mutex<Vec<usize>>,
    }
    impl FakeBackend {
        fn script_upload(
            &self,
            results: Vec<Result<pandock_core::cloud::UploadOutcome, CloudError>>,
        ) {
            *self.upload_script.lock().unwrap() = results;
        }
        fn upload_outcome(fs_id: u64, size: u64) -> pandock_core::cloud::UploadOutcome {
            pandock_core::cloud::UploadOutcome {
                fs_id: Some(fs_id),
                md5: Some("deadbeef".into()),
                size,
                mtime: Some(1_700_000_100),
            }
        }
    }
    impl FakeBackend {
        fn replace(&self, entries: Vec<(&str, Vec<ResourceSnapshot>)>) {
            let mut map = self.listings.lock().unwrap();
            map.clear();
            for (key, value) in entries {
                map.insert(key.to_string(), value);
            }
        }
    }
    impl CloudBackend for FakeBackend {
        fn stat<'a>(
            &'a self,
            path: &'a RemotePath,
        ) -> futures_util::future::BoxFuture<'a, Result<ResourceSnapshot, CloudError>> {
            let found = self
                .listings
                .lock()
                .unwrap()
                .values()
                .flatten()
                .find(|s| s.path.as_str() == path.as_str())
                .cloned();
            async move { found.ok_or_else(|| CloudError::unknown("not found")) }.boxed()
        }
        fn list<'a>(
            &'a self,
            path: &'a RemotePath,
        ) -> futures_util::future::BoxFuture<'a, Result<Vec<ResourceSnapshot>, CloudError>>
        {
            self.list_calls
                .lock()
                .unwrap()
                .push(path.as_str().to_string());
            let out = if self.fail_list.load(Ordering::SeqCst) {
                Err(CloudError::unknown("list failed"))
            } else {
                Ok(self
                    .listings
                    .lock()
                    .unwrap()
                    .get(path.as_str())
                    .cloned()
                    .unwrap_or_default())
            };
            async move { out }.boxed()
        }
        fn upload<'a>(
            &'a self,
            path: &'a RemotePath,
            _local: &'a std::path::Path,
            size: u64,
            rtype: u32,
        ) -> futures_util::future::BoxFuture<
            'a,
            Result<pandock_core::cloud::UploadOutcome, CloudError>,
        > {
            self.uploads
                .lock()
                .unwrap()
                .push((path.as_str().to_string(), size, rtype));
            let result = self.upload_script.lock().unwrap().first().cloned();
            if result.is_some() {
                let _ = self.upload_script.lock().unwrap().remove(0);
            }
            let outcome = result.unwrap_or_else(|| {
                Ok(Self::upload_outcome(
                    9_000_000 + self.uploads.lock().unwrap().len() as u64,
                    size,
                ))
            });
            async move { outcome }.boxed()
        }
        fn rename<'a>(
            &'a self,
            from: &'a RemotePath,
            to: &'a RemotePath,
        ) -> futures_util::future::BoxFuture<'a, Result<(), CloudError>> {
            self.renames
                .lock()
                .unwrap()
                .push((from.as_str().to_string(), to.as_str().to_string()));
            async move { Ok(()) }.boxed()
        }
        fn remove<'a>(
            &'a self,
            path: &'a RemotePath,
        ) -> futures_util::future::BoxFuture<'a, Result<(), CloudError>> {
            self.removes.lock().unwrap().push(path.as_str().to_string());
            async move { Ok(()) }.boxed()
        }
        fn stats<'a>(
            &'a self,
            paths: &'a [RemotePath],
        ) -> futures_util::future::BoxFuture<'a, Result<Vec<ResourceSnapshot>, CloudError>> {
            self.stats_calls
                .lock()
                .unwrap()
                .push(paths.len());
            let found: Vec<ResourceSnapshot> = paths
                .iter()
                .filter_map(|path| self.stat_listings.lock().unwrap().get(path.as_str()).cloned())
                .collect();
            async move { Ok(found) }.boxed()
        }
        fn create_dir<'a>(
            &'a self,
            path: &'a RemotePath,
        ) -> futures_util::future::BoxFuture<'a, Result<(), CloudError>> {
            self.created_dirs
                .lock()
                .unwrap()
                .push(path.as_str().to_string());
            async move { Ok(()) }.boxed()
        }
        fn read_at<'a>(
            &'a self,
            _path: &'a RemotePath,
            _snapshot: &'a ResourceSnapshot,
            _start: u64,
            count: usize,
        ) -> futures_util::future::BoxFuture<'a, Result<Vec<u8>, CloudError>> {
            async move { Ok(vec![7u8; count]) }.boxed()
        }
    }

    fn file_snap(path: &str, fs_id: u64, md5: Option<&str>) -> ResourceSnapshot {
        let remote = RemotePath::parse(path).unwrap();
        let name = remote.file_name().unwrap().to_string();
        ResourceSnapshot::from_entry(
            remote,
            Some(fs_id),
            5,
            0,
            Some(1_700_000_000),
            md5.map(|s| s.to_string()),
            Some(name),
            None,
            None,
        )
    }
    fn dir_snap(path: &str, fs_id: u64) -> ResourceSnapshot {
        let remote = RemotePath::parse(path).unwrap();
        let name = remote.file_name().map(|s| s.to_string());
        ResourceSnapshot::from_entry(
            remote,
            Some(fs_id),
            0,
            1,
            Some(1_700_000_000),
            None,
            name,
            None,
            None,
        )
    }
    fn backend_with(entries: Vec<(&str, Vec<ResourceSnapshot>)>) -> Arc<FakeBackend> {
        let backend = Arc::new(FakeBackend {
            listings: Mutex::new(HashMap::new()),
            fail_list: AtomicBool::new(false),
            list_calls: Mutex::new(Vec::new()),
            uploads: Mutex::new(Vec::new()),
            upload_script: Mutex::new(Vec::new()),
            removes: Mutex::new(Vec::new()),
            renames: Mutex::new(Vec::new()),
            created_dirs: Mutex::new(Vec::new()),
            stat_listings: Mutex::new(HashMap::new()),
            stats_calls: Mutex::new(Vec::new()),
        });
        backend.replace(entries);
        backend
    }
    /// 索引全量快照（干净树断言用；不含 auto_state —— 那本来就是会变的调度状态）。
    fn dump_index(manager: &NativeSyncManager) -> Vec<String> {
        let mut rows = Vec::new();
        let mut st = manager
            .db
            .conn
            .prepare(
                "SELECT r.fs_id,r.remote_path,r.size,r.md5,r.modified_at,r.tombstone,l.relative_path,l.hydration_state FROM remote_entry r LEFT JOIN local_state l ON l.fs_id=r.fs_id ORDER BY r.fs_id",
            )
            .unwrap();
        let mapped = st
            .query_map([], |r| {
                Ok(format!(
                    "{}|{}|{}|{:?}|{:?}|{}|{}|{}",
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?
                ))
            })
            .unwrap();
        for row in mapped {
            rows.push(row.unwrap());
        }
        rows
    }
    fn record(fs_id: i64, relative: &str, md5: Option<&str>) -> EntryRecord {
        EntryRecord {
            fs_id,
            local_only: false,
            parent_fs_id: None,
            remote_path: format!("/{relative}"),
            relative_path: relative.into(),
            name: relative.rsplit('/').next().unwrap().into(),
            is_dir: false,
            size: 3,
            md5: md5.map(str::to_string),
            modified_at: Some(123),
            state: NativeSyncState::OnlineOnly,
            pin_state: PinState::Unpinned,
            error: None,
            has_children: false,
            pending_op: None,
            pending_since: None,
        }
    }
    fn setup() -> (TempDir, PathBuf, Arc<Mutex<Vec<String>>>, NativeSyncManager) {
        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let manager = NativeSyncManager {
            db: IndexDb::open(dir.path()).unwrap(),
            backend: None,
            platform: Box::new(FakePlatform {
                calls: calls.clone(),
                ..Default::default()
            }),
            notifications_config: true,
            notifications_enabled: true,
        };
        (dir, root, calls, manager)
    }
    /// 与 `setup()` 同构，但把「磁盘真值表」交回给用例，用来模拟外部水合/脱水。
    /// 平台记录到的调用序列（用例断言用）。
    type Calls = Arc<Mutex<Vec<String>>>;
    /// 磁盘真值表句柄：路径 → 本地实际占用字节。
    type DiskTruth = Arc<Mutex<HashMap<String, i64>>>;
    fn setup_with_disk() -> (TempDir, PathBuf, Calls, NativeSyncManager, DiskTruth) {
        let dir = tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let on_disk = Arc::new(Mutex::new(HashMap::new()));
        let manager = NativeSyncManager {
            db: IndexDb::open(dir.path()).unwrap(),
            backend: None,
            platform: Box::new(FakePlatform {
                calls: calls.clone(),
                on_disk: on_disk.clone(),
                placeholder_paths: Arc::new(Mutex::new(std::collections::HashSet::new())),
            }),
            notifications_config: true,
            notifications_enabled: true,
        };
        (dir, root, calls, manager, on_disk)
    }
    fn set_on_disk(on_disk: &DiskTruth, path: &Path, bytes: i64) {
        on_disk
            .lock()
            .unwrap()
            .insert(path.to_string_lossy().to_string(), bytes);
    }
    fn hydration_state_of(manager: &NativeSyncManager, fs_id: u64) -> String {
        manager
            .db
            .conn
            .query_row(
                "SELECT hydration_state FROM local_state WHERE fs_id=?1",
                rusqlite::params![fs_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// additive：不带 `filter` 的 `list()` 结果与之前完全一致；`list_actionable()` 扁平、
    /// 只收可操作条目、父路径恒为 `"/"`。
    #[test]
    fn list_actionable_is_flat_filtered_and_list_stays_unchanged() {
        let (_dir, _root, _calls, manager) = setup();
        // 造数：普通 online_only（排除）+ hydrated（进）+ pending delete（进）。
        manager
            .db
            .upsert(&record(11, "a.txt", Some("aaa")))
            .unwrap();
        manager
            .db
            .upsert(&record(12, "b.txt", Some("bbb")))
            .unwrap();
        manager.db.set_state_by_fs_id(12, "hydrated", None).unwrap();
        manager
            .db
            .upsert(&record(13, "c.txt", Some("ccc")))
            .unwrap();
        manager.db.set_pending_op(13, Some("delete"), None).unwrap();
        // 既有行为回归：不带 filter 的 list() 必须仍返回父级的全部条目。
        let plain = manager.list(None, None, 50).unwrap();
        assert_eq!(
            plain.entries.len(),
            3,
            "list() 不带 filter 必须仍返回全部条目"
        );
        assert_eq!(plain.parent_path, "/");
        let actionable = manager.list_actionable(None, 50).unwrap();
        let names: Vec<&str> = actionable
            .entries
            .iter()
            .map(|e| e.relative_path.as_str())
            .collect();
        assert!(!names.contains(&"a.txt"), "仅在线条目不得出现: {names:?}");
        assert!(names.contains(&"b.txt"));
        assert!(names.contains(&"c.txt"));
        assert_eq!(actionable.total, 2);
        assert_eq!(actionable.parent_path, "/", "可操作视图没有父目录概念");
    }

    #[tokio::test]
    async fn sync_now_marks_only_pending_entries_not_in_sync() {
        let (_dir, root, calls, mut manager, on_disk) = setup_with_disk();
        manager.db.set_root(&root, true, "connected").unwrap();
        manager.backend = Some(backend_with(vec![(
            "/",
            vec![
                file_snap("/a.txt", 11, Some("aaa")),
                file_snap("/b.txt", 12, Some("bbb")),
            ],
        )]));
        // b.txt 模拟「老库行 / 重启后遗留」的未同步本地改动：必须被重新标未同步。
        manager
            .db
            .upsert(&record(12, "b.txt", Some("bbb")))
            .unwrap();
        manager
            .db
            .set_pending_op(12, Some("upload"), Some("2026-10-12T00:00:00Z"))
            .unwrap();
        set_on_disk(&on_disk, &root.join("b.txt"), 5);
        manager.sync_now().await.unwrap();
        let marked = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("mark_not_in_sync:"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            marked.len(),
            1,
            "只有带 pending 的条目才标未同步: {marked:?}"
        );
        assert!(marked[0].contains("b.txt"), "{marked:?}");
        // 磁盘对账照旧工作，不受该守卫影响。
        assert_eq!(hydration_state_of(&manager, 12), "hydrated");
    }

    #[tokio::test]
    async fn sync_now_reconciles_hydration_state_from_disk_truth() {
        let (_dir, root, calls, mut manager, on_disk) = setup_with_disk();
        manager.db.set_root(&root, true, "connected").unwrap();
        manager.backend = Some(backend_with(vec![(
            "/",
            vec![file_snap("/a.txt", 11, Some("aaa"))],
        )]));
        let local = root.join("a.txt");
        // 外部已经把内容读了回来（磁盘是完整数据）：库里的 online_only 必须被纠正。
        set_on_disk(&on_disk, &local, 5);
        manager.sync_now().await.unwrap();
        assert_eq!(hydration_state_of(&manager, 11), "hydrated");
        // 反向：平台把数据释放掉了（OnDisk=0）⇒ 回到 online_only。
        set_on_disk(&on_disk, &local, 0);
        manager.sync_now().await.unwrap();
        assert_eq!(hydration_state_of(&manager, 11), "online_only");
        // 部分区间（2/5）不得被标记成 hydrated：状态保持原值，只落 warn。
        manager.db.set_state_by_fs_id(11, "hydrated", None).unwrap();
        set_on_disk(&on_disk, &local, 2);
        manager.sync_now().await.unwrap();
        assert_eq!(hydration_state_of(&manager, 11), "hydrated");
        // 对账绝不碰用户 pin：既有实现也不得凭空产生 pin 调用。
        assert!(!calls.lock().unwrap().iter().any(|c| c.starts_with("pin:")));
    }

    #[tokio::test]
    async fn sync_now_reconcile_keeps_remote_missing_and_error_states() {
        let (_dir, root, _calls, mut manager, on_disk) = setup_with_disk();
        manager.db.set_root(&root, true, "connected").unwrap();
        manager.backend = Some(backend_with(vec![(
            "/",
            vec![file_snap("/a.txt", 11, Some("aaa"))],
        )]));
        let local = root.join("a.txt");
        set_on_disk(&on_disk, &local, 5);
        manager.sync_now().await.unwrap();
        assert_eq!(hydration_state_of(&manager, 11), "hydrated");
        // remote_missing 有独立语义：磁盘真值不得把它冲掉（用户确认前必须保留标记）。
        manager
            .db
            .set_state_by_fs_id(11, "remote_missing", Some("远端条目已删除，保留本地内容"))
            .unwrap();
        manager.sync_now().await.unwrap();
        assert_eq!(hydration_state_of(&manager, 11), "remote_missing");
        // error 同理：等 retry 清理，不由对账顺手抹掉。
        manager
            .db
            .set_state_by_fs_id(11, "error", Some("boom"))
            .unwrap();
        manager.sync_now().await.unwrap();
        assert_eq!(hydration_state_of(&manager, 11), "error");
    }

    fn pin_state_of(manager: &NativeSyncManager, fs_id: u64) -> String {
        manager
            .db
            .conn
            .query_row(
                "SELECT pin_state FROM local_state WHERE fs_id=?1",
                rusqlite::params![fs_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn enable_disable_lifecycle_updates_state_and_platform() {
        let (_dir, root, calls, mut manager) = setup();
        #[cfg(not(windows))]
        assert!(manager.enable(None).is_err());
        let status = manager.enable(Some(root.clone())).unwrap();
        assert!(status.enabled);
        assert!(status.supported);
        assert_eq!(
            status.root_path.as_deref(),
            Some(root.to_string_lossy().as_ref())
        );
        {
            let calls = calls.lock().unwrap();
            assert!(calls.iter().any(|c| c.starts_with("register:")));
            assert!(calls.contains(&"connect".to_string()));
        }
        let status = manager.disable(false).unwrap();
        assert!(!status.enabled);
        assert!(calls.lock().unwrap().contains(&"disconnect".to_string()));
        manager.enable(Some(root.clone())).unwrap();
        manager.disable(true).unwrap();
        assert!(calls.lock().unwrap().contains(&"unregister".to_string()));
    }

    #[test]
    fn reconnect_from_db_restores_mappings_after_restart() {
        let (dir, root, _calls, manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        manager.db.upsert(&record(11, "a.txt", None)).unwrap();
        manager.db.upsert(&record(12, "d/b.txt", None)).unwrap();
        drop(manager);
        // 模拟重启：同 data-dir 上新建 manager（内存映射为空），走自动重连。
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut restarted = NativeSyncManager {
            db: IndexDb::open(dir.path()).unwrap(),
            backend: None,
            platform: Box::new(FakePlatform {
                calls: calls.clone(),
                ..Default::default()
            }),
            notifications_config: true,
            notifications_enabled: true,
        };
        let status = restarted.reconnect_from_db().unwrap();
        assert!(status.enabled);
        assert_eq!(status.state, "connected");
        let calls = calls.lock().unwrap();
        assert!(calls.iter().any(|c| c.starts_with("register:")));
        assert!(calls.contains(&"connect".to_string()));
        assert!(calls.iter().any(|c| c == "mapping:a.txt:/a.txt:11"));
        assert!(calls.iter().any(|c| c == "mapping:d/b.txt:/d/b.txt:12"));
    }

    #[test]
    fn reconnect_with_missing_root_goes_disconnected_without_rehome() {
        let (dir, root, _calls, manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        drop(manager);
        std::fs::remove_dir_all(&root).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut restarted = NativeSyncManager {
            db: IndexDb::open(dir.path()).unwrap(),
            backend: None,
            platform: Box::new(FakePlatform {
                calls: calls.clone(),
                ..Default::default()
            }),
            notifications_config: true,
            notifications_enabled: true,
        };
        let status = restarted.reconnect_from_db().unwrap();
        assert!(!status.enabled);
        assert_eq!(status.state, "disconnected");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn reconnect_without_root_row_stays_disabled() {
        let (_dir, _root, calls, mut manager) = setup();
        let status = manager.reconnect_from_db().unwrap();
        assert!(!status.enabled);
        assert_eq!(status.state, "disabled");
        assert!(calls.lock().unwrap().is_empty());
    }
    #[test]
    fn set_event_sink_wires_through_platform() {
        let (_dir, _root, calls, mut manager) = setup();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        manager.set_event_sink(Some(Arc::new(move |event| {
            captured.lock().unwrap().push(event);
        })));
        assert!(calls
            .lock()
            .unwrap()
            .contains(&"set_event_sink".to_string()));
    }
    #[test]
    fn status_propagates_db_errors_instead_of_disguising() {
        let (_dir, root, _calls, manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let healthy = manager.status().unwrap();
        assert!(healthy.enabled);
        assert_eq!(healthy.state, "connected");
        // 注入 root 表损坏：必须显式 Err，不得伪装成“未启用”。
        manager.db.conn.execute("DROP TABLE sync_root", []).unwrap();
        assert!(manager.status().is_err());
        // 注入计数表损坏：同样显式 Err。
        let (_dir2, _root2, _calls2, manager2) = setup();
        manager2
            .db
            .conn
            .execute("DROP TABLE remote_entry", [])
            .unwrap();
        assert!(manager2.status().is_err());
    }

    #[tokio::test]
    async fn sync_now_backfills_parent_fs_id_and_fixes_has_children() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        manager.backend = Some(backend_with(vec![
            (
                "/",
                vec![
                    dir_snap("/d", 10),
                    dir_snap("/empty", 16),
                    file_snap("/leaf.txt", 15, None),
                ],
            ),
            ("/d", vec![dir_snap("/d/sub", 13)]),
            ("/d/sub", vec![file_snap("/d/sub/f.txt", 14, None)]),
        ]));
        manager.sync_now().await.unwrap();
        let parent_of = |fs_id: u64| -> Option<u64> {
            manager
                .db
                .conn
                .query_row(
                    "SELECT parent_fs_id FROM remote_entry WHERE fs_id=?1",
                    rusqlite::params![fs_id],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(parent_of(13), Some(10), "子目录应回填父目录 fs_id");
        assert_eq!(parent_of(14), Some(13), "文件应回填父目录 fs_id");
        assert_eq!(parent_of(10), None, "根级目录 parent_fs_id 为 NULL");
        assert_eq!(parent_of(15), None, "根级文件 parent_fs_id 为 NULL");
        let root_page = manager.list(None, None, 100).unwrap();
        let d = root_page
            .entries
            .iter()
            .find(|e| e.relative_path == "d")
            .unwrap();
        assert!(d.has_children, "含子目录的目录 hasChildren=true");
        let empty = root_page
            .entries
            .iter()
            .find(|e| e.relative_path == "empty")
            .unwrap();
        assert!(!empty.has_children, "空目录 hasChildren=false");
        let d_page = manager.list(Some("d".into()), None, 100).unwrap();
        let sub = d_page
            .entries
            .iter()
            .find(|e| e.relative_path == "d/sub")
            .unwrap();
        assert!(sub.has_children, "含文件的目录 hasChildren=true");
    }
    #[test]
    fn action_requires_enabled_root() {
        let (_dir, _root, _calls, mut manager) = setup();
        let err = manager
            .action(NativeSyncAction {
                relative_path: "x".into(),
                action: EntryAction::Pin,
            })
            .unwrap_err();
        assert!(err.to_string().contains("启用"));
    }

    #[tokio::test]
    async fn sync_now_requires_enabled_root_and_backend() {
        let (_dir, root, _calls, mut manager) = setup();
        let err = manager.sync_now().await.unwrap_err();
        assert!(err.to_string().contains("启用"));
        manager.db.set_root(&root, true, "connected").unwrap();
        let err = manager.sync_now().await.unwrap_err();
        assert!(err.to_string().contains("授权"));
        manager.db.set_root(&root, false, "disabled").unwrap();
        manager.backend = Some(backend_with(vec![]));
        let err = manager.sync_now().await.unwrap_err();
        assert!(err.to_string().contains("未启用"));
    }

    #[tokio::test]
    async fn sync_now_enumerates_tree_and_creates_placeholders() {
        let (_dir, root, calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        manager.backend = Some(backend_with(vec![
            (
                "/",
                vec![dir_snap("/docs", 10), file_snap("/a.txt", 11, Some("aaa"))],
            ),
            ("/docs", vec![file_snap("/docs/b.txt", 12, None)]),
        ]));
        let status = manager.sync_now().await.unwrap();
        assert_eq!(status.total_entries, 3);
        let page = manager.list(None, None, 50).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.entries.len(), 2);
        let calls = calls.lock().unwrap();
        assert!(calls.iter().any(|c| c == "set_backend"));
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("placeholder:"))
                .count(),
            3
        );
        assert!(calls.iter().any(|c| c.contains("a.txt")));
        // mapping 契约：relative -> remote path + fs_id（不携带缓存 dlink 的快照）。
        let mappings = calls
            .iter()
            .filter(|c| c.starts_with("mapping:"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(mappings.len(), 3);
        assert!(mappings.iter().any(|c| c == "mapping:a.txt:/a.txt:11"));
        assert!(mappings
            .iter()
            .any(|c| c == "mapping:docs/b.txt:/docs/b.txt:12"));
        assert!(calls.iter().any(|c| c.contains("b.txt")));
    }

    #[tokio::test]
    async fn incremental_root_quick_check_detects_top_changes() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![(
            "/",
            vec![dir_snap("/docs", 10), file_snap("/a.txt", 11, Some("aaa"))],
        )]);
        manager.backend = Some(backend.clone());
        let report = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert!(report.calls >= 1, "根快查至少 1 次调用: {}", report.calls);
        assert_eq!(report.added, 2);
        assert_eq!(report.skipped, None);
        assert!(!report.preempted);
        // 与 sync_now 同一条 ingest 路径：占位符与映射已落。
        let page = manager.list(None, None, 50).unwrap();
        assert_eq!(page.total, 2);
        // /docs 是新目录（可疑顶层）⇒ 被定向下钻一次；不变子树（无）不重复列。
        let lists = backend.list_calls.lock().unwrap().clone();
        assert_eq!(lists, vec!["/".to_string(), "/docs".to_string()]);
        assert_eq!(report.drilled, 1);
        // 成功层写回调度状态。
        let status = manager.status().unwrap();
        assert_eq!(status.auto_sync_state, "ok");
        assert!(status.last_sync_at.is_some());
        assert!(status.last_layer_at.is_some());
        assert_eq!(status.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn incremental_drilldown_capped_at_4_and_counts_into_budget() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        // 6 个**新**顶层目录（prior_absent ⇒ 可疑），每个带一个子文件。
        let mut root_listing = Vec::new();
        let mut entries: Vec<(&str, Vec<ResourceSnapshot>)> = Vec::new();
        let mut owned: Vec<(String, Vec<ResourceSnapshot>)> = Vec::new();
        for i in 0..6 {
            root_listing.push(dir_snap(&format!("/d{i}"), 100 + i));
            owned.push((
                format!("/d{i}"),
                vec![file_snap(&format!("/d{i}/f.txt"), 200 + i, None)],
            ));
        }
        for (k, v) in &owned {
            entries.push((k.as_str(), v.clone()));
        }
        entries.insert(0, ("/", root_listing));
        let backend = backend_with(entries);
        manager.backend = Some(backend.clone());
        let report = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(report.drilled, 4, "定向下钻 ≤4（ADR-111 定稿）");
        assert_eq!(report.deferred, 2, "其余 2 个推迟到下一层");
        let lists = backend.list_calls.lock().unwrap().clone();
        assert_eq!(
            lists.len(),
            1 + 4 + 2,
            "根 1 + 钻 4 + 深扫 2（已钻目录跳过不重列）"
        );
        // 深扫游标跳过本层已钻的 /d0../d3，只列 /d4 /d5。
        let deep: Vec<_> = lists[5..].to_vec();
        assert_eq!(
            deep,
            vec!["/d4", "/d5"],
            "深扫游标跳过本层已钻目录: {deep:?}"
        );
        assert_eq!(report.deep_scanned, 2);
    }

    #[tokio::test]
    async fn incremental_budget_defers_overflow() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let entries = vec![
            (
                "/",
                vec![
                    dir_snap("/d0", 100),
                    dir_snap("/d1", 101),
                    dir_snap("/d2", 102),
                ],
            ),
            ("/d0", vec![file_snap("/d0/f.txt", 200, None)]),
        ];
        let backend = backend_with(entries);
        manager.backend = Some(backend.clone());
        let params = AutoSyncParams {
            layer_budget: 2,
            ..Default::default()
        };
        let report = manager
            .reconcile_incremental(&params, &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.drilled, 1, "预算 2 = 根 1 + 钻 1");
        assert_eq!(report.deferred, 2);
        assert_eq!(report.deep_scanned, 0, "预算耗尽不得再深扫");
        assert!(report.calls <= 2);
    }

    #[tokio::test]
    async fn incremental_deep_scan_cursor_round_robin_persists() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![
            (
                "/",
                vec![
                    dir_snap("/d1", 101),
                    dir_snap("/d2", 102),
                    dir_snap("/d3", 103),
                ],
            ),
            ("/d1", vec![file_snap("/d1/a.txt", 201, None)]),
            ("/d2", vec![file_snap("/d2/b.txt", 202, None)]),
            ("/d3", vec![file_snap("/d3/c.txt", 203, None)]),
        ]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        let params = AutoSyncParams {
            deep_scan_batch: 2,
            ..Default::default()
        };
        let cancel = Arc::new(AtomicBool::new(false));
        // 第一层：扫 /d1 /d2，游标停在 /d2。
        backend.list_calls.lock().unwrap().clear();
        let r1 = manager
            .reconcile_incremental(&params, &cancel)
            .await
            .unwrap();
        assert_eq!(r1.deep_scanned, 2);
        assert_eq!(
            manager
                .db
                .auto_state_get("deep_scan_cursor")
                .unwrap()
                .as_deref(),
            Some("/d2")
        );
        // 第二层：/d3 扫完后回绕到 /d1（继续覆盖，不算错误）。
        backend.list_calls.lock().unwrap().clear();
        let r2 = manager
            .reconcile_incremental(&params, &cancel)
            .await
            .unwrap();
        assert_eq!(r2.deep_scanned, 2);
        let lists = backend.list_calls.lock().unwrap().clone();
        let deep: Vec<_> = lists[1..].to_vec();
        assert_eq!(deep, vec!["/d3", "/d1"], "游标回绕继续覆盖: {deep:?}");
        assert_eq!(
            manager
                .db
                .auto_state_get("deep_scan_cursor")
                .unwrap()
                .as_deref(),
            Some("/d1")
        );
    }

    #[tokio::test]
    async fn incremental_scoped_missing_only_within_scanned_dir() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![
            ("/", vec![dir_snap("/A", 101), dir_snap("/B", 102)]),
            (
                "/A",
                vec![file_snap("/A/a1", 201, None), file_snap("/A/a2", 202, None)],
            ),
            ("/B", vec![file_snap("/B/b1", 203, None)]),
        ]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // 云端删了 /A/a2；本层只深扫 /A（/B 未扫 ⇒ b1 绝不能被误判远端删除）。
        backend.replace(vec![
            ("/", vec![dir_snap("/A", 101), dir_snap("/B", 102)]),
            ("/A", vec![file_snap("/A/a1", 201, None)]),
            ("/B", vec![file_snap("/B/b1", 203, None)]),
        ]);
        let params = AutoSyncParams {
            deep_scan_batch: 1,
            ..Default::default()
        };
        let report = manager
            .reconcile_incremental(&params, &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.deep_scanned, 1);
        assert_eq!(report.removed, 1, "只移除 /A/a2: {report:?}");
        let page_a = manager.list(Some("A".into()), None, 50).unwrap();
        assert_eq!(page_a.total, 1);
        assert_eq!(page_a.entries[0].name, "a1");
        let page_b = manager.list(Some("B".into()), None, 50).unwrap();
        assert_eq!(page_b.total, 1, "/B 未扫 ⇒ b1 必须原样保留");
        assert_eq!(page_b.entries[0].name, "b1");
    }

    #[tokio::test]
    async fn incremental_skips_when_disabled_or_paused() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, false, "disabled").unwrap();
        let backend = backend_with(vec![]);
        manager.backend = Some(backend.clone());
        let report = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(report.skipped, Some("disabled"));
        assert!(backend.list_calls.lock().unwrap().is_empty());
        // paused 同样跳过（不消耗后端调用）。
        manager.db.set_root(&root, true, "connected").unwrap();
        manager.db.auto_state_set("paused", "1").unwrap();
        let report = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(report.skipped, Some("paused"));
        assert!(backend.list_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn incremental_preempted_between_listings_is_not_failure() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![
            ("/", vec![dir_snap("/d0", 100)]),
            ("/d0", vec![file_snap("/d0/f.txt", 200, None)]),
        ]);
        manager.backend = Some(backend.clone());
        let cancel = Arc::new(AtomicBool::new(true));
        let report = manager
            .reconcile_incremental(&AutoSyncParams::default(), &cancel)
            .await
            .unwrap();
        assert!(report.preempted);
        assert_eq!(report.drilled, 0, "抢占在目录列举边界生效");
        // 抢占不算失败：不记连续失败、不暂停。
        let status = manager.status().unwrap();
        assert_eq!(status.consecutive_failures, 0);
        assert_eq!(status.auto_sync_state, "ok");
    }

    #[tokio::test]
    async fn incremental_clean_tree_writes_nothing() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![
            (
                "/",
                vec![dir_snap("/docs", 10), file_snap("/a.txt", 11, Some("aaa"))],
            ),
            ("/docs", vec![file_snap("/docs/b.txt", 12, None)]),
        ]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        let dump_before = dump_index(&manager);
        let report = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!((report.added, report.changed, report.removed), (0, 0, 0));
        assert_eq!(
            dump_index(&manager),
            dump_before,
            "干净树的增量层不得写索引"
        );
    }

    #[tokio::test]
    async fn incremental_failures_backoff_then_pause_then_manual_resume() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![]);
        backend.fail_list.store(true, Ordering::SeqCst);
        manager.backend = Some(backend.clone());
        let params = AutoSyncParams::default();
        let cancel = Arc::new(AtomicBool::new(false));
        for expect in 1..=4u64 {
            let err = manager
                .reconcile_incremental(&params, &cancel)
                .await
                .unwrap_err();
            let status = manager.status().unwrap();
            assert_eq!(status.consecutive_failures, expect, "{err}");
            assert_eq!(status.auto_sync_state, "backing_off");
            assert!(status.last_error.is_some());
        }
        // 第 5 次失败 → paused；之后层直接 skipped，不再调用后端。
        let _ = manager
            .reconcile_incremental(&params, &cancel)
            .await
            .unwrap_err();
        let status = manager.status().unwrap();
        assert_eq!(status.auto_sync_state, "paused");
        assert_eq!(status.consecutive_failures, 5);
        backend.list_calls.lock().unwrap().clear();
        let report = manager
            .reconcile_incremental(&params, &cancel)
            .await
            .unwrap();
        assert_eq!(report.skipped, Some("paused"));
        assert!(backend.list_calls.lock().unwrap().is_empty());
        // 手动「立即对账」成功 ⇒ 恢复（ADR-111 §2.3）。
        backend.fail_list.store(false, Ordering::SeqCst);
        manager.sync_now().await.unwrap();
        let status = manager.status().unwrap();
        assert_eq!(status.auto_sync_state, "ok");
        assert_eq!(status.consecutive_failures, 0);
    }

    #[test]
    fn list_call_cost_charges_pagination() {
        assert_eq!(list_call_cost(0), 1);
        assert_eq!(list_call_cost(999), 1);
        assert_eq!(list_call_cost(1000), 2, "整页会多一次空页请求");
        assert_eq!(list_call_cost(1001), 2);
        assert_eq!(list_call_cost(2000), 3);
    }

    #[test]
    fn remote_content_changed_md5_fallback_rules() {
        // 无基线 ⇒ 变。
        assert!(remote_content_changed(None, 5, &Some("a".into()), Some(1)));
        // size 不同 ⇒ 变（其余相同也挡不住）。
        assert!(remote_content_changed(
            Some((3, Some("a".into()), Some(1))),
            5,
            &Some("a".into()),
            Some(1)
        ));
        // 双侧 md5 ⇒ 比 md5（mtime 不同也不误判）。
        assert!(!remote_content_changed(
            Some((5, Some("a".into()), Some(1))),
            5,
            &Some("a".into()),
            Some(2)
        ));
        assert!(remote_content_changed(
            Some((5, Some("a".into()), Some(1))),
            5,
            &Some("b".into()),
            Some(1)
        ));
        // 缺任一侧 md5 ⇒ 退化 mtime（ADR-111 定稿）。
        assert!(!remote_content_changed(
            Some((5, None, Some(1))),
            5,
            &Some("b".into()),
            Some(1)
        ));
        assert!(remote_content_changed(
            Some((5, None, Some(1))),
            5,
            &Some("b".into()),
            Some(2)
        ));
    }

    // ---------- S2 上传方向（ADR-112）：drain_upload_queue ----------

    /// 把条目的 pending 时间改到防抖窗口之外。
    fn make_pending_old(manager: &NativeSyncManager, fs_id: i64, op: &str) {
        manager
            .db
            .set_pending_op(fs_id, Some(op), Some("1000000000"))
            .unwrap();
    }

    #[tokio::test]
    async fn upload_new_local_file_rekeys_and_writes_baseline() {
        let (_dir, root, calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        std::fs::write(root.join("n.txt"), b"hello").unwrap();
        manager
            .db
            .insert_local_only_entry(-5, "n.txt", "n.txt", false, 5)
            .unwrap();
        make_pending_old(&manager, -5, "upload");
        let backend = backend_with(vec![]);
        backend.script_upload(vec![Ok(pandock_core::cloud::UploadOutcome {
            fs_id: Some(4242),
            md5: Some("5d41402abc4b2a76b9719d911017c592".into()),
            size: 5,
            mtime: Some(1_700_000_123),
        })]);
        manager.backend = Some(backend.clone());
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(
            report.uploaded,
            1,
            "report={report:?} queue={:?}",
            manager.db.upload_queue()
        );
        assert_eq!(
            backend.uploads.lock().unwrap().as_slice(),
            &[("/n.txt".to_string(), 5u64, 0u32)],
            "local_only 新建必须 rtype=0"
        );
        // rekey：真实 fs_id 落库，hydrated、placeholder=0，旧的负数行消失。
        let (real_id, hydration, md5, remote, _, local_only) =
            manager.db.entry_by_relative("n.txt").unwrap().unwrap();
        assert_eq!(real_id, 4242);
        assert_eq!(hydration, "hydrated");
        assert_eq!(md5.as_deref(), Some("5d41402abc4b2a76b9719d911017c592"));
        assert_eq!(remote, "/n.txt");
        assert!(!local_only);
        let placeholder: i64 = manager
            .db
            .conn
            .query_row(
                "SELECT placeholder FROM local_state WHERE fs_id=4242",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(placeholder, 0, "上传成功的本地文件不是占位符");
        assert!(
            manager.db.upload_queue().unwrap().is_empty(),
            "pending 必须清掉"
        );
        {
            let calls = calls.lock().unwrap();
            assert!(
                calls.iter().any(|c| c == "mapping:n.txt:/n.txt:4242"),
                "{calls:?}"
            );
        } // 守卫必须在这里 drop：下面的自动对账还会写平台调用记录（同一把互斥锁）。
          // 回声抑制：下一轮自动对账看到同样的云端事实 ⇒ 零差异。
        backend.replace(vec![(
            "/",
            vec![file_snap(
                "/n.txt",
                4242,
                Some("5d41402abc4b2a76b9719d911017c592"),
            )],
        )]);
        let layer = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(
            (layer.added, layer.changed, layer.removed),
            (0, 0, 0),
            "上传后下一轮对账必须判一致（ADR-112 §3.5）"
        );
    }

    #[tokio::test]
    async fn upload_edit_of_cloud_entry_then_incremental_layer_zero_diff() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/a.txt", 11, Some("aaa"))])]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // 本地编辑（内容变为 5 字节新内容）→ pending upload。
        std::fs::write(root.join("a.txt"), b"loc12").unwrap();
        make_pending_old(&manager, 11, "upload");
        backend.script_upload(vec![Ok(FakeBackend::upload_outcome(11, 5))]);
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.uploaded, 1);
        assert_eq!(
            backend.uploads.lock().unwrap().as_slice(),
            &[("/a.txt".to_string(), 5u64, 3u32)],
            "stat 一致 ⇒ 有基线编辑必须 rtype=3 安全覆盖（ADR-112 §3.3 修订）"
        );
        // 基线回写 create 响应值（md5=deadbeef）。
        let (_, _, md5, remote, ..) = manager.db.entry_by_relative("a.txt").unwrap().unwrap();
        assert_eq!(md5.as_deref(), Some("deadbeef"));
        assert_eq!(remote, "/a.txt");
        assert!(manager.db.upload_queue().unwrap().is_empty());
        // 回声：远端列出的就是刚上传的事实 ⇒ 零差异。
        backend.replace(vec![("/", vec![file_snap("/a.txt", 11, Some("deadbeef"))])]);
        let layer = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!((layer.added, layer.changed, layer.removed), (0, 0, 0));
    }

    #[tokio::test]
    async fn upload_delete_with_baseline_propagates_and_no_echo() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![(
            "/",
            vec![
                file_snap("/a.txt", 11, Some("aaa")),
                file_snap("/b.txt", 12, Some("bbb")),
            ],
        )]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // 用户在 Explorer 删除 a.txt（本地消失 + pending delete）。
        std::fs::remove_file(root.join("a.txt")).unwrap();
        make_pending_old(&manager, 11, "delete");
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.deleted, 1);
        assert_eq!(
            backend.removes.lock().unwrap().as_slice(),
            &["/a.txt".to_string()]
        );
        // 回声：远端也消失了 ⇒ 下一轮对账 removed=0（tombstone 行不参与 missing）。
        backend.replace(vec![("/", vec![file_snap("/b.txt", 12, Some("bbb"))])]);
        let layer = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!((layer.added, layer.changed, layer.removed), (0, 0, 0));
        let page = manager.list(Some("B".into()), None, 50).unwrap();
        assert_eq!(page.total, 0);
    }

    #[tokio::test]
    async fn upload_delete_without_baseline_blocked_no_cloud_call() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        // 本地新建（从未上云）随即删除 ⇒ 无基线，绝不能向云端发 delete（ADR-112 §3.4 护栏）。
        manager
            .db
            .insert_local_only_entry(-7, "ghost.txt", "ghost.txt", false, 3)
            .unwrap();
        make_pending_old(&manager, -7, "delete");
        let backend = backend_with(vec![]);
        manager.backend = Some(backend.clone());
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.skipped, 1);
        assert_eq!(report.deleted, 0);
        assert!(
            backend.removes.lock().unwrap().is_empty(),
            "无基线 ⇒ 零云端调用"
        );
        assert!(manager.db.upload_queue().unwrap().is_empty());
    }

    #[tokio::test]
    async fn upload_rename_uses_move_not_delete_upload_and_updates_remote_path() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/old.txt", 11, Some("aaa"))])]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // 用户改名：本地文件改名 + 索引捕获（apply_local_rename 的线上行为）。
        std::fs::rename(root.join("old.txt"), root.join("new.txt")).unwrap();
        manager.db.apply_local_rename(11, "new.txt").unwrap();
        make_pending_old(&manager, 11, "rename");
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.renamed, 1);
        assert_eq!(
            backend.renames.lock().unwrap().as_slice(),
            &[("/old.txt".to_string(), "/new.txt".to_string())]
        );
        assert!(
            backend.removes.lock().unwrap().is_empty(),
            "改名不得走 delete"
        );
        assert!(
            backend.uploads.lock().unwrap().is_empty(),
            "改名不得走 upload"
        );
        // fs_id 保留（opera=move 的全部意义）；remote_path 已回写。
        let (real_id, _, _, remote, ..) = manager.db.entry_by_relative("new.txt").unwrap().unwrap();
        assert_eq!(real_id, 11);
        assert_eq!(remote, "/new.txt");
        // 回声：远端列出新名 ⇒ 零差异。
        backend.replace(vec![("/", vec![file_snap("/new.txt", 11, Some("aaa"))])]);
        let layer = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!((layer.added, layer.changed, layer.removed), (0, 0, 0));
    }

    #[tokio::test]
    async fn local_only_minus8_same_md5_writes_baseline_without_conflict() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        // 本地新建 n.txt，云端恰好已有**同内容**同名文件 ⇒ rtype=0 吃 -8，
        // stat 比 md5 一致 ⇒ 双方已一致，回写基线，不走冲突（ADR-112 §3.3 修订）。
        std::fs::write(root.join("n.txt"), b"hello").unwrap();
        manager
            .db
            .insert_local_only_entry(-5, "n.txt", "n.txt", false, 5)
            .unwrap();
        make_pending_old(&manager, -5, "upload");
        let backend = backend_with(vec![(
            "/",
            vec![file_snap(
                "/n.txt",
                11,
                Some("5d41402abc4b2a76b9719d911017c592"),
            )],
        )]);
        let mut conflict = CloudError::already_exists("百度 API 错误：{\"errno\":-8}");
        conflict.raw_errno = Some(-8);
        backend.script_upload(vec![Err(conflict)]);
        manager.backend = Some(backend.clone());
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.uploaded, 1, "md5 一致 ⇒ 双方已一致，不走冲突");
        assert_eq!(report.conflicts, 0);
        // 没有改名发生：本地文件保持原名。
        assert!(root.join("n.txt").exists());
        // rekey 到云端真身（stat 回写），pending 清空。
        let (real_id, _, _, remote, _, local_only) =
            manager.db.entry_by_relative("n.txt").unwrap().unwrap();
        assert_eq!(real_id, 11);
        assert_eq!(remote, "/n.txt");
        assert!(!local_only);
        assert!(manager.db.upload_queue().unwrap().is_empty());
    }

    #[tokio::test]
    async fn baseline_edit_stat_md5_differs_goes_conflict_without_upload() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/a.txt", 11, Some("aaa"))])]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // 本地编辑（pending upload）；期间云端被别人改了（md5 从 aaa 变成 cloud-v2）。
        std::fs::write(root.join("a.txt"), b"mine!").unwrap();
        make_pending_old(&manager, 11, "upload");
        backend.replace(vec![("/", vec![file_snap("/a.txt", 11, Some("cloud-v2"))])]);
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.conflicts, 1);
        // stat 比对直接判冲突：原路径**没有**发起上传（上传反而可能覆盖他人版本）。
        let uploads = backend.uploads.lock().unwrap().clone();
        assert_eq!(uploads.len(), 1, "只有副本回传这一次上传: {uploads:?}");
        let copy_name =
            crate::upload::conflict_copy_name("a.txt", &crate::upload::device_name(), 1);
        assert_eq!(uploads[0], (format!("/{copy_name}"), 5u64, 0u32));
        // OneDrive 语义：云端版本占原名，本地副本改名保留。
        assert!(root.join(&copy_name).exists());
        assert!(!root.join("a.txt").exists());
        let (_, state, ..) = manager.db.entry_by_relative("a.txt").unwrap().unwrap();
        assert_eq!(state, "error", "原名条目进「需处理」");
        let (copy_id, copy_state, ..) = manager.db.entry_by_relative(&copy_name).unwrap().unwrap();
        assert!(copy_id > 0);
        assert_eq!(copy_state, "hydrated");
        assert!(manager.db.upload_queue().unwrap().is_empty());
    }

    #[tokio::test]
    async fn baseline_edit_rtype3_minus8_race_falls_back_to_conflict() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/a.txt", 11, Some("aaa"))])]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        std::fs::write(root.join("a.txt"), b"mine!").unwrap();
        make_pending_old(&manager, 11, "upload");
        // stat 一致 ⇒ rtype=3 上传；create 仍 -8（stat 之后云端被并发改）⇒ 兜底进冲突。
        let mut race = CloudError::already_exists("百度 API 错误：{\"errno\":-8}");
        race.raw_errno = Some(-8);
        backend.script_upload(vec![Err(race), Ok(FakeBackend::upload_outcome(7777, 5))]);
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.conflicts, 1);
        let copy_name =
            crate::upload::conflict_copy_name("a.txt", &crate::upload::device_name(), 1);
        let uploads = backend.uploads.lock().unwrap().clone();
        assert_eq!(
            uploads.as_slice(),
            &[
                ("/a.txt".to_string(), 5u64, 3u32),
                (format!("/{copy_name}"), 5u64, 0u32),
            ],
            "rtype=3 尝试 → -8 兜底 → 副本 rtype=0 回传: {uploads:?}"
        );
        assert!(root.join(&copy_name).exists());
    }

    #[tokio::test]
    async fn local_only_conflict_tombstones_original_row_and_copy_reuploads() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        // 本地新建 a.txt，云端恰好已有同名文件（无基线，local_only）⇒ 上传吃 -8。
        std::fs::write(root.join("a.txt"), b"mine!").unwrap();
        manager
            .db
            .insert_local_only_entry(-5, "a.txt", "a.txt", false, 5)
            .unwrap();
        make_pending_old(&manager, -5, "upload");
        let backend = backend_with(vec![("/", vec![file_snap("/a.txt", 11, Some("aaa"))])]);
        let mut conflict = CloudError::already_exists("百度 API 错误：{\"errno\":-8}");
        conflict.raw_errno = Some(-8);
        backend.script_upload(vec![
            Err(conflict),
            Ok(FakeBackend::upload_outcome(7777, 5)),
        ]);
        manager.backend = Some(backend.clone());
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.conflicts, 1);
        // 副本改名保留并回传。
        let copy_name =
            crate::upload::conflict_copy_name("a.txt", &crate::upload::device_name(), 1);
        assert!(root.join(&copy_name).exists());
        assert_eq!(
            backend.uploads.lock().unwrap()[1].0,
            format!("/{copy_name}")
        );
        // 原路径的 local_only 行已 tombstone（不是留在索引里的幽灵行）。
        let tombstoned: i64 = manager
            .db
            .conn
            .query_row(
                "SELECT tombstone FROM remote_entry WHERE fs_id=-5",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tombstoned, 1, "local_only 原名行必须 tombstone");
        // 下一轮对账：云端版本落到原名 ⇒ 同路径只剩云端真身一行。
        let layer = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(layer.added, 1, "云端版本作为新条目落到原名");
        let (real_id, ..) = manager.db.entry_by_relative("a.txt").unwrap().unwrap();
        assert_eq!(real_id, 11, "原名路径现在属于云端真身");
        let rows: i64 = manager
            .db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM local_state l JOIN remote_entry r ON r.fs_id=l.fs_id WHERE l.relative_path='a.txt' AND r.tombstone=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "同路径不得有幽灵行 + 真身两行");
        assert!(manager.db.upload_queue().unwrap().is_empty());
    }

    #[tokio::test]
    async fn conflict_copy_name_collision_appends_counter_and_never_recurses() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/a.txt", 11, Some("aaa"))])]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        std::fs::write(root.join("a.txt"), b"mine!").unwrap();
        make_pending_old(&manager, 11, "upload");
        let minus8 = || {
            let mut e = CloudError::already_exists("百度 API 错误：{\"errno\":-8}");
            e.raw_errno = Some(-8);
            e
        };
        // 原名 -8；副本 -8；序号副本仍 -8 ⇒ 终止：留本地，不再重传，不递归冲突。
        backend.script_upload(vec![Err(minus8()), Err(minus8()), Err(minus8())]);
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.conflicts, 1);
        let device = crate::upload::device_name();
        let final_name = crate::upload::conflict_copy_name("a.txt", &device, 2);
        assert!(
            root.join(&final_name).exists(),
            "序号副本保留在本地: {final_name}"
        );
        // 三次上传调用后停手（原名 1 + 副本 1 + 序号副本 1），绝不递归冲突流程。
        let uploads = backend.uploads.lock().unwrap().clone();
        assert_eq!(
            uploads.len(),
            3,
            "改名循环终止：最多原名+副本+序号副本各一次: {uploads:?}"
        );
        assert_eq!(uploads[2].0, format!("/{final_name}"));
        assert_eq!(uploads[0].2, 3, "有基线编辑首次尝试 rtype=3");
        assert_eq!(uploads[1].2, 0, "冲突副本回传 rtype=0");
        assert!(
            manager.db.upload_queue().unwrap().is_empty(),
            "不再自动重传 ⇒ 无 pending"
        );
    }

    #[tokio::test]
    async fn upload_debounce_defers_recent_pending() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        std::fs::write(root.join("n.txt"), b"hello").unwrap();
        manager
            .db
            .insert_local_only_entry(-5, "n.txt", "n.txt", false, 5)
            .unwrap();
        // insert_local_only_entry 的 pending_since = 现在 ⇒ 防抖窗口内。
        let backend = backend_with(vec![]);
        manager.backend = Some(backend.clone());
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.deferred, 1);
        assert_eq!(report.uploaded, 0);
        assert!(
            backend.uploads.lock().unwrap().is_empty(),
            "防抖窗口内不上传"
        );
        // 窗口过去后继续。
        make_pending_old(&manager, -5, "upload");
        let report = manager
            .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.uploaded, 1);
    }

    #[tokio::test]
    async fn upload_failure_counts_then_errors_at_threshold() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        manager
            .db
            .insert_local_only_entry(-5, "a.txt", "a.txt", false, 5)
            .unwrap();
        make_pending_old(&manager, -5, "upload");
        let backend = backend_with(vec![]);
        backend.script_upload(vec![
            Err(CloudError::transient("net down")),
            Err(CloudError::transient("net down")),
            Err(CloudError::transient("net down")),
            Err(CloudError::transient("net down")),
            Err(CloudError::transient("net down")),
        ]);
        manager.backend = Some(backend.clone());
        for round in 1..=5u32 {
            let report = manager
                .drain_upload_queue(&UploadParams::default(), &Arc::new(AtomicBool::new(false)))
                .await
                .unwrap();
            assert_eq!(report.failed, 1, "第 {round} 轮");
            assert_eq!(report.uploaded, 0);
        }
        assert_eq!(backend.uploads.lock().unwrap().len(), 5);
        // 达到阈值（5 次）⇒ 清 pending 不再自动重试（local_only 行不进 error 状态，仅清 pending）。
        assert!(
            manager.db.upload_queue().unwrap().is_empty(),
            "≥5 次后不得再重试"
        );
    }

    #[test]
    fn upload_queue_orders_parents_before_children() {
        let (_dir, root, _calls, manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        manager
            .db
            .insert_local_only_entry(-1, "zdir", "zdir", true, 0)
            .unwrap();
        manager
            .db
            .insert_local_only_entry(-2, "zdir/f.txt", "f.txt", false, 3)
            .unwrap();
        manager
            .db
            .insert_local_only_entry(-3, "a.txt", "a.txt", false, 3)
            .unwrap();
        let queue = manager.db.upload_queue().unwrap();
        let relatives: Vec<&str> = queue.iter().map(|row| row.1.as_str()).collect();
        assert_eq!(
            relatives,
            vec!["a.txt", "zdir", "zdir/f.txt"],
            "父目录必须先于其子项"
        );
    }

    // ---------- PR-3a：本地侧对账捕获（Gap A/B/C + md5 漏斗） ----------

    #[tokio::test]
    async fn local_scan_detects_new_file_and_marks_pending_upload() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        std::fs::write(root.join("new.txt"), b"brand new").unwrap();
        let mut report = AutoLayerReport::default();
        manager.scan_local_dir(&root, "", &mut report).unwrap();
        assert_eq!(report.local_new, 1);
        let (fs_id, _, _, remote, _, local_only) =
            manager.db.entry_by_relative("new.txt").unwrap().unwrap();
        assert!(
            fs_id < 0 && local_only,
            "本地新建必须是负数 fs_id 的 local_only 行"
        );
        assert_eq!(remote, "local:new.txt");
        assert_eq!(
            manager.db.pending_op_of(fs_id).unwrap().as_deref(),
            Some("upload"),
            "新建即待上传（worker 沿用 3s 防抖）"
        );
    }

    #[tokio::test]
    async fn local_scan_skips_indexed_cloud_placeholder() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/a.txt", 11, Some("aaa"))])]);
        manager.backend = Some(backend);
        manager.sync_now().await.unwrap();
        // 占位符在磁盘上（online_only）：本地侧扫描不得把它当新建或编辑。
        let mut report = AutoLayerReport::default();
        manager.scan_local_dir(&root, "", &mut report).unwrap();
        assert_eq!((report.local_new, report.local_edited), (0, 0));
        assert!(manager.db.upload_queue().unwrap().is_empty());
        // 竞态保险：索引缺失但磁盘是云占位符（属性判定）也不得误捕。
        manager.db.tombstone(11).unwrap();
        manager
            .platform
            .create_placeholder(&root.join("ghost.txt"), false, 0, &99u64.to_le_bytes())
            .unwrap();
        let mut report = AutoLayerReport::default();
        manager.scan_local_dir(&root, "", &mut report).unwrap();
        assert_eq!(report.local_new, 0, "云占位符属性命中 ⇒ 跳过");
        assert!(manager.db.entry_by_relative("ghost.txt").unwrap().is_none());
    }

    #[tokio::test]
    async fn local_scan_skips_local_only_already_captured() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        std::fs::write(root.join("n.txt"), b"hello").unwrap();
        manager
            .db
            .insert_local_only_entry(-5, "n.txt", "n.txt", false, 5)
            .unwrap();
        let mut report = AutoLayerReport::default();
        manager.scan_local_dir(&root, "", &mut report).unwrap();
        assert_eq!(report.local_new, 0, "已在索引的 local_only 不重复捕获");
        let rows: i64 = manager
            .db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM remote_entry WHERE local_only=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn local_scan_records_baseline_when_snapshot_missing() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        let mut rec = record(11, "a.txt", Some("aaa"));
        rec.state = NativeSyncState::Hydrated;
        rec.size = 5;
        manager.db.upsert(&rec).unwrap();
        // 快照为 NULL（老库行）⇒ 只补记基线，绝不判修改（ADR-109「读≠改」）。
        let mut report = AutoLayerReport::default();
        manager.scan_local_dir(&root, "", &mut report).unwrap();
        assert_eq!(report.local_edited, 0);
        assert!(manager.db.pending_op_of(11).unwrap().is_none());
        let snap = manager.db.snapshot_for(11).unwrap().unwrap();
        assert_eq!(snap.size, Some(5), "基线已被补记");
    }

    #[tokio::test]
    async fn local_scan_md5_funnel_mtime_only_change_updates_baseline_without_pending() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        // 基线 md5 与本地内容一致（只是 mtime 会被碰）：md5 漏斗必须放行。
        let mut rec = record(11, "a.txt", Some("5d41402abc4b2a76b9719d911017c592"));
        rec.state = NativeSyncState::Hydrated;
        rec.size = 5;
        manager.db.upsert(&rec).unwrap();
        manager.db.set_snapshot(11, Some(1_000_000), 5).unwrap();
        let before = manager.db.snapshot_for(11).unwrap().unwrap();
        // 同内容重写 ⇒ mtime 变化、size/md5 不变。
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        let mut report = AutoLayerReport::default();
        manager.scan_local_dir(&root, "", &mut report).unwrap();
        assert_eq!(report.local_edited, 0, "md5 一致 ⇒ 不算编辑");
        assert!(manager.db.pending_op_of(11).unwrap().is_none());
        let after = manager.db.snapshot_for(11).unwrap().unwrap();
        assert_eq!(after.size, Some(5));
        assert_ne!(after.mtime, before.mtime, "基线已更新到新 mtime");
    }

    #[tokio::test]
    async fn local_scan_marks_edit_when_content_differs() {
        let (_dir, root, calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        std::fs::write(root.join("a.txt"), b"world!").unwrap();
        let mut rec = record(11, "a.txt", Some("aaa"));
        rec.state = NativeSyncState::Hydrated;
        rec.size = 5;
        manager.db.upsert(&rec).unwrap();
        manager.db.set_snapshot(11, Some(1_000_000), 5).unwrap();
        let mut report = AutoLayerReport::default();
        manager.scan_local_dir(&root, "", &mut report).unwrap();
        assert_eq!(report.local_edited, 1);
        assert_eq!(
            manager.db.pending_op_of(11).unwrap().as_deref(),
            Some("upload")
        );
        let calls = calls.lock().unwrap();
        assert!(
            calls.iter().any(|c| c.starts_with("mark_not_in_sync:")),
            "未上云期间必须防脱水: {calls:?}"
        );
    }

    #[tokio::test]
    async fn local_scan_runs_within_auto_layer_root() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![])]);
        manager.backend = Some(backend);
        std::fs::write(root.join("stray.txt"), b"i am new here").unwrap();
        let report = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(report.local_new, 1, "层内本地侧对账必须捕获新建");
        let (fs_id, _, _, _, _, local_only) =
            manager.db.entry_by_relative("stray.txt").unwrap().unwrap();
        assert!(fs_id < 0 && local_only);
        assert_eq!(manager.db.upload_queue().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn local_scan_covers_deep_scanned_dirs() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![dir_snap("/d", 101)]), ("/d", vec![])]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // 深目录里的本地新建：目录已落位后新出现的文件。
        std::fs::write(root.join("d").join("f.txt"), b"deep new").unwrap();
        let report = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert!(report.deep_scanned >= 1);
        assert_eq!(report.local_new, 1, "交错深扫目录的本地侧对账同样生效");
        let (fs_id, _, _, _, _, local_only) =
            manager.db.entry_by_relative("d/f.txt").unwrap().unwrap();
        assert!(fs_id < 0 && local_only);
    }

    #[tokio::test]
    async fn conflict_copy_with_matching_index_row_is_not_recaptured() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        // 冲突副本：已落库（hydrated、placeholder=0）且快照与磁盘一致 ⇒ 自杀防护靠索引判定。
        std::fs::write(root.join("a-PC.txt"), b"conflict copy").unwrap();
        let rec = EntryRecord {
            fs_id: 7777,
            local_only: false,
            parent_fs_id: None,
            remote_path: "/a-PC.txt".into(),
            relative_path: "a-PC.txt".into(),
            name: "a-PC.txt".into(),
            is_dir: false,
            size: 13,
            md5: Some("ccc".into()),
            modified_at: Some(1_700_000_000),
            state: NativeSyncState::Hydrated,
            pin_state: PinState::Unpinned,
            error: None,
            pending_op: None,
            pending_since: None,
            has_children: false,
        };
        manager.db.insert_uploaded_entry(&rec).unwrap();
        let meta = std::fs::metadata(root.join("a-PC.txt")).unwrap();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        manager.db.set_snapshot(7777, mtime, meta.len()).unwrap();
        let mut report = AutoLayerReport::default();
        manager.scan_local_dir(&root, "", &mut report).unwrap();
        assert_eq!(
            (report.local_new, report.local_edited),
            (0, 0),
            "冲突副本不得被误捕"
        );
        assert!(manager.db.upload_queue().unwrap().is_empty());
    }

    /// 排序缺陷的端到端回归（2026-10-13 真实机实证）：
    /// scan 必须先于 ingest——否则 ingest 的 set_snapshot 会先把基线刷成磁盘现状，
    /// 编辑证据在比对前被擦除（先 ingest 时本测试 local_edited 恒 0）。
    #[tokio::test]
    async fn local_scan_runs_before_ingest_so_layer_captures_edit() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/a.txt", 11, Some("aaa"))])]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // 模拟已水合且有基线：写内容 + 状态 hydrated + 快照一致。
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        manager.db.set_state_by_fs_id(11, "hydrated", None).unwrap();
        let meta = std::fs::metadata(root.join("a.txt")).unwrap();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        manager.db.set_snapshot(11, mtime, meta.len()).unwrap();
        // 本地编辑（内容变为 6 字节）。
        std::fs::write(root.join("a.txt"), b"world!").unwrap();
        let report = manager
            .reconcile_incremental(
                &AutoSyncParams::default(),
                &Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(
            report.local_edited, 1,
            "scan 必须先于 ingest，否则编辑证据被快照刷新擦除: {report:?}"
        );
        assert_eq!(
            manager.db.pending_op_of(11).unwrap().as_deref(),
            Some("upload"),
            "编辑必须进入上传队列"
        );
    }

    // ---------- 陈旧 listing 守卫（list 管广度、stat 管生杀） ----------

    fn tombstone_of(manager: &NativeSyncManager, fs_id: i64) -> i64 {
        manager
            .db
            .conn
            .query_row(
                "SELECT tombstone FROM remote_entry WHERE fs_id=?1",
                rusqlite::params![fs_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn missing_candidate_alive_by_stat_is_kept() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![(
            "/",
            vec![file_snap("/a.txt", 11, Some("aaa")), file_snap("/b.txt", 12, Some("bbb"))],
        )]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // listing 滞后：a.txt 从 listing 消失；但 stat（新鲜路径）确认它还在。
        backend.replace(vec![("/", vec![file_snap("/b.txt", 12, Some("bbb"))])]);
        backend
            .stat_listings
            .lock()
            .unwrap()
            .insert("/a.txt".to_string(), file_snap("/a.txt", 11, Some("aaa")));
        let report = manager
            .reconcile_incremental(&AutoSyncParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.removed, 0, "stat 确认存活 ⇒ 不得删除: {report:?}");
        assert_eq!(tombstone_of(&manager, 11), 0, "a.txt 不得 tombstone");
        assert_eq!(backend.stats_calls.lock().unwrap().as_slice(), &[1], "批量 stat 恰好一次");
        let (_, state, ..) = manager.db.entry_by_relative("a.txt").unwrap().unwrap();
        assert_eq!(state, "online_only", "a.txt 状态不受陈旧 listing 影响");
    }

    #[tokio::test]
    async fn missing_candidate_confirmed_gone_is_processed() {
        let (_dir, root, calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![(
            "/",
            vec![file_snap("/a.txt", 11, Some("aaa")), file_snap("/b.txt", 12, Some("bbb"))],
        )]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // stat 也查无（stat_listings 默认空）⇒ 真删除，照原流程 tombstone + 移除占位符。
        backend.replace(vec![("/", vec![file_snap("/b.txt", 12, Some("bbb"))])]);
        let report = manager
            .reconcile_incremental(&AutoSyncParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(tombstone_of(&manager, 11), 1);
        let calls = calls.lock().unwrap();
        assert!(calls.iter().any(|c| c.starts_with("remove:")), "{calls:?}");
    }

    #[tokio::test]
    async fn recently_uploaded_entry_skips_missing_grace_and_stat() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![(
            "/",
            vec![file_snap("/a.txt", 11, Some("aaa")), file_snap("/b.txt", 12, Some("bbb"))],
        )]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // a.txt 是我们刚上传的（uploaded_at < 30min）⇒ 宽限：不进候选、不调 stat。
        manager.db.mark_uploaded(11).unwrap();
        backend.replace(vec![("/", vec![file_snap("/b.txt", 12, Some("bbb"))])]);
        let report = manager
            .reconcile_incremental(&AutoSyncParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(tombstone_of(&manager, 11), 0);
        assert!(
            backend.stats_calls.lock().unwrap().is_empty(),
            "宽限条目不得产生 stat 调用"
        );
    }

    #[tokio::test]
    async fn missing_candidates_over_50_skip_all_actions() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let mut seed = Vec::new();
        for i in 0..51u64 {
            seed.push(file_snap(&format!("/f{i}.txt"), 1000 + i, Some("x")));
        }
        let backend = backend_with(vec![("/", seed)]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // 51 个候选（>50 闸门）⇒ 不是滞后是事故：本层不做任何 missing 动作、不打 API。
        backend.replace(vec![("/", vec![])]);
        let report = manager
            .reconcile_incremental(&AutoSyncParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        assert_eq!(report.removed, 0, "闸门触发 ⇒ 零删除: {report:?}");
        assert!(backend.stats_calls.lock().unwrap().is_empty(), "闸门触发 ⇒ 零 stat 调用");
        assert_eq!(tombstone_of(&manager, 1000), 0);
    }

    /// H1 扩展的层内端到端：pending upload 条目在层内**不被收编基线**——
    /// worker 的 stat 才能看见「云端已越过最后一致版本」并触发冲突（finding #3 防御）。
    #[tokio::test]
    async fn pending_upload_entry_keeps_baseline_through_layer() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/a.txt", 11, Some("aaa"))])]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        // 本地编辑已捕获（pending upload）；期间云端被别人改成 v2changed。
        std::fs::write(root.join("a.txt"), b"mine!").unwrap();
        make_pending_old(&manager, 11, "upload");
        backend.replace(vec![("/", vec![file_snap("/a.txt", 11, Some("v2changed"))])]);
        let report = manager
            .reconcile_incremental(&AutoSyncParams::default(), &Arc::new(AtomicBool::new(false)))
            .await
            .unwrap();
        let (_, baseline_md5, _) = manager.db.remote_meta_by_fs_id(11).unwrap().unwrap();
        assert_eq!(
            baseline_md5.as_deref(),
            Some("aaa"),
            "pending upload 条目的基线必须保留为最后一致版本（不得被 ingest 收编成 v2changed）"
        );
        assert_eq!(
            manager.db.pending_op_of(11).unwrap().as_deref(),
            Some("upload"),
            "pending 必须留给 worker 处理"
        );
        assert_eq!(report.changed, 0, "pending 条目的云端变化不得计入本层变更");
    }

    #[tokio::test]
    async fn sync_now_rejects_entries_without_fs_id() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        manager.backend = Some(backend_with(vec![(
            "/",
            vec![ResourceSnapshot::synthetic_dir(
                RemotePath::parse("/d").unwrap(),
            )],
        )]));
        let err = manager.sync_now().await.unwrap_err();
        assert!(err.to_string().contains("fs_id"));
    }

    #[tokio::test]
    async fn sync_now_aborts_when_listing_fails() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/a.txt", 11, None)])]);
        backend.fail_list.store(true, Ordering::SeqCst);
        manager.backend = Some(backend);
        let err = manager.sync_now().await.unwrap_err();
        assert!(err.to_string().contains("百度网盘"));
        assert_eq!(manager.status().unwrap().total_entries, 0);
    }

    #[tokio::test]
    async fn remote_delete_tombstones_online_only_but_preserves_hydrated() {
        let (_dir, root, calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![]);
        manager.backend = Some(backend.clone());
        backend.replace(vec![
            (
                "/",
                vec![
                    dir_snap("/docs", 10),
                    dir_snap("/other", 14),
                    file_snap("/a.txt", 11, Some("aaa")),
                ],
            ),
            ("/docs", vec![file_snap("/docs/b.txt", 12, None)]),
            ("/other", vec![file_snap("/other/c.txt", 13, None)]),
        ]);
        manager.sync_now().await.unwrap();
        // a.txt and docs/ become hydrated locally before the remote deletion.
        manager.db.set_state_by_fs_id(11, "hydrated", None).unwrap();
        manager.db.set_state_by_fs_id(10, "hydrated", None).unwrap();
        // Remote now deletes a.txt and the whole docs/ tree.
        backend.replace(vec![("/", vec![dir_snap("/other", 14)])]);
        calls.lock().unwrap().clear();
        let status = manager.sync_now().await.unwrap();
        assert_eq!(status.total_entries, 3);
        assert_eq!(
            manager.db.entry_by_relative("a.txt").unwrap().unwrap().1,
            "remote_missing"
        );
        assert_eq!(
            manager.db.entry_by_relative("docs").unwrap().unwrap().1,
            "remote_missing"
        );
        assert!(manager
            .db
            .entry_by_relative("docs/b.txt")
            .unwrap()
            .is_none());
        let calls = calls.lock().unwrap();
        assert!(!calls
            .iter()
            .any(|c| c.contains("a.txt") && c.starts_with("remove:")));
        assert!(calls
            .iter()
            .any(|c| c.contains("b.txt") && c.starts_with("remove:")));
        let removed_dirs = calls
            .iter()
            .filter(|c| c.starts_with("remove:") && c.ends_with(":dir=true"))
            .count();
        assert_eq!(removed_dirs, 0);
    }

    #[test]
    fn list_paginates_and_reports_running_job_progress() {
        let (_dir, _root, _calls, manager) = setup();
        for id in 1..=3 {
            manager
                .db
                .upsert(&record(id, &format!("f{id}"), None))
                .unwrap();
        }
        let page1 = manager.list(None, None, 2).unwrap();
        assert_eq!(page1.entries.len(), 2);
        assert_eq!(page1.total, 3);
        assert_eq!(page1.next_cursor.as_deref(), Some("2"));
        let page2 = manager.list(None, page1.next_cursor, 2).unwrap();
        assert_eq!(page2.entries.len(), 1);
        assert_eq!(page2.next_cursor, None);
        let first = page1.entries.iter().find(|e| e.fs_id == Some(1)).unwrap();
        assert_eq!(first.modified_at, Some(123));
        let job = manager.db.hydration_started(1, 100).unwrap();
        manager.db.hydration_progress(job, 40).unwrap();
        let page = manager.list(None, None, 10).unwrap();
        let first = page.entries.iter().find(|e| e.fs_id == Some(1)).unwrap();
        assert_eq!(
            first.progress.as_ref().map(|p| (p.completed, p.total)),
            Some((40, 100))
        );
        manager
            .db
            .hydration_finished(1, job, "online_only", None)
            .unwrap();
        let page = manager.list(None, None, 10).unwrap();
        assert!(page
            .entries
            .iter()
            .find(|e| e.fs_id == Some(1))
            .unwrap()
            .progress
            .is_none());
    }

    #[test]
    fn pin_unpin_update_state_and_platform() {
        let (_dir, root, calls, mut manager) = setup();
        manager.db.upsert(&record(5, "a.txt", None)).unwrap();
        manager.db.set_root(&root, true, "connected").unwrap();
        manager
            .action(NativeSyncAction {
                relative_path: "a.txt".into(),
                action: EntryAction::Pin,
            })
            .unwrap();
        assert_eq!(pin_state_of(&manager, 5), "pinned");
        assert!(calls.lock().unwrap().contains(&"pin:true".to_string()));
        manager
            .action(NativeSyncAction {
                relative_path: "a.txt".into(),
                action: EntryAction::Unpin,
            })
            .unwrap();
        assert_eq!(pin_state_of(&manager, 5), "unpinned");
        assert!(calls.lock().unwrap().contains(&"pin:false".to_string()));
        let err = manager
            .action(NativeSyncAction {
                relative_path: "missing.txt".into(),
                action: EntryAction::Pin,
            })
            .unwrap_err();
        assert!(err.to_string().contains("找不到"));
    }

    #[test]
    fn dehydrate_enforces_hydrated_state_and_md5_match() {
        let (_dir, root, calls, mut manager) = setup();
        let content = b"hello";
        let digest = format!("{:x}", md5::compute(content));
        manager
            .db
            .upsert(&record(7, "a.txt", Some(&digest)))
            .unwrap();
        manager.db.set_root(&root, true, "connected").unwrap();
        let err = manager
            .action(NativeSyncAction {
                relative_path: "a.txt".into(),
                action: EntryAction::Dehydrate,
            })
            .unwrap_err();
        assert!(err.to_string().contains("已完整下载"));
        manager.db.set_state_by_fs_id(7, "hydrated", None).unwrap();
        std::fs::write(root.join("a.txt"), b"bad").unwrap();
        let err = manager
            .action(NativeSyncAction {
                relative_path: "a.txt".into(),
                action: EntryAction::Dehydrate,
            })
            .unwrap_err();
        assert!(err.to_string().contains("md5"));
        std::fs::write(root.join("a.txt"), content).unwrap();
        manager
            .action(NativeSyncAction {
                relative_path: "a.txt".into(),
                action: EntryAction::Dehydrate,
            })
            .unwrap();
        let calls = calls.lock().unwrap();
        assert!(calls.contains(&"mark_in_sync".to_string()));
        assert!(calls.iter().any(|c| c.starts_with("dehydrate:")));
        drop(calls);
        assert_eq!(
            manager.db.entry_by_relative("a.txt").unwrap().unwrap().1,
            "online_only"
        );
    }

    #[test]
    fn retry_clears_error_state() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.upsert(&record(8, "a.txt", None)).unwrap();
        manager.db.set_root(&root, true, "connected").unwrap();
        manager
            .db
            .set_state_by_fs_id(8, "error", Some("boom"))
            .unwrap();
        manager
            .action(NativeSyncAction {
                relative_path: "a.txt".into(),
                action: EntryAction::Retry,
            })
            .unwrap();
        let entry = manager.db.entry_by_relative("a.txt").unwrap().unwrap();
        assert_eq!(entry.1, "online_only");
        let last_error: Option<String> = manager
            .db
            .conn
            .query_row(
                "SELECT last_error FROM local_state WHERE fs_id=8",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(last_error, None);
    }

    #[test]
    fn ack_remote_missing_requires_state_and_only_records_time() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.upsert(&record(9, "a.txt", None)).unwrap();
        manager.db.set_root(&root, true, "connected").unwrap();
        let err = manager
            .action(NativeSyncAction {
                relative_path: "a.txt".into(),
                action: EntryAction::AckRemoteMissing,
            })
            .unwrap_err();
        assert!(err.to_string().contains("远端删除"));
        manager
            .db
            .set_state_by_fs_id(9, "remote_missing", Some("gone"))
            .unwrap();
        manager
            .action(NativeSyncAction {
                relative_path: "a.txt".into(),
                action: EntryAction::AckRemoteMissing,
            })
            .unwrap();
        let entry = manager.db.entry_by_relative("a.txt").unwrap().unwrap();
        assert_eq!(entry.1, "remote_missing");
        let acknowledged: Option<String> = manager
            .db
            .conn
            .query_row(
                "SELECT remote_missing_ack_at FROM local_state WHERE fs_id=9",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(acknowledged.is_some());
    }

    #[test]
    fn open_path_allows_inside_symlinked_root_and_rejects_escape() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("real-root");
        std::fs::create_dir_all(real.join("d")).unwrap();
        std::fs::write(real.join("d").join("exists.txt"), b"x").unwrap();
        let link = dir.path().join("link-root");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(windows)]
        {
            // junction 无需管理员权限，等价于目录符号链接。
            let output = std::process::Command::new("cmd")
                .args([
                    "/c",
                    "mklink",
                    "/J",
                    &link.to_string_lossy(),
                    &real.to_string_lossy(),
                ])
                .output()
                .unwrap();
            assert!(output.status.success(), "mklink /J failed");
        }
        let manager = NativeSyncManager {
            db: {
                let db = IndexDb::open(dir.path()).unwrap();
                db.set_root(&link, true, "connected").unwrap();
                db
            },
            backend: None,
            platform: Box::new(FakePlatform::default()),
            notifications_config: true,
            notifications_enabled: true,
        };
        // 存在候选：canonicalize 现值。
        let inside_existing = manager.open_path("d/exists.txt").unwrap();
        assert!(inside_existing.ends_with("exists.txt"));
        // 不存在候选：词法归一后必须放行（CI temp 短名/符号链接场景回归）。
        let inside_missing = manager.open_path("d/b.txt").unwrap();
        assert!(inside_missing.ends_with("d/b.txt"));
        // 存在目录祖先 + 不存在文件混合场景。
        let inside_mixed = manager.open_path("d/new/child.txt").unwrap();
        assert!(inside_mixed.ends_with("d/new/child.txt"));
        // 逃逸依旧拒绝。
        assert!(manager.open_path("../x").is_err());
        assert!(manager.open_path("d/../../x").is_err());
        // 根自身的 canonical 形态必须作为前缀基准（link 与 real 不同名）。
        let canonical_root = std::fs::canonicalize(&real).unwrap();
        let canonical_root_text = canonical_root.to_string_lossy();
        let canonical_root_str = canonical_root_text
            .strip_prefix(r"\\?\")
            .unwrap_or(&canonical_root_text)
            .replace('\\', "/");
        for candidate in [&inside_existing, &inside_missing, &inside_mixed] {
            let text = candidate.to_string_lossy().replace('\\', "/");
            assert!(
                text.starts_with(&canonical_root_str),
                "候选必须落在 canonical 根内: {text} vs {canonical_root_str}"
            );
        }
    }
    #[test]
    fn open_path_rejects_escape_and_accepts_inside() {
        let (_dir, root, _calls, manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let inside = manager.open_path("d/b.txt").unwrap();
        // open_path 按契约返回 canonical 路径（macOS /var→/private/var 等），
        // 前缀比较必须以 canonical 根为基准。
        let root_canon = std::fs::canonicalize(&root).unwrap();
        let root_canon_text = root_canon.to_string_lossy();
        let root_canon_str = root_canon_text
            .strip_prefix(r"\\?\")
            .unwrap_or(&root_canon_text);
        let inside_text = inside.to_string_lossy();
        let inside_str = inside_text.strip_prefix(r"\\?\").unwrap_or(&inside_text);
        assert!(inside_str.starts_with(root_canon_str));
        assert!(manager.open_path("../x").is_err());
        assert!(manager.open_path("d/../../x").is_err());
        let absolute = std::fs::canonicalize(&root).unwrap().join("x");
        assert!(manager.open_path(absolute.to_str().unwrap()).is_err());
    }

    /// H1：本地已删除（pending_op=delete）的条目在下次 sync_now 不得被重建。
    /// 没有这条守卫，注册 NOTIFY_DELETE 之后用户删掉的文件会无声复活。
    #[tokio::test]
    async fn sync_now_does_not_revive_locally_deleted_entry() {
        let (_dir, root, calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/a.txt", 11, None)])]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        assert!(root.join("a.txt").exists(), "首次同步应落地占位符");

        // 用户在 Explorer 删除：文件消失 + CF 回调记下待同步删除。
        std::fs::remove_file(root.join("a.txt")).unwrap();
        manager
            .db
            .set_pending_op(11, Some("delete"), Some("2026-10-12T00:00:00Z"))
            .unwrap();
        calls.lock().unwrap().clear();

        // 云端仍列出 a.txt（S1 不上传，云端事实未变）。
        let status = manager.sync_now().await.unwrap();
        let calls = calls.lock().unwrap();
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("placeholder:") && c.contains("a.txt")),
            "已删除的条目不得被重建：{calls:?}"
        );
        assert_eq!(status.pending_entries, 1);
        assert_eq!(status.attention_entries, 1, "待同步删除必须计入 attention");
        assert_eq!(
            manager.db.pending_op_of(11).unwrap().as_deref(),
            Some("delete"),
            "跳过重建时必须保留 pending 标记"
        );
        assert!(manager.db.entry_by_relative("a.txt").unwrap().is_some());
    }

    /// H1：本地改名后不得在下次 sync_now 里把旧路径重新建出来。
    #[tokio::test]
    async fn sync_now_does_not_recreate_old_path_after_local_rename() {
        let (_dir, root, calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/old.txt", 21, None)])]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();

        // 本地改名：磁盘 a→b，回调记下新相对路径与待同步改名。
        std::fs::rename(root.join("old.txt"), root.join("new.txt")).unwrap();
        manager.db.apply_local_rename(21, "new.txt").unwrap();
        calls.lock().unwrap().clear();

        manager.sync_now().await.unwrap();
        let calls = calls.lock().unwrap();
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("placeholder:") && c.contains("old.txt")),
            "旧路径不得被重建：{calls:?}"
        );
        assert!(manager.db.entry_by_relative("old.txt").unwrap().is_none());
        assert!(manager.db.entry_by_relative("new.txt").unwrap().is_some());
        assert_eq!(
            manager.db.pending_op_of(21).unwrap().as_deref(),
            Some("rename")
        );
    }

    /// 本地已不存在的条目在远端删除对账时直接记账，不再打平台删除接口
    /// （对不存在的路径调用会 ENOENT 并中断整轮同步）。
    #[tokio::test]
    async fn sync_now_tombstones_locally_absent_entry_without_platform_call() {
        let (_dir, root, calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let backend = backend_with(vec![("/", vec![file_snap("/a.txt", 31, None)])]);
        manager.backend = Some(backend.clone());
        manager.sync_now().await.unwrap();
        std::fs::remove_file(root.join("a.txt")).unwrap();

        backend.replace(vec![("/", vec![])]);
        calls.lock().unwrap().clear();
        let status = manager.sync_now().await.unwrap();
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("remove:")),
            "本地已不存在时不得再调 remove_placeholder"
        );
        assert_eq!(status.total_entries, 0);
    }

    /// 本地新建条目（云端尚无）不得进入任何云端操作路径：必须显式拒绝，
    /// 而不是拿负数 fs_id 去构造非法云端请求。
    #[test]
    fn local_only_entry_is_rejected_by_every_cloud_action() {
        let (_dir, root, _calls, mut manager) = setup();
        manager.db.set_root(&root, true, "connected").unwrap();
        let fs_id = manager.db.next_local_fs_id().unwrap();
        manager
            .db
            .insert_local_only_entry(fs_id, "新建.txt", "新建.txt", false, 10)
            .unwrap();
        for action in [
            EntryAction::Pin,
            EntryAction::Unpin,
            EntryAction::Dehydrate,
            EntryAction::Retry,
            EntryAction::AckRemoteMissing,
        ] {
            let err = manager
                .action(NativeSyncAction {
                    relative_path: "新建.txt".into(),
                    action: action.clone(),
                })
                .unwrap_err();
            assert!(
                err.to_string().contains("本地新建条目"),
                "{action:?} 必须被显式拒绝，实际: {err}"
            );
        }
        // UI 侧：本地新建条目的 fsId 为 None，不暴露负数内部 id。
        let view = manager.list(None, None, 10).unwrap();
        let entry = view
            .entries
            .iter()
            .find(|e| e.relative_path == "新建.txt")
            .unwrap();
        assert_eq!(entry.fs_id, None);
        assert_eq!(entry.pending_operation.as_deref(), Some("upload"));
    }
}
