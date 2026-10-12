use crate::model::{EntryRecord, NativeSyncState, PinState};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct IndexDb {
    pub(crate) path: PathBuf,
    pub(crate) conn: Connection,
}
/// Row shape returned by IndexDb::entry_by_relative.
pub type EntryByRelativeRow = (i64, String, Option<String>, String, bool, bool);

impl IndexDb {
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        let dir = data_dir.join("native-sync");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join("sync.db");
        if path.exists() {
            let _ = std::fs::copy(&path, path.with_extension("db.bak"));
        }
        let conn = Connection::open(&path).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| e.to_string())?;
        let db = Self { conn, path };
        db.migrate()?;
        Ok(db)
    }
    fn migrate(&self) -> Result<(), String> {
        self.conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY); CREATE TABLE IF NOT EXISTS sync_root(id INTEGER PRIMARY KEY CHECK(id=1),root_path TEXT NOT NULL,state TEXT NOT NULL,enabled INTEGER NOT NULL DEFAULT 0,registered_at TEXT NOT NULL); CREATE TABLE IF NOT EXISTS remote_entry(fs_id INTEGER PRIMARY KEY,parent_fs_id INTEGER,remote_path TEXT NOT NULL UNIQUE,name TEXT NOT NULL,is_dir INTEGER NOT NULL,size INTEGER NOT NULL,md5 TEXT,modified_at INTEGER,etag TEXT,seen_cycle INTEGER NOT NULL DEFAULT 0,tombstone INTEGER NOT NULL DEFAULT 0,local_only INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS local_state(fs_id INTEGER PRIMARY KEY REFERENCES remote_entry(fs_id),relative_path TEXT NOT NULL,placeholder INTEGER NOT NULL DEFAULT 1,hydration_state TEXT NOT NULL DEFAULT 'online_only',pin_state TEXT NOT NULL DEFAULT 'unpinned',hydrated_md5 TEXT,last_error TEXT,remote_missing_ack_at TEXT,error_count INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS hydration_job(id INTEGER PRIMARY KEY AUTOINCREMENT,fs_id INTEGER NOT NULL,status TEXT NOT NULL,bytes_done INTEGER NOT NULL DEFAULT 0,total_bytes INTEGER NOT NULL DEFAULT 0,started_at TEXT NOT NULL,finished_at TEXT,error TEXT); CREATE TABLE IF NOT EXISTS sync_cycle(id INTEGER PRIMARY KEY AUTOINCREMENT,started_at TEXT NOT NULL,finished_at TEXT,result TEXT); INSERT OR IGNORE INTO schema_migrations(version) VALUES(1);").map_err(|e|e.to_string())?;
        let has_ack: Option<String> = self.conn.query_row("SELECT name FROM pragma_table_info('local_state') WHERE name='remote_missing_ack_at'", [], |row| row.get(0)).optional().map_err(|e| e.to_string())?;
        if has_ack.is_none() {
            self.conn
                .execute(
                    "ALTER TABLE local_state ADD COLUMN remote_missing_ack_at TEXT",
                    [],
                )
                .map_err(|e| e.to_string())?;
        }
        self.conn
            .execute(
                "INSERT OR IGNORE INTO schema_migrations(version) VALUES(2)",
                [],
            )
            .map_err(|e| e.to_string())?;
        // ---- v3（ADR-109 / S1）：本地变更捕获 + 待同步轴（与 hydration 轴正交）----
        // 纯增量：建表/建索引 + 受探测保护的 ADD COLUMN；打开前已生成 .bak。
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS local_change(id INTEGER PRIMARY KEY AUTOINCREMENT,fs_id INTEGER,kind TEXT NOT NULL,phase TEXT NOT NULL,from_relative TEXT,to_relative TEXT,is_dir INTEGER NOT NULL DEFAULT 0,reason TEXT,source TEXT NOT NULL,detected_at TEXT NOT NULL,applied_to_cloud INTEGER NOT NULL DEFAULT 0,error TEXT);
                 CREATE INDEX IF NOT EXISTS local_change_pending ON local_change(applied_to_cloud, id);",
            )
            .map_err(|e| e.to_string())?;
        for (column, decl) in [
            ("pending_op", "TEXT"),
            ("pending_since", "TEXT"),
            ("snapshot_mtime", "INTEGER"),
            ("snapshot_size", "INTEGER"),
        ] {
            let probe =
                format!("SELECT name FROM pragma_table_info('local_state') WHERE name='{column}'");
            let exists: Option<String> = self
                .conn
                .query_row(&probe, [], |row| row.get(0))
                .optional()
                .map_err(|e| e.to_string())?;
            if exists.is_none() {
                self.conn
                    .execute(
                        &format!("ALTER TABLE local_state ADD COLUMN {column} {decl}"),
                        [],
                    )
                    .map_err(|e| e.to_string())?;
            }
        }
        let local_only_probe: Option<String> = self
            .conn
            .query_row(
                "SELECT name FROM pragma_table_info('remote_entry') WHERE name='local_only'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if local_only_probe.is_none() {
            self.conn
                .execute(
                    "ALTER TABLE remote_entry ADD COLUMN local_only INTEGER NOT NULL DEFAULT 0",
                    [],
                )
                .map_err(|e| e.to_string())?;
        }
        self.conn
            .execute(
                "INSERT OR IGNORE INTO schema_migrations(version) VALUES(3)",
                [],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn root(&self) -> Result<Option<(PathBuf, bool, String)>, String> {
        self.conn
            .query_row(
                "SELECT root_path,enabled,state FROM sync_root WHERE id=1",
                [],
                |r| {
                    Ok((
                        PathBuf::from(r.get::<_, String>(0)?),
                        r.get::<_, i64>(1)? != 0,
                        r.get(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| e.to_string())
    }
    pub fn set_root(&self, root: &Path, enabled: bool, state: &str) -> Result<(), String> {
        self.conn.execute("INSERT INTO sync_root(id,root_path,state,enabled,registered_at) VALUES(1,?,?,?,?) ON CONFLICT(id) DO UPDATE SET root_path=excluded.root_path,state=excluded.state,enabled=excluded.enabled",params![root.to_string_lossy(),state,enabled as i64,now()]).map(|_|()).map_err(|e|e.to_string())
    }
    pub fn count_entries(&self, parent: &str) -> Result<u64, String> {
        let prefix = if parent == "/" {
            String::new()
        } else {
            format!("{}/", parent.trim_end_matches('/'))
        };
        self.conn.query_row("SELECT COUNT(*) FROM remote_entry r JOIN local_state l ON l.fs_id=r.fs_id WHERE l.relative_path LIKE ?1 AND instr(substr(l.relative_path,length(?1)+1),'/')=0 AND r.tombstone=0", params![format!("{}%", prefix)], |r| r.get::<_, i64>(0)).map(|v| v as u64).map_err(|e| e.to_string())
    }
    pub fn entries(
        &self,
        parent: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<EntryRecord>, String> {
        let prefix = if parent == "/" {
            String::new()
        } else {
            format!("{}/", parent.trim_end_matches('/'))
        };
        let mut st=self.conn.prepare("SELECT r.fs_id,r.parent_fs_id,r.remote_path,r.name,r.is_dir,r.size,r.md5,r.modified_at,l.relative_path,l.hydration_state,l.pin_state,l.last_error,l.pending_op,l.pending_since,EXISTS(SELECT 1 FROM remote_entry c WHERE c.parent_fs_id=r.fs_id AND c.tombstone=0),r.local_only FROM remote_entry r LEFT JOIN local_state l ON l.fs_id=r.fs_id WHERE l.relative_path LIKE ?1 AND instr(substr(l.relative_path,length(?1)+1),'/')=0 AND r.tombstone=0 ORDER BY r.is_dir DESC,r.name LIMIT ?2 OFFSET ?3").map_err(|e|e.to_string())?;
        let rows = st
            .query_map(
                params![
                    format!("{}%", prefix),
                    limit.clamp(1, 1000) as i64,
                    offset as i64
                ],
                |r| {
                    let state: String = r.get(9).unwrap_or_else(|_| "online_only".into());
                    let pin: String = r.get(10).unwrap_or_else(|_| "unpinned".into());
                    Ok(EntryRecord {
                        fs_id: r.get::<_, i64>(0)?,
                        parent_fs_id: r.get(1)?,
                        remote_path: r.get(2)?,
                        name: r.get(3)?,
                        relative_path: r.get(8)?,
                        is_dir: r.get::<_, i64>(4)? != 0,
                        size: r.get::<_, i64>(5)? as u64,
                        md5: r.get(6)?,
                        modified_at: r.get(7)?,
                        state: parse_state(&state),
                        pin_state: parse_pin(&pin),
                        error: r.get(11)?,
                        pending_op: r.get(12)?,
                        pending_since: r.get(13)?,
                        has_children: r.get::<_, i64>(14)? != 0,
                        local_only: r.get::<_, i64>(15)? != 0,
                    })
                },
            )
            .map_err(|e| e.to_string())?;
        rows.map(|r| r.map_err(|e| e.to_string())).collect()
    }
    pub fn upsert(&self, e: &EntryRecord) -> Result<(), String> {
        // remote_entry 有**两个**唯一约束：fs_id（主键）与 remote_path。
        // 云端条目被替换时 fs_id 会变，而旧行仍占着同一个 remote_path；
        // 下面的 INSERT 只声明了 ON CONFLICT(fs_id)，此时会撞 remote_path 的
        // 唯一约束并让整次同步中断（表现为资源管理器里只剩目录、没有文件）。
        // 因此先按 remote_path 清掉不同 fs_id 的陈旧行（连同其 local_state）。
        let stale: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT fs_id FROM remote_entry WHERE remote_path=?1 AND fs_id<>?2")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![e.remote_path, e.fs_id], |r| r.get::<_, i64>(0))
                .map_err(|e| e.to_string())?;
            rows.filter_map(Result::ok).collect()
        };
        for fs_id in &stale {
            log::info!(
                "native-sync replacing stale entry: fs_id={fs_id} path={}",
                e.remote_path
            );
            self.conn
                .execute("DELETE FROM local_state WHERE fs_id=?1", params![fs_id])
                .map_err(|e| e.to_string())?;
            self.conn
                .execute("DELETE FROM remote_entry WHERE fs_id=?1", params![fs_id])
                .map_err(|e| e.to_string())?;
        }
        self.conn.execute("INSERT INTO remote_entry(fs_id,parent_fs_id,remote_path,name,is_dir,size,md5,modified_at,etag,seen_cycle,tombstone) VALUES(?,?,?,?,?,?,?,?,?,0,0) ON CONFLICT(fs_id) DO UPDATE SET parent_fs_id=excluded.parent_fs_id,remote_path=excluded.remote_path,name=excluded.name,is_dir=excluded.is_dir,size=excluded.size,md5=excluded.md5,modified_at=excluded.modified_at,tombstone=0",params![e.fs_id,e.parent_fs_id,e.remote_path,e.name,e.is_dir as i64,e.size,e.md5,e.modified_at,Option::<String>::None]).map_err(|err|format!("remote_entry 写入失败 fs_id={} path={} err={err}",e.fs_id,e.remote_path))?;
        self.conn.execute("INSERT INTO local_state(fs_id,relative_path,placeholder,hydration_state,pin_state,last_error,error_count) VALUES(?,?,1,?,?,?,0) ON CONFLICT(fs_id) DO UPDATE SET relative_path=CASE WHEN local_state.pending_op IS NULL THEN excluded.relative_path ELSE local_state.relative_path END",params![e.fs_id,e.relative_path,e.state.to_string(),e.pin_state.to_string(),e.error]).map_err(|e|e.to_string())?;
        Ok(())
    }
    /// 按远端路径查 fs_id（容忍前导斜杠差异）；parent_fs_id 回填用。
    pub fn fs_id_for_remote(&self, remote: &str) -> Result<Option<u64>, String> {
        let key = if remote.starts_with('/') {
            remote.to_string()
        } else {
            format!("/{remote}")
        };
        self.conn
            .query_row(
                "SELECT fs_id FROM remote_entry WHERE remote_path=?1 AND tombstone=0",
                params![key],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())
    }
    pub fn relative_for_remote(&self, remote: &str) -> Result<Option<String>, String> {
        self.conn.query_row("SELECT l.relative_path FROM remote_entry r JOIN local_state l ON l.fs_id=r.fs_id WHERE r.remote_path=?1 AND r.tombstone=0", params![remote], |r| r.get(0)).optional().map_err(|e| e.to_string())
    }
    pub fn entry_by_relative(&self, relative: &str) -> Result<Option<EntryByRelativeRow>, String> {
        self.conn.query_row("SELECT r.fs_id,l.hydration_state,r.md5,r.remote_path,r.is_dir,r.local_only FROM remote_entry r JOIN local_state l ON l.fs_id=r.fs_id WHERE l.relative_path=?1 AND r.tombstone=0",params![relative],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get::<_,i64>(4)?!=0,r.get::<_,i64>(5)?!=0))).optional().map_err(|e|e.to_string())
    }
    pub fn set_pin_state(&self, fs_id: u64, pinned: bool) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE local_state SET pin_state=?2 WHERE fs_id=?1",
                params![fs_id, if pinned { "pinned" } else { "unpinned" }],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn set_state_by_fs_id(
        &self,
        fs_id: u64,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE local_state SET hydration_state=?2,last_error=?3 WHERE fs_id=?1",
                params![fs_id, state, error],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn has_hydrated_descendant(&self, relative: &str) -> Result<bool, String> {
        let prefix = format!("{}/%", relative.trim_end_matches('/'));
        self.conn.query_row("SELECT EXISTS(SELECT 1 FROM local_state WHERE relative_path LIKE ?1 AND hydration_state='hydrated')", params![prefix], |r| r.get(0)).map_err(|e| e.to_string())
    }
    pub fn missing_entries(
        &self,
        seen: &[u64],
    ) -> Result<Vec<(u64, String, String, bool)>, String> {
        let mut st=self.conn.prepare("SELECT r.fs_id,l.relative_path,l.hydration_state,r.is_dir FROM remote_entry r JOIN local_state l ON l.fs_id=r.fs_id WHERE r.tombstone=0 AND r.local_only=0").map_err(|e|e.to_string())?;
        let rows = st
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0))
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            let row = row.map_err(|e| e.to_string())?;
            if !seen.contains(&row.0) {
                out.push(row);
            }
        }
        Ok(out)
    }
    pub fn tombstone(&self, fs_id: u64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE remote_entry SET tombstone=1 WHERE fs_id=?1",
                params![fs_id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn hydration_started(&self, fs_id: u64, total: u64) -> Result<i64, String> {
        let now = now();
        self.conn.execute("INSERT INTO hydration_job(fs_id,status,total_bytes,started_at) VALUES(?1,'running',?2,?3)",params![fs_id,total,now]).map_err(|e|e.to_string())?;
        let id = self.conn.last_insert_rowid();
        self.conn
            .execute(
                "UPDATE local_state SET hydration_state='hydrating',last_error=NULL WHERE fs_id=?1",
                params![fs_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(id)
    }
    /// 召回映射的唯一权威出口。**本地新建条目（local_only）必须拒绝**：
    /// 它们的 remote_path 是 `local:` 前缀的内部占位，若被当作远端路径解析会构造出
    /// 非法云端请求。这里返回显式错误而不是伪造映射。
    pub fn mapping_by_fs_id(&self, fs_id: i64) -> Result<Option<(String, String)>, String> {
        if fs_id < 0 {
            return Err(LOCAL_ONLY_NO_REMOTE.into());
        }
        let row: Option<(String, String, i64)> = self
            .conn
            .query_row(
                "SELECT r.remote_path,l.relative_path,r.local_only FROM remote_entry r JOIN local_state l ON l.fs_id=r.fs_id WHERE r.fs_id=?1 AND r.tombstone=0",
                params![fs_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        match row {
            None => Ok(None),
            Some((_, _, local_only)) if local_only != 0 => Err(LOCAL_ONLY_NO_REMOTE.into()),
            Some((remote, relative, _)) => Ok(Some((remote, relative))),
        }
    }
    pub fn all_mappings(&self) -> Result<Vec<(u64, String, String)>, String> {
        let mut st = self
            .conn
            .prepare(
                "SELECT r.fs_id,r.remote_path,l.relative_path FROM remote_entry r JOIN local_state l ON l.fs_id=r.fs_id WHERE r.tombstone=0 AND r.local_only=0",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| e.to_string())?;
        rows.map(|r| r.map_err(|e| e.to_string())).collect()
    }
    pub fn hydration_set_total(&self, job: i64, total: u64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE hydration_job SET total_bytes=?2 WHERE id=?1 AND status='running'",
                params![job, total],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn hydration_progress(&self, job: i64, bytes_done: u64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE hydration_job SET bytes_done=?2 WHERE id=?1 AND status='running'",
                params![job, bytes_done],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn hydration_finished(
        &self,
        fs_id: u64,
        job: i64,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE hydration_job SET status=?2,finished_at=?3,error=?4 WHERE id=?1",
                params![
                    job,
                    if error.is_some() {
                        "failed"
                    } else {
                        "completed"
                    },
                    now(),
                    error
                ],
            )
            .map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "UPDATE local_state SET hydration_state=?2,last_error=?3 WHERE fs_id=?1",
                params![fs_id, state, error],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn hydration_progress_for(
        &self,
        fs_id: u64,
    ) -> Result<Option<crate::model::ProgressView>, String> {
        self.conn.query_row(
            "SELECT bytes_done,total_bytes FROM hydration_job WHERE fs_id=?1 AND status='running' ORDER BY id DESC LIMIT 1",
            params![fs_id],
            |row| Ok(crate::model::ProgressView {
                completed: row.get::<_, i64>(0)? as u64,
                total: row.get::<_, i64>(1)? as u64,
            }),
        ).optional().map_err(|e| e.to_string())
    }
    pub fn acknowledge_remote_missing(&self, fs_id: u64) -> Result<(), String> {
        self.conn.execute("UPDATE local_state SET remote_missing_ack_at=?2 WHERE fs_id=?1 AND hydration_state='remote_missing'", params![fs_id, now()]).map(|_| ()).map_err(|e| e.to_string())
    }
    /// 记录一条本地变更（S1 追加式日志；S2/S3 将其作为上传/传播队列消费）。
    pub fn record_local_change(&self, change: &LocalChangeRecord) -> Result<i64, String> {
        self.conn
            .execute(
                "INSERT INTO local_change(fs_id,kind,phase,from_relative,to_relative,is_dir,reason,source,detected_at,applied_to_cloud,error) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    change.fs_id,
                    change.kind,
                    change.phase,
                    change.from_relative,
                    change.to_relative,
                    change.is_dir as i64,
                    change.reason,
                    change.source,
                    change.detected_at,
                    change.applied_to_cloud as i64,
                    change.error
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(self.conn.last_insert_rowid())
    }
    #[cfg(test)]
    pub fn recent_local_changes(&self, limit: usize) -> Result<Vec<LocalChangeRecord>, String> {
        let mut st = self
            .conn
            .prepare(
                "SELECT id,fs_id,kind,phase,from_relative,to_relative,is_dir,reason,source,detected_at,applied_to_cloud,error FROM local_change ORDER BY id DESC LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map(params![limit as i64], |r| {
                Ok(LocalChangeRecord {
                    id: Some(r.get(0)?),
                    fs_id: r.get(1)?,
                    kind: r.get(2)?,
                    phase: r.get(3)?,
                    from_relative: r.get(4)?,
                    to_relative: r.get(5)?,
                    is_dir: r.get::<_, i64>(6)? != 0,
                    reason: r.get(7)?,
                    source: r.get(8)?,
                    detected_at: r.get(9)?,
                    applied_to_cloud: r.get::<_, i64>(10)? != 0,
                    error: r.get(11)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.map(|r| r.map_err(|e| e.to_string())).collect()
    }
    /// 设置/清除「待同步到云端」标记（hydration 轴之外的第二条状态轴）。
    pub fn set_pending_op(
        &self,
        fs_id: i64,
        op: Option<&str>,
        since: Option<&str>,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE local_state SET pending_op=?2,pending_since=?3 WHERE fs_id=?1",
                params![fs_id, op, since],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())?;
        // 写穿到「脱水安全闸门」的内存镜像（见 `crate::pending`）：脱水回调线程只读内存，
        // 绝不查库。这里是唯一写入点（设置与清除都覆盖），镜像因此不会漏标。
        if op.is_some() {
            crate::pending::mark(fs_id as u64);
        } else {
            crate::pending::clear(fs_id as u64);
        }
        Ok(())
    }
    /// 有未同步本地改动的 `fs_id` 集合（**权威来源**）：启动/重连时用它预热内存镜像。
    pub fn pending_fs_ids(&self) -> Result<Vec<u64>, String> {
        let mut st = self
            .conn
            .prepare("SELECT fs_id FROM local_state WHERE pending_op IS NOT NULL")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        rows.map(|r| r.map(|v| v as u64).map_err(|e| e.to_string()))
            .collect()
    }
    pub fn pending_count(&self) -> Result<u64, String> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM local_state WHERE pending_op IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v as u64)
            .map_err(|e| e.to_string())
    }
    /// 记录占位符创建/水合时的本地快照，用于识别后续的本地写入。
    pub fn set_snapshot(&self, fs_id: i64, mtime: Option<i64>, size: u64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE local_state SET snapshot_mtime=?2,snapshot_size=?3 WHERE fs_id=?1",
                params![fs_id, mtime, size as i64],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    /// 读取占位符创建 / 上次水合时记录的本地快照。
    ///
    /// **`None`（无行）或 size 为 `None`（NULL）都表示「没有可用基线」**——例如 S1 之前
    /// 水合的老库行，或尚未记录过快照的条目。调用方必须把它当作「无法判定」，
    /// **绝不能**折叠成 `0`：否则任何一次读取都会被判成「本地被修改」
    /// （实测事故：只读打开三个文件即被记成 `pending_op='upload'`）。
    pub fn snapshot_for(&self, fs_id: i64) -> Result<Option<LocalSnapshot>, String> {
        self.conn
            .query_row(
                "SELECT snapshot_mtime,snapshot_size FROM local_state WHERE fs_id=?1",
                params![fs_id],
                |r| {
                    Ok(LocalSnapshot {
                        mtime: r.get(0)?,
                        size: r.get::<_, Option<i64>>(1)?.map(|v| v.max(0) as u64),
                    })
                },
            )
            .optional()
            .map_err(|e| e.to_string())
    }
    /// 本地新建（云端不存在）条目：预留负数 fs_id，remote_path 用 local: 前缀。
    pub fn next_local_fs_id(&self) -> Result<i64, String> {
        self.conn
            .query_row(
                "SELECT COALESCE(MIN(fs_id),0) FROM remote_entry WHERE local_only=1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|min| if min < 0 { min - 1 } else { -1 })
            .map_err(|e| e.to_string())
    }
    pub fn insert_local_only_entry(
        &self,
        fs_id: i64,
        relative: &str,
        name: &str,
        is_dir: bool,
        size: u64,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO remote_entry(fs_id,parent_fs_id,remote_path,name,is_dir,size,md5,modified_at,etag,seen_cycle,tombstone,local_only) VALUES(?1,NULL,?2,?3,?4,?5,NULL,NULL,NULL,0,0,1)",
                params![
                    fs_id,
                    format!("local:{relative}"),
                    name,
                    is_dir as i64,
                    size as i64
                ],
            )
            .map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "INSERT INTO local_state(fs_id,relative_path,placeholder,hydration_state,pin_state,last_error,error_count) VALUES(?1,?2,0,'online_only','unpinned',NULL,0)",
                params![fs_id, relative],
            )
            .map_err(|e| e.to_string())?;
        self.set_pending_op(fs_id, Some("upload"), Some(&now()))
    }
    /// 某条云端路径上是否已有「本地已删除/改名、尚未同步到云端」的待同步标记。
    ///
    /// H1：注册 NOTIFY_DELETE / NOTIFY_RENAME 之后，`sync_now` 会同时看到
    /// 「本地已删除/改名」与「云端仍在」。若照旧重建占位符，用户删掉的文件会在下次
    /// 同步无声复活、改名会多出旧路径副本。调用方据此跳过占位符重建。
    pub fn pending_op_for_remote(&self, remote: &str) -> Result<Option<(i64, String)>, String> {
        let key = if remote.starts_with('/') {
            remote.to_string()
        } else {
            format!("/{remote}")
        };
        self.conn
            .query_row(
                "SELECT r.fs_id,l.pending_op FROM remote_entry r JOIN local_state l ON l.fs_id=r.fs_id WHERE r.remote_path=?1 AND r.tombstone=0 AND l.pending_op IS NOT NULL",
                params![key],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())
    }
    pub fn pending_op_of(&self, fs_id: i64) -> Result<Option<String>, String> {
        self.conn
            .query_row(
                "SELECT pending_op FROM local_state WHERE fs_id=?1",
                params![fs_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|v| v.flatten())
            .map_err(|e| e.to_string())
    }
    /// 占位符 identity 的 fs_id 出口守卫：identity 必须来自云端 fs_id。
    pub fn cloud_fs_id(&self, fs_id: i64) -> Result<u64, String> {
        if fs_id < 0 {
            return Err(LOCAL_ONLY_NO_REMOTE.into());
        }
        let local_only: Option<i64> = self
            .conn
            .query_row(
                "SELECT local_only FROM remote_entry WHERE fs_id=?1",
                params![fs_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if local_only == Some(1) {
            return Err(LOCAL_ONLY_NO_REMOTE.into());
        }
        Ok(fs_id as u64)
    }
    /// 记录一次本地改名：路径改为新值，并标记待同步（不改动远端事实）。
    pub fn apply_local_rename(&self, fs_id: i64, to_relative: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE local_state SET relative_path=?2,pending_op='rename',pending_since=?3 WHERE fs_id=?1",
                params![fs_id, to_relative, now()],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    pub fn counts(&self) -> Result<(u64, u64, u64, u64, u64), String> {
        self.conn.query_row("SELECT COUNT(*),COALESCE(SUM(CASE WHEN l.placeholder=1 THEN 1 ELSE 0 END),0),COALESCE(SUM(CASE WHEN l.hydration_state='hydrated' THEN 1 ELSE 0 END),0),COALESCE(SUM(CASE WHEN l.hydration_state IN ('error','remote_missing','stale') OR l.pending_op IN ('delete','rename') THEN 1 ELSE 0 END),0),COALESCE((SELECT COUNT(*) FROM hydration_job WHERE status='running'),0) FROM remote_entry r LEFT JOIN local_state l ON l.fs_id=r.fs_id WHERE r.tombstone=0",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).map_err(|e|e.to_string())
    }
}
/// 占位符创建 / 上次水合时记录的本地快照。
///
/// `size` 为 `None` 表示**没有可用基线**（老库行为 NULL、或从未记录过）——
/// 调用方必须视为「无法判定」，绝不能当作 0，否则任何一次读取都会被判成本地修改。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalSnapshot {
    pub mtime: Option<i64>,
    pub size: Option<u64>,
}

/// 本地新建条目（`local_only=1`）没有云端映射的统一错误文本。
pub const LOCAL_ONLY_NO_REMOTE: &str =
    "本地新建条目没有远端映射，不能用于云端召回或占位符 identity";

/// 一条本地变更记录（S1 只写不消费；S2/S3 消费）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalChangeRecord {
    pub id: Option<i64>,
    pub fs_id: Option<i64>,
    pub kind: String,
    pub phase: String,
    pub from_relative: Option<String>,
    pub to_relative: Option<String>,
    pub is_dir: bool,
    pub reason: Option<String>,
    pub source: String,
    pub detected_at: String,
    pub applied_to_cloud: bool,
    pub error: Option<String>,
}

fn now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}
fn parse_state(s: &str) -> NativeSyncState {
    match s {
        "hydrating" => NativeSyncState::Hydrating,
        "hydrated" => NativeSyncState::Hydrated,
        "error" => NativeSyncState::Error,
        "remote_missing" => NativeSyncState::RemoteMissing,
        "stale" => NativeSyncState::Stale,
        _ => NativeSyncState::OnlineOnly,
    }
}
fn parse_pin(s: &str) -> PinState {
    if s == "pinned" {
        PinState::Pinned
    } else {
        PinState::Unpinned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntryRecord, NativeSyncState, PinState};
    use tempfile::tempdir;
    /// remote_path 是唯一键；云端条目被替换后 fs_id 变化时，
    /// 旧行仍占用同一路径，若 upsert 只处理 ON CONFLICT(fs_id) 会撞
    /// remote_path 唯一约束并使整次同步中断（实测：目录建出、文件全缺）。
    #[test]
    fn upsert_replaces_stale_row_when_remote_path_changes_fs_id() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        let record = |fs_id: i64, state: NativeSyncState| EntryRecord {
            fs_id,
            local_only: false,
            parent_fs_id: None,
            remote_path: "/a.txt".into(),
            relative_path: "a.txt".into(),
            name: "a.txt".into(),
            is_dir: false,
            size: 3,
            md5: None,
            modified_at: None,
            state,
            pin_state: PinState::Unpinned,
            error: None,
            has_children: false,

            pending_op: None,
            pending_since: None,
        };

        db.upsert(&record(1, NativeSyncState::Hydrated)).unwrap();
        // 同一路径、新 fs_id：必须成功，并替换掉旧行。
        db.upsert(&record(2, NativeSyncState::OnlineOnly)).unwrap();

        let ids: Vec<i64> = {
            let mut stmt = db
                .conn
                .prepare("SELECT fs_id FROM remote_entry WHERE remote_path='/a.txt'")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap();
            rows.filter_map(Result::ok).collect()
        };
        assert_eq!(ids, vec![2], "旧 fs_id 应被清理，只保留新行");

        let states: Vec<i64> = {
            let mut stmt = db
                .conn
                .prepare("SELECT fs_id FROM local_state ORDER BY fs_id")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap();
            rows.filter_map(Result::ok).collect()
        };
        assert_eq!(states, vec![2], "旧 local_state 应一并清理");
    }

    #[test]
    fn migration_enables_wal_and_creates_all_tables() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        let mode: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3, "schema_migrations 应记录 1/2/3 三个版本");
        // v3：本地变更捕获表、索引与待同步/快照列必须存在
        let table: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='local_change'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table, 1, "local_change 表应存在");
        let index: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='local_change_pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index, 1, "local_change_pending 索引应存在");
        for column in [
            "pending_op",
            "pending_since",
            "snapshot_mtime",
            "snapshot_size",
        ] {
            let n: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('local_state') WHERE name=?1",
                    [column],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "local_state.{column} 应存在");
        }
        let local_only: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('remote_entry') WHERE name='local_only'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(local_only, 1, "remote_entry.local_only 应存在");
    }
    #[test]
    fn mapping_queries_support_recall_resolution() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        db.upsert(&EntryRecord {
            fs_id: 77,
            local_only: false,
            parent_fs_id: None,
            remote_path: "/a.txt".into(),
            relative_path: "a.txt".into(),
            name: "a.txt".into(),
            is_dir: false,
            size: 3,
            md5: None,
            modified_at: None,
            state: NativeSyncState::OnlineOnly,
            pin_state: PinState::Unpinned,
            error: None,
            has_children: false,

            pending_op: None,
            pending_since: None,
        })
        .unwrap();
        db.upsert(&EntryRecord {
            fs_id: 78,
            local_only: false,
            parent_fs_id: None,
            remote_path: "/d/b.txt".into(),
            relative_path: "d/b.txt".into(),
            name: "b.txt".into(),
            is_dir: false,
            size: 3,
            md5: None,
            modified_at: None,
            state: NativeSyncState::OnlineOnly,
            pin_state: PinState::Unpinned,
            error: None,
            has_children: false,

            pending_op: None,
            pending_since: None,
        })
        .unwrap();
        assert_eq!(
            db.mapping_by_fs_id(77).unwrap(),
            Some(("/a.txt".to_string(), "a.txt".to_string()))
        );
        assert_eq!(db.mapping_by_fs_id(999).unwrap(), None);
        let all = db.all_mappings().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&(77, "/a.txt".to_string(), "a.txt".to_string())));
        assert!(all.contains(&(78, "/d/b.txt".to_string(), "d/b.txt".to_string())));
    }
    #[test]
    fn upsert_conflict_heals_parent_fs_id_for_legacy_rows() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        let record = |parent: Option<u64>| EntryRecord {
            fs_id: 20,
            local_only: false,
            parent_fs_id: parent,
            remote_path: "/d/f.txt".into(),
            relative_path: "d/f.txt".into(),
            name: "f.txt".into(),
            is_dir: false,
            size: 3,
            md5: None,
            modified_at: None,
            state: NativeSyncState::OnlineOnly,
            pin_state: PinState::Unpinned,
            error: None,
            has_children: false,

            pending_op: None,
            pending_since: None,
        };
        db.upsert(&record(None)).unwrap();
        db.upsert(&record(Some(5))).unwrap();
        let parent: Option<u64> = db
            .conn
            .query_row(
                "SELECT parent_fs_id FROM remote_entry WHERE fs_id=20",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            parent,
            Some(5),
            "重复 upsert 应自愈老库的 NULL parent_fs_id"
        );
    }
    #[test]
    fn running_job_progress_is_object_and_completed_job_disappears() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        let job = db.hydration_started(99, 1024).unwrap();
        db.hydration_progress(job, 256).unwrap();
        let p = db.hydration_progress_for(99).unwrap().unwrap();
        assert_eq!((p.completed, p.total), (256, 1024));
        db.hydration_finished(99, job, "online_only", None).unwrap();
        assert!(db.hydration_progress_for(99).unwrap().is_none());
    }
    #[test]
    fn migration_upgrades_v1_and_ack_preserves_remote_missing() {
        let dir = tempdir().unwrap();
        let folder = dir.path().join("native-sync");
        std::fs::create_dir_all(&folder).unwrap();
        let legacy = Connection::open(folder.join("sync.db")).unwrap();
        legacy.execute_batch("CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY); INSERT INTO schema_migrations VALUES(1); CREATE TABLE local_state(fs_id INTEGER PRIMARY KEY, hydration_state TEXT NOT NULL, last_error TEXT);
            INSERT INTO local_state VALUES(42, 'remote_missing', 'kept');").unwrap();
        drop(legacy);
        let db = IndexDb::open(dir.path()).unwrap();
        assert!(db.path().with_extension("db.bak").exists());
        db.acknowledge_remote_missing(42).unwrap();
        let (state, acknowledged): (String, Option<String>) = db
            .conn
            .query_row(
                "SELECT hydration_state,remote_missing_ack_at FROM local_state WHERE fs_id=42",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "remote_missing");
        assert!(acknowledged.is_some());
        let versions: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(versions, 3, "v1 库升级后应记录到 3");
        db.migrate().unwrap();
        // 幂等：二次迁移不得报错，且 v3 对象已在 v1 库上升级出来
        let table: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='local_change'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table, 1, "v1 库升级后应具备 local_change 表");
        let pending: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('local_state') WHERE name='pending_op'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1, "v1 库升级后 local_state 应具备 pending_op");
    }
    #[test]
    fn entry_listing_uses_local_relative_path_and_cursor() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        for id in 1..=3 {
            db.upsert(&EntryRecord {
                fs_id: id,
                local_only: false,
                parent_fs_id: None,
                remote_path: format!("/f{id}"),
                relative_path: format!("f{id}"),
                name: format!("f{id}"),
                is_dir: false,
                size: 0,
                md5: None,
                modified_at: None,
                state: NativeSyncState::OnlineOnly,
                pin_state: PinState::Unpinned,
                error: None,
                has_children: false,

                pending_op: None,
                pending_since: None,
            })
            .unwrap();
        }
        assert_eq!(db.entries("/", 2, 0).unwrap().len(), 2);
        assert_eq!(db.entries("/", 2, 2).unwrap().len(), 1);
    }

    /// 本地新建条目的三条云出口必须显式拒绝：fs_id→RemotePath、fs_id→占位符 identity、
    /// all_mappings（fs_id→by_id 预热源）不得把内部占位当成合法云端事实。
    #[test]
    fn local_only_rows_are_rejected_by_every_cloud_identity_exit() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        let fs_id = db.next_local_fs_id().unwrap();
        assert!(fs_id < 0, "本地新建条目必须占用负数保留区间");
        db.insert_local_only_entry(fs_id, "新建.txt", "新建.txt", false, 3)
            .unwrap();

        // ① fs_id -> RemotePath
        let err = db.mapping_by_fs_id(fs_id).unwrap_err();
        assert_eq!(err, LOCAL_ONLY_NO_REMOTE);
        // ② fs_id -> 占位符 identity
        let err = db.cloud_fs_id(fs_id).unwrap_err();
        assert_eq!(err, LOCAL_ONLY_NO_REMOTE);
        // ③ all_mappings（内存 by_id 预热的唯一来源）
        assert!(
            db.all_mappings().unwrap().is_empty(),
            "本地新建条目不得进入云端映射预热"
        );

        // 对照：云端条目仍然正常，守卫不是「一律拒绝」
        db.upsert(&EntryRecord {
            fs_id: 42,
            local_only: false,
            parent_fs_id: None,
            remote_path: "/a.txt".into(),
            relative_path: "a.txt".into(),
            name: "a.txt".into(),
            is_dir: false,
            size: 3,
            md5: None,
            modified_at: None,
            state: NativeSyncState::OnlineOnly,
            pin_state: PinState::Unpinned,
            error: None,
            has_children: false,
            pending_op: None,
            pending_since: None,
        })
        .unwrap();
        assert_eq!(
            db.mapping_by_fs_id(42).unwrap().unwrap().0,
            "/a.txt".to_string()
        );
        assert_eq!(db.cloud_fs_id(42).unwrap(), 42);
        assert_eq!(db.all_mappings().unwrap().len(), 1);
    }

    /// H1 的数据层前提：pending_op_for_remote 必须能按云端路径查到本地待同步标记。
    #[test]
    fn pending_op_lookup_is_keyed_by_remote_path() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        db.upsert(&EntryRecord {
            fs_id: 7,
            local_only: false,
            parent_fs_id: None,
            remote_path: "/d/f.txt".into(),
            relative_path: "d/f.txt".into(),
            name: "f.txt".into(),
            is_dir: false,
            size: 3,
            md5: None,
            modified_at: None,
            state: NativeSyncState::OnlineOnly,
            pin_state: PinState::Unpinned,
            error: None,
            has_children: false,
            pending_op: None,
            pending_since: None,
        })
        .unwrap();
        assert!(db.pending_op_for_remote("/d/f.txt").unwrap().is_none());
        db.set_pending_op(7, Some("delete"), Some("2026-10-12T00:00:00Z"))
            .unwrap();
        let (fs_id, op) = db.pending_op_for_remote("d/f.txt").unwrap().unwrap();
        assert_eq!((fs_id, op.as_str()), (7, "delete"));
        assert_eq!(db.pending_op_of(7).unwrap().as_deref(), Some("delete"));
        // 待同步的删除/改名必须计入 attention（否则 UI 无从提示）
        assert_eq!(db.counts().unwrap().3, 1);
        db.set_pending_op(7, None, None).unwrap();
        assert_eq!(db.counts().unwrap().3, 0);
    }

    /// `snapshot_for` 必须区分「没有基线」（NULL）与「基线为 0 字节」。
    /// 把 NULL 折叠成 0 会让任何读取被误判成本地修改（真实事故）。
    #[test]
    fn snapshot_for_distinguishes_missing_baseline_from_zero_size() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        db.upsert(&EntryRecord {
            fs_id: 51,
            local_only: false,
            parent_fs_id: None,
            remote_path: "/legacy.txt".into(),
            relative_path: "legacy.txt".into(),
            name: "legacy.txt".into(),
            is_dir: false,
            size: 12,
            md5: None,
            modified_at: None,
            state: NativeSyncState::Hydrated,
            pin_state: PinState::Unpinned,
            error: None,
            has_children: false,
            pending_op: None,
            pending_since: None,
        })
        .unwrap();
        // 未记录过快照 => 两个字段都是 NULL（没有可用基线）。
        let missing = db.snapshot_for(51).unwrap().unwrap();
        assert_eq!((missing.mtime, missing.size), (None, None));
        // 记录 0 字节基线 => 是真实基线，不等于"没有基线"。
        db.set_snapshot(51, Some(1_700_000_000), 0).unwrap();
        let zero = db.snapshot_for(51).unwrap().unwrap();
        assert_eq!((zero.mtime, zero.size), (Some(1_700_000_000), Some(0)));
        // 不存在的条目 => None。
        assert_eq!(db.snapshot_for(999).unwrap(), None);
    }

    /// 本地改名：路径改为新值并标记待同步，云端事实保持不变。
    #[test]
    fn apply_local_rename_rewrites_relative_path_and_marks_pending() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        db.upsert(&EntryRecord {
            fs_id: 9,
            local_only: false,
            parent_fs_id: None,
            remote_path: "/old.txt".into(),
            relative_path: "old.txt".into(),
            name: "old.txt".into(),
            is_dir: false,
            size: 3,
            md5: None,
            modified_at: None,
            state: NativeSyncState::OnlineOnly,
            pin_state: PinState::Unpinned,
            error: None,
            has_children: false,
            pending_op: None,
            pending_since: None,
        })
        .unwrap();
        db.apply_local_rename(9, "new.txt").unwrap();
        let (_, state, _, remote_path, _, local_only) =
            db.entry_by_relative("new.txt").unwrap().unwrap();
        assert!(!local_only);
        assert_eq!(remote_path, "/old.txt", "本地改名不得改写云端路径");
        assert_eq!(state, "online_only");
        assert_eq!(db.pending_op_of(9).unwrap().as_deref(), Some("rename"));
        assert!(db.entry_by_relative("old.txt").unwrap().is_none());
    }
}
