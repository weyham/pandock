use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NativeSyncState {
    #[default]
    #[serde(rename = "online_only")]
    OnlineOnly,
    #[serde(rename = "hydrating")]
    Hydrating,
    #[serde(rename = "hydrated")]
    Hydrated,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "remote_missing")]
    RemoteMissing,
    #[serde(rename = "stale")]
    Stale,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PinState {
    Pinned,
    #[default]
    Unpinned,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressView {
    pub completed: u64,
    pub total: u64,
}

/// hydration job 终态事件（完成/失败才推送，进度类高频变化不推送）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSyncEvent {
    pub fs_id: u64,
    pub success: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSyncStatusView {
    pub enabled: bool,
    pub supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub support_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_path: Option<String>,
    pub state: String,
    pub total_entries: u64,
    pub placeholder_entries: u64,
    pub hydrated_entries: u64,
    pub attention_entries: u64,
    pub active_jobs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSyncEntryView {
    pub fs_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_fs_id: Option<u64>,
    pub name: String,
    pub relative_path: String,
    pub is_dir: bool,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<i64>,
    pub state: NativeSyncState,
    pub pin_state: PinState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub has_children: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSyncListView {
    pub entries: Vec<NativeSyncEntryView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub total: u64,
    pub parent_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSyncAction {
    pub relative_path: String,
    pub action: EntryAction,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EntryAction {
    Pin,
    Unpin,
    Dehydrate,
    Retry,
    #[serde(rename = "ack_remote_missing")]
    AckRemoteMissing,
}

#[derive(Clone, Debug)]
pub(crate) struct EntryRecord {
    pub fs_id: u64,
    pub parent_fs_id: Option<u64>,
    pub remote_path: String,
    pub relative_path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub md5: Option<String>,
    pub modified_at: Option<i64>,
    pub state: NativeSyncState,
    pub pin_state: PinState,
    pub error: Option<String>,
    pub has_children: bool,
}

impl std::fmt::Display for NativeSyncState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OnlineOnly => "online_only",
            Self::Hydrating => "hydrating",
            Self::Hydrated => "hydrated",
            Self::Error => "error",
            Self::RemoteMissing => "remote_missing",
            Self::Stale => "stale",
        })
    }
}
impl std::fmt::Display for PinState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pinned => "pinned",
            Self::Unpinned => "unpinned",
        })
    }
}

#[cfg(test)]
mod serde_tests {
    use super::*;

    #[test]
    fn entry_view_serializes_epoch_seconds_and_progress_object() {
        let view = NativeSyncEntryView {
            fs_id: 7,
            parent_fs_id: Some(1),
            name: "file.txt".into(),
            relative_path: "file.txt".into(),
            is_dir: false,
            size: 100,
            modified_at: Some(1_700_000_000),
            state: NativeSyncState::Hydrating,
            pin_state: PinState::Pinned,
            progress: Some(ProgressView {
                completed: 25,
                total: 100,
            }),
            error: None,
            has_children: false,
        };
        let json = serde_json::to_value(view).unwrap();
        assert_eq!(json["modifiedAt"], 1_700_000_000);
        assert_eq!(json["progress"]["completed"], 25);
        assert_eq!(json["progress"]["total"], 100);
    }

    #[test]
    fn actions_include_unpin_and_preserve_ack_name() {
        assert_eq!(
            serde_json::to_string(&EntryAction::Unpin).unwrap(),
            "\"unpin\""
        );
        assert_eq!(
            serde_json::to_string(&EntryAction::AckRemoteMissing).unwrap(),
            "\"ack_remote_missing\""
        );
    }
}
