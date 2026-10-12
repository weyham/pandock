//! Pandock NativeSync core. The public API is platform-neutral; Windows
//! Cloud Files API calls are isolated in `platform` so the state machine and
//! SQLite contract remain testable on every target.

mod backend;
mod db;
mod hydration;
mod manager;
mod model;
mod notify;
mod path;
mod platform;

pub use backend::{CloudBackend, SnapshotBackend};
pub use model::{NativeSyncChangeEvent, NativeSyncEvent};
pub use notify::{resolve_notifications_enabled, NotifySource, NOTIFICATIONS_ENV};

pub use manager::{NativeSyncError, NativeSyncManager};
pub use model::{
    EntryAction, NativeSyncAction, NativeSyncEntryView, NativeSyncListView, NativeSyncState,
    NativeSyncStatusView, PinState, ProgressView,
};
pub use path::{default_root_path, sanitize_component, validate_root_path};
