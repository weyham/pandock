use crate::backend::CloudBackend;
use crate::db::IndexDb;
use crate::model::{
    EntryAction, NativeSyncAction, NativeSyncEntryView, NativeSyncEvent, NativeSyncListView,
    NativeSyncState, NativeSyncStatusView, PinState,
};
use crate::path::{default_root_path, sanitize_component, validate_root_path};
use crate::platform::{new_platform, PlatformError, PlatformSync};
use pandock_core::cloud::RemotePath;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

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
            last_sync_at: None,
            last_error: None,
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
            for snapshot in entries {
                let fs_id = snapshot
                    .fs_id
                    .ok_or_else(|| NativeSyncError::Message("远端条目缺少 fs_id".into()))?;
                // 云端仍存在：先登记，否则下面的 missing 对账会把它误判成远端删除。
                seen.push(fs_id);
                // H1：本地已删除/改名、尚未同步到云端的条目不得被重建，
                // 否则用户删掉的文件会在下次同步无声复活、改名会多出旧路径副本。
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
                    modified_at: snapshot
                        .modified
                        .duration_since(UNIX_EPOCH)
                        .ok()
                        .map(|x| x.as_secs() as i64),
                    state: NativeSyncState::OnlineOnly,
                    pin_state: PinState::Unpinned,
                    error: None,
                    pending_op: None,
                    pending_since: None,
                    has_children: snapshot.is_dir(),
                };
                self.db.upsert(&record).map_err(NativeSyncError::Message)?;
                self.platform
                    .set_remote_mapping(&relative, snapshot.path.clone(), fs_id);
                self.platform.create_placeholder(
                    &local,
                    snapshot.is_dir(),
                    snapshot.size,
                    &fs_id.to_le_bytes(),
                )?;
                // 先算 in-sync 自愈的前置条件（**必须在写入新快照之前**，否则就是自证。
                let (pending_change, snapshot_matches) = if let Ok(meta) = std::fs::metadata(&local)
                {
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
                }
                if snapshot.is_dir() {
                    queue.push(snapshot.path.clone());
                }
            }
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
        for (fs_id, relative, state, is_dir) in missing {
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
            } else if local.exists() {
                self.platform.remove_placeholder(&local, is_dir)?;
                self.db.tombstone(fs_id).map_err(NativeSyncError::Message)?;
            } else {
                // 本地已不存在（用户在 Explorer 删除、或父目录本轮被跳过枚举）：
                // 直接记账，不再调用平台删除接口（对不存在的路径会 ENOENT 而中断整轮）。
                self.db.tombstone(fs_id).map_err(NativeSyncError::Message)?;
            }
        }
        self.status()
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
        let views = entries
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
            .collect::<Result<Vec<_>, NativeSyncError>>()?;
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
        });
        backend.replace(entries);
        backend
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
