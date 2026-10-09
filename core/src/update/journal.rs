use crate::update::{UpdateError, UPDATE_PROTOCOL, UPDATE_SCHEMA};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalState {
    Prepared,
    ParentExited,
    LockAcquired,
    BackupComplete,
    ReplaceStarted,
    ReplaceComplete,
    LaunchStarted,
    Ready,
    RollbackStarted,
    RolledBack,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateJournal {
    pub schema: u32,
    pub protocol: u32,
    pub source_kind: String,
    pub from_version: String,
    pub to_version: String,
    pub app_dir: String,
    pub staging_dir: String,
    pub rollback_dir: String,
    pub helper_path: String,
    pub parent_pid: u32,
    pub readiness_token: String,
    pub state: JournalState,
    pub created_at: i64,
    pub updated_at: i64,
    pub error: Option<String>,
}

impl UpdateJournal {
    // The constructor intentionally mirrors the journal record schema.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        from_version: impl Into<String>,
        to_version: impl Into<String>,
        app_dir: &Path,
        staging_dir: &Path,
        rollback_dir: &Path,
        helper_path: &Path,
        parent_pid: u32,
        readiness_token: impl Into<String>,
    ) -> Self {
        let now = now_epoch();
        Self {
            schema: UPDATE_SCHEMA,
            protocol: UPDATE_PROTOCOL,
            source_kind: "private_github".into(),
            from_version: from_version.into(),
            to_version: to_version.into(),
            app_dir: app_dir.to_string_lossy().into_owned(),
            staging_dir: staging_dir.to_string_lossy().into_owned(),
            rollback_dir: rollback_dir.to_string_lossy().into_owned(),
            helper_path: helper_path.to_string_lossy().into_owned(),
            parent_pid,
            readiness_token: readiness_token.into(),
            state: JournalState::Prepared,
            created_at: now,
            updated_at: now,
            error: None,
        }
    }

    pub fn load(path: &Path) -> Result<Self, UpdateError> {
        let text =
            fs::read_to_string(path).map_err(|error| UpdateError::Internal(error.to_string()))?;
        let journal: Self = serde_json::from_str(&text)
            .map_err(|error| UpdateError::InvalidManifest(error.to_string()))?;
        journal.validate()?;
        Ok(journal)
    }

    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.schema != UPDATE_SCHEMA || self.protocol != UPDATE_PROTOCOL {
            return Err(UpdateError::InvalidManifest("journal 协议不匹配".into()));
        }
        if self.source_kind != "private_github" {
            return Err(UpdateError::InvalidManifest("journal 更新源不匹配".into()));
        }
        if self.from_version.trim().is_empty() || self.to_version.trim().is_empty() {
            return Err(UpdateError::InvalidManifest("journal 版本为空".into()));
        }
        Ok(())
    }

    pub fn transition(&mut self, state: JournalState) -> Result<(), UpdateError> {
        self.state = state;
        self.updated_at = now_epoch();
        Ok(())
    }

    pub fn fail(&mut self, error: impl Into<String>) -> Result<(), UpdateError> {
        self.error = Some(error.into());
        self.transition(JournalState::Failed)
    }

    pub fn save(&self, path: &Path) -> Result<(), UpdateError> {
        write_json_atomic(path, self)
    }

    pub fn recovery_action(&self) -> RecoveryAction {
        match self.state {
            JournalState::Prepared => RecoveryAction::CleanStaging,
            JournalState::ParentExited | JournalState::LockAcquired => RecoveryAction::Retry,
            JournalState::BackupComplete
            | JournalState::ReplaceStarted
            | JournalState::ReplaceComplete => RecoveryAction::Rollback,
            JournalState::LaunchStarted => RecoveryAction::LaunchOrRollback,
            JournalState::Ready | JournalState::Completed => RecoveryAction::Cleanup,
            JournalState::RollbackStarted | JournalState::RolledBack => RecoveryAction::Rollback,
            JournalState::Failed => RecoveryAction::ManualRepair,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    CleanStaging,
    Retry,
    Rollback,
    LaunchOrRollback,
    Cleanup,
    ManualRepair,
}

pub fn journal_path(app_dir: &Path) -> PathBuf {
    app_dir.join("updates").join("update-journal.json")
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), UpdateError> {
    let parent = path
        .parent()
        .ok_or_else(|| UpdateError::Internal("journal 路径没有父目录".into()))?;
    fs::create_dir_all(parent).map_err(|error| UpdateError::Internal(error.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| UpdateError::Internal(error.to_string()))?;
    fs::write(&tmp, text).map_err(|error| UpdateError::Internal(error.to_string()))?;
    if path.exists() {
        fs::remove_file(path).map_err(|error| UpdateError::Internal(error.to_string()))?;
    }
    fs::rename(&tmp, path).map_err(|error| UpdateError::Internal(error.to_string()))?;
    Ok(())
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_round_trip_and_recovery_mapping() {
        let root = std::env::temp_dir().join(format!("pandock-journal-{}", uuid::Uuid::new_v4()));
        let path = root.join("update-journal.json");
        let mut journal = UpdateJournal::new(
            "1.0.0",
            "1.0.1",
            &root.join("app"),
            &root.join("app/updates/staging/1.0.1"),
            &root.join("app/updates/rollback/1.0.0"),
            &root.join("app/updates/staging/1.0.1/pandock-updater.exe"),
            123,
            "token",
        );
        journal.save(&path).unwrap();
        assert_eq!(UpdateJournal::load(&path).unwrap().to_version, "1.0.1");
        journal.transition(JournalState::BackupComplete).unwrap();
        assert_eq!(journal.recovery_action(), RecoveryAction::Rollback);
        journal.transition(JournalState::Ready).unwrap();
        assert_eq!(journal.recovery_action(), RecoveryAction::Cleanup);
    }

    #[test]
    fn invalid_protocol_is_rejected() {
        let mut journal = UpdateJournal::new(
            "1.0.0",
            "1.0.1",
            Path::new("app"),
            Path::new("staging"),
            Path::new("rollback"),
            Path::new("helper"),
            1,
            "token",
        );
        journal.protocol = 99;
        assert!(journal.validate().is_err());
    }
    #[test]
    fn covers_recovery_actions_and_failure_state() {
        let mut journal = UpdateJournal::new(
            "1.0.0",
            "1.0.1",
            Path::new("app"),
            Path::new("staging"),
            Path::new("rollback"),
            Path::new("helper"),
            1,
            "token",
        );
        let cases = [
            (JournalState::Prepared, RecoveryAction::CleanStaging),
            (JournalState::ParentExited, RecoveryAction::Retry),
            (JournalState::LockAcquired, RecoveryAction::Retry),
            (JournalState::BackupComplete, RecoveryAction::Rollback),
            (JournalState::ReplaceStarted, RecoveryAction::Rollback),
            (JournalState::ReplaceComplete, RecoveryAction::Rollback),
            (
                JournalState::LaunchStarted,
                RecoveryAction::LaunchOrRollback,
            ),
            (JournalState::Ready, RecoveryAction::Cleanup),
            (JournalState::Completed, RecoveryAction::Cleanup),
            (JournalState::RollbackStarted, RecoveryAction::Rollback),
            (JournalState::RolledBack, RecoveryAction::Rollback),
            (JournalState::Failed, RecoveryAction::ManualRepair),
        ];
        for (state, action) in cases {
            journal.state = state;
            assert_eq!(journal.recovery_action(), action);
        }
        journal.fail("boom").unwrap();
        assert_eq!(journal.state, JournalState::Failed);
        assert_eq!(journal.error.as_deref(), Some("boom"));
    }

    #[test]
    fn rejects_blank_versions() {
        let mut journal = UpdateJournal::new(
            "1.0.0",
            "1.0.1",
            Path::new("app"),
            Path::new("staging"),
            Path::new("rollback"),
            Path::new("helper"),
            1,
            "token",
        );
        journal.from_version.clear();
        assert!(journal.validate().is_err());
        journal.from_version = "1.0.0".into();
        journal.to_version.clear();
        assert!(journal.validate().is_err());
    }
}
