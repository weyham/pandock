use crate::model::{EntryRecord, NativeSyncState, PinState};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct IndexDb {
    pub(crate) path: PathBuf,
    pub(crate) conn: Connection,
}
/// Row shape returned by IndexDb::entry_by_relative.
pub type EntryByRelativeRow = (u64, String, Option<String>, String, bool);

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
        self.conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY); CREATE TABLE IF NOT EXISTS sync_root(id INTEGER PRIMARY KEY CHECK(id=1),root_path TEXT NOT NULL,state TEXT NOT NULL,enabled INTEGER NOT NULL DEFAULT 0,registered_at TEXT NOT NULL); CREATE TABLE IF NOT EXISTS remote_entry(fs_id INTEGER PRIMARY KEY,parent_fs_id INTEGER,remote_path TEXT NOT NULL UNIQUE,name TEXT NOT NULL,is_dir INTEGER NOT NULL,size INTEGER NOT NULL,md5 TEXT,modified_at INTEGER,etag TEXT,seen_cycle INTEGER NOT NULL DEFAULT 0,tombstone INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS local_state(fs_id INTEGER PRIMARY KEY REFERENCES remote_entry(fs_id),relative_path TEXT NOT NULL,placeholder INTEGER NOT NULL DEFAULT 1,hydration_state TEXT NOT NULL DEFAULT 'online_only',pin_state TEXT NOT NULL DEFAULT 'unpinned',hydrated_md5 TEXT,last_error TEXT,remote_missing_ack_at TEXT,error_count INTEGER NOT NULL DEFAULT 0); CREATE TABLE IF NOT EXISTS hydration_job(id INTEGER PRIMARY KEY AUTOINCREMENT,fs_id INTEGER NOT NULL,status TEXT NOT NULL,bytes_done INTEGER NOT NULL DEFAULT 0,total_bytes INTEGER NOT NULL DEFAULT 0,started_at TEXT NOT NULL,finished_at TEXT,error TEXT); CREATE TABLE IF NOT EXISTS sync_cycle(id INTEGER PRIMARY KEY AUTOINCREMENT,started_at TEXT NOT NULL,finished_at TEXT,result TEXT); INSERT OR IGNORE INTO schema_migrations(version) VALUES(1);").map_err(|e|e.to_string())?;
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
        let mut st=self.conn.prepare("SELECT r.fs_id,r.parent_fs_id,r.remote_path,r.name,r.is_dir,r.size,r.md5,r.modified_at,l.relative_path,l.hydration_state,l.pin_state,l.last_error,EXISTS(SELECT 1 FROM remote_entry c WHERE c.parent_fs_id=r.fs_id AND c.tombstone=0) FROM remote_entry r LEFT JOIN local_state l ON l.fs_id=r.fs_id WHERE l.relative_path LIKE ?1 AND instr(substr(l.relative_path,length(?1)+1),'/')=0 AND r.tombstone=0 ORDER BY r.is_dir DESC,r.name LIMIT ?2 OFFSET ?3").map_err(|e|e.to_string())?;
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
                        fs_id: r.get(0)?,
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
                        has_children: r.get::<_, i64>(12)? != 0,
                    })
                },
            )
            .map_err(|e| e.to_string())?;
        rows.map(|r| r.map_err(|e| e.to_string())).collect()
    }
    pub fn upsert(&self, e: &EntryRecord) -> Result<(), String> {
        self.conn.execute("INSERT INTO remote_entry(fs_id,parent_fs_id,remote_path,name,is_dir,size,md5,modified_at,etag,seen_cycle,tombstone) VALUES(?,?,?,?,?,?,?,?,?,0,0) ON CONFLICT(fs_id) DO UPDATE SET parent_fs_id=excluded.parent_fs_id,remote_path=excluded.remote_path,name=excluded.name,is_dir=excluded.is_dir,size=excluded.size,md5=excluded.md5,modified_at=excluded.modified_at,tombstone=0",params![e.fs_id,e.parent_fs_id,e.remote_path,e.name,e.is_dir as i64,e.size,e.md5,e.modified_at,Option::<String>::None]).map_err(|e|e.to_string())?;
        self.conn.execute("INSERT INTO local_state(fs_id,relative_path,placeholder,hydration_state,pin_state,last_error,error_count) VALUES(?,?,1,?,?,?,0) ON CONFLICT(fs_id) DO UPDATE SET relative_path=excluded.relative_path",params![e.fs_id,e.relative_path,e.state.to_string(),e.pin_state.to_string(),e.error]).map_err(|e|e.to_string())?;
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
        self.conn.query_row("SELECT r.fs_id,l.hydration_state,r.md5,r.remote_path,r.is_dir FROM remote_entry r JOIN local_state l ON l.fs_id=r.fs_id WHERE l.relative_path=?1 AND r.tombstone=0",params![relative],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get::<_,i64>(4)?!=0))).optional().map_err(|e|e.to_string())
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
        let mut st=self.conn.prepare("SELECT r.fs_id,l.relative_path,l.hydration_state,r.is_dir FROM remote_entry r JOIN local_state l ON l.fs_id=r.fs_id WHERE r.tombstone=0").map_err(|e|e.to_string())?;
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
    pub fn mapping_by_fs_id(&self, fs_id: u64) -> Result<Option<(String, String)>, String> {
        self.conn
            .query_row(
                "SELECT r.remote_path,l.relative_path FROM remote_entry r JOIN local_state l ON l.fs_id=r.fs_id WHERE r.fs_id=?1 AND r.tombstone=0",
                params![fs_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())
    }
    pub fn all_mappings(&self) -> Result<Vec<(u64, String, String)>, String> {
        let mut st = self
            .conn
            .prepare(
                "SELECT r.fs_id,r.remote_path,l.relative_path FROM remote_entry r JOIN local_state l ON l.fs_id=r.fs_id WHERE r.tombstone=0",
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
    pub fn counts(&self) -> Result<(u64, u64, u64, u64, u64), String> {
        self.conn.query_row("SELECT COUNT(*),COALESCE(SUM(CASE WHEN l.placeholder=1 THEN 1 ELSE 0 END),0),COALESCE(SUM(CASE WHEN l.hydration_state='hydrated' THEN 1 ELSE 0 END),0),COALESCE(SUM(CASE WHEN l.hydration_state IN ('error','remote_missing','stale') THEN 1 ELSE 0 END),0),COALESCE((SELECT COUNT(*) FROM hydration_job WHERE status='running'),0) FROM remote_entry r LEFT JOIN local_state l ON l.fs_id=r.fs_id WHERE r.tombstone=0",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).map_err(|e|e.to_string())
    }
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
        assert_eq!(count, 2);
    }
    #[test]
    fn mapping_queries_support_recall_resolution() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        db.upsert(&EntryRecord {
            fs_id: 77,
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
        })
        .unwrap();
        db.upsert(&EntryRecord {
            fs_id: 78,
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
        assert_eq!(versions, 2);
        db.migrate().unwrap();
    }
    #[test]
    fn entry_listing_uses_local_relative_path_and_cursor() {
        let dir = tempdir().unwrap();
        let db = IndexDb::open(dir.path()).unwrap();
        for id in 1..=3 {
            db.upsert(&EntryRecord {
                fs_id: id,
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
            })
            .unwrap();
        }
        assert_eq!(db.entries("/", 2, 0).unwrap().len(), 2);
        assert_eq!(db.entries("/", 2, 2).unwrap().len(), 1);
    }
}
