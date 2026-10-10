//! In-app update runtime backed by Velopack over the public GitHub
//! Releases CDN.
//!
//! Release discovery and downloads are fully anonymous: the Velopack
//! (installed) channel reads `releases.win.json` through an
//! `HttpSource` pointed at `releases/latest/download/`, and the
//! portable channel fetches the same feed via `ReleaseCdn`. Instances
//! that were not installed by Velopack (portable / dev builds) fall back
//! to the portable channel; platforms without an updater asset are
//! reported as manual-download mode.

use pandock_core::update::cdn::{releases_page_url, ReleaseCdn};
use pandock_core::update::velopack_feed::{
    extract_app_files, parse_feed, select_update, verify_package, FeedAsset, FEED_ASSET_NAME,
    PACKAGE_ID,
};
use pandock_core::update::{UpdateError, UpdateSourceError, UpdateSourceErrorCode};
use semver::Version;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
#[cfg(target_os = "windows")]
use velopack::{sources::HttpSource, UpdateCheck, UpdateInfo, UpdateManager};

const REPO_OWNER: &str = "weyham";
const REPO_NAME: &str = "pandock";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    Idle,
    Checking,
    UpToDate,
    UpdateAvailable,
    Downloading,
    ReadyToInstall,
    Installing,
    Completed,
    ManualDownload,
    Error,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStateView {
    pub phase: UpdatePhase,
    pub current_version: String,
    pub available_version: Option<String>,
    pub release_url: Option<String>,
    pub manual_only: bool,
    pub error: Option<UpdateSourceError>,
}

#[derive(Default)]
struct RuntimeState {
    phase: Option<UpdatePhase>,
    available_version: Option<String>,
    release_url: Option<String>,
    manual_only: bool,
    error: Option<UpdateSourceError>,
    update: Option<PendingUpdate>,
}

#[derive(Clone)]
enum PendingUpdate {
    #[cfg(target_os = "windows")]
    Velopack(Box<UpdateInfo>),
    Portable(Box<PortableOffer>),
}

#[derive(Clone)]
struct PortableOffer {
    asset: FeedAsset,
    staged: Option<PortableStaged>,
}

#[derive(Clone)]
pub struct PortableStaged {
    pub staging_dir: PathBuf,
    pub helper_path: PathBuf,
    pub version: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingKind {
    None,
    Velopack,
    Portable,
}

const MAX_PACKAGE_SIZE: u64 = 512 * 1024 * 1024;
const MAX_FEED_SIZE: u64 = 4 * 1024 * 1024;

fn cdn() -> Result<ReleaseCdn, UpdateError> {
    ReleaseCdn::new(REPO_OWNER, REPO_NAME)
}

#[cfg(target_os = "windows")]
fn build_manager() -> Result<UpdateManager, velopack::Error> {
    let source = HttpSource::new(format!(
        "https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/latest/download"
    ));
    UpdateManager::new(source, None, None)
}

#[cfg(target_os = "windows")]
fn map_velopack_error(error: velopack::Error) -> UpdateError {
    match error {
        velopack::Error::Network(network) => {
            let message = network.to_string();
            if message.contains("403") || message.contains("429") {
                UpdateError::RateLimited(None)
            } else {
                UpdateError::Network(message)
            }
        }
        other => UpdateError::Internal(other.to_string()),
    }
}

pub struct UpdateRuntime {
    current_version: String,
    state: Mutex<RuntimeState>,
}

impl UpdateRuntime {
    pub fn new(current_version: impl Into<String>) -> Result<Self, String> {
        Ok(Self {
            current_version: current_version.into(),
            state: Mutex::new(RuntimeState::default()),
        })
    }

    pub async fn view(&self) -> UpdateStateView {
        let state = self.state.lock().unwrap();
        UpdateStateView {
            phase: state.phase.unwrap_or(UpdatePhase::Idle),
            current_version: self.current_version.clone(),
            available_version: state.available_version.clone(),
            release_url: state.release_url.clone(),
            manual_only: state.manual_only,
            error: state.error.clone(),
        }
    }

    /// Synchronous state mutation helpers: each locks briefly and returns,
    /// so no `MutexGuard` ever lives across an `.await` point.
    fn set_up_to_date(&self) {
        let mut state = self.state.lock().unwrap();
        state.phase = Some(UpdatePhase::UpToDate);
        state.available_version = None;
        state.release_url = None;
        state.manual_only = false;
        state.error = None;
        state.update = None;
    }

    #[cfg(target_os = "windows")]
    fn set_velopack_offer(&self, info: Box<UpdateInfo>) {
        let mut state = self.state.lock().unwrap();
        state.phase = Some(UpdatePhase::UpdateAvailable);
        state.available_version = Some(info.TargetFullRelease.Version.clone());
        state.release_url = Some(releases_page_url(REPO_OWNER, REPO_NAME));
        state.manual_only = false;
        state.error = None;
        state.update = Some(PendingUpdate::Velopack(info));
    }

    fn set_portable_offer(&self, offer: PortableOffer) {
        let mut state = self.state.lock().unwrap();
        state.phase = Some(UpdatePhase::UpdateAvailable);
        state.available_version = Some(offer.asset.version.to_string());
        state.release_url = Some(releases_page_url(REPO_OWNER, REPO_NAME));
        state.manual_only = false;
        state.error = None;
        state.update = Some(PendingUpdate::Portable(Box::new(offer)));
    }

    fn set_manual_download(&self, version: String) {
        let mut state = self.state.lock().unwrap();
        state.phase = Some(UpdatePhase::ManualDownload);
        state.available_version = Some(version);
        state.release_url = Some(releases_page_url(REPO_OWNER, REPO_NAME));
        state.manual_only = true;
        state.error = None;
        state.update = None;
    }

    #[cfg(target_os = "windows")]
    pub async fn check(&self) -> Result<UpdateStateView, UpdateError> {
        {
            let mut state = self.state.lock().unwrap();
            state.phase = Some(UpdatePhase::Checking);
            state.error = None;
        }
        let result = tokio::task::spawn_blocking(move || {
            build_manager().and_then(|manager| manager.check_for_updates())
        })
        .await
        .map_err(|error| UpdateError::Internal(error.to_string()))?;
        match result {
            Ok(UpdateCheck::UpdateAvailable(info)) => {
                self.set_velopack_offer(info);
            }
            Ok(UpdateCheck::NoUpdateAvailable) | Ok(UpdateCheck::RemoteIsEmpty) => {
                self.set_up_to_date();
            }
            Err(velopack::Error::NotInstalled(_)) => {
                return self.portable_check().await;
            }
            Err(error) => {
                return Err(map_velopack_error(error));
            }
        }
        Ok(self.view().await)
    }

    /// Non-Windows platforms have no Velopack and no portable helper:
    /// check the CDN feed and surface a manual-download hint.
    #[cfg(not(target_os = "windows"))]
    pub async fn check(&self) -> Result<UpdateStateView, UpdateError> {
        {
            let mut state = self.state.lock().unwrap();
            state.phase = Some(UpdatePhase::Checking);
            state.error = None;
        }
        self.portable_check().await
    }

    /// Update check for portable (non-Velopack) instances: read the
    /// Velopack feed from the latest release's CDN route and pick a newer
    /// full package.
    async fn portable_check(&self) -> Result<UpdateStateView, UpdateError> {
        let download = cdn()?
            .download_latest_asset(FEED_ASSET_NAME, MAX_FEED_SIZE)
            .await?;
        let Some(download) = download else {
            // Latest release carries no Velopack feed: nothing to compare
            // against.
            self.set_up_to_date();
            return Ok(self.view().await);
        };
        let assets = parse_feed(&download.bytes)?;
        let current = Version::parse(&self.current_version)
            .map_err(|error| UpdateError::Internal(format!("当前版本号无效: {error}")))?;
        match select_update(&assets, PACKAGE_ID, &current) {
            Some(asset) => {
                if cfg!(target_os = "windows") {
                    self.set_portable_offer(PortableOffer {
                        asset,
                        staged: None,
                    });
                } else {
                    // Non-Windows platforms have no portable updater
                    // helper: surface the new version and point the user
                    // at the releases page instead.
                    self.set_manual_download(asset.version.to_string());
                }
            }
            None => {
                self.set_up_to_date();
            }
        }
        Ok(self.view().await)
    }

    pub async fn download(&self) -> Result<UpdateStateView, UpdateError> {
        let pending = {
            let state = self.state.lock().unwrap();
            state
                .update
                .clone()
                .ok_or_else(|| UpdateError::Internal("请先检查更新并在有可用版本时下载".into()))?
        };
        {
            let mut state = self.state.lock().unwrap();
            state.phase = Some(UpdatePhase::Downloading);
            state.error = None;
        }
        match pending {
            #[cfg(target_os = "windows")]
            PendingUpdate::Velopack(info) => {
                tokio::task::spawn_blocking(move || {
                    build_manager().and_then(|manager| manager.download_updates(&info, None))
                })
                .await
                .map_err(|error| UpdateError::Internal(error.to_string()))?
                .map_err(map_velopack_error)?;
            }
            PendingUpdate::Portable(offer) => {
                self.download_portable(&offer).await?;
            }
        }
        {
            let mut state = self.state.lock().unwrap();
            state.phase = Some(UpdatePhase::ReadyToInstall);
            state.error = None;
        }
        Ok(self.view().await)
    }

    async fn download_portable(&self, offer: &PortableOffer) -> Result<(), UpdateError> {
        let download = cdn()?
            .download_latest_asset(&offer.asset.file_name, MAX_PACKAGE_SIZE)
            .await?
            .ok_or_else(|| {
                UpdateError::Internal(format!("Release 缺少更新包 {}", offer.asset.file_name))
            })?;
        verify_package(&download.bytes, &offer.asset.sha256, offer.asset.size)?;
        let exe_dir = std::env::current_exe()
            .map_err(|error| UpdateError::Internal(error.to_string()))?
            .parent()
            .ok_or_else(|| UpdateError::Internal("无法定位应用目录".into()))?
            .to_path_buf();
        let staging_dir = exe_dir.join("updates").join("staging");
        if staging_dir.exists() {
            std::fs::remove_dir_all(&staging_dir)
                .map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        extract_app_files(&download.bytes, &staging_dir)?;
        let helper_path = staging_dir.join("updater.exe");
        if !helper_path.exists() {
            return Err(UpdateError::InvalidArchive("更新包缺少 updater.exe".into()));
        }
        // nupkg 不含 VERSION.txt（它由打包脚本写给便携 zip）。为保持安装目录
        // 版本标记不滞后，按目标版本生成进 staging，随白名单一起落位。
        std::fs::write(
            staging_dir.join("VERSION.txt"),
            format!("{}\r\n", offer.asset.version),
        )
        .map_err(|error| UpdateError::Internal(error.to_string()))?;
        let mut state = self.state.lock().unwrap();
        if let Some(PendingUpdate::Portable(offer)) = state.update.as_mut() {
            offer.staged = Some(PortableStaged {
                staging_dir,
                helper_path,
                version: offer.asset.version.to_string(),
            });
        }
        Ok(())
    }

    pub fn pending_kind(&self) -> PendingKind {
        let state = self.state.lock().unwrap();
        #[allow(unreachable_patterns)]
        match state.update {
            #[cfg(target_os = "windows")]
            Some(PendingUpdate::Velopack(_)) => PendingKind::Velopack,
            Some(PendingUpdate::Portable(_)) => PendingKind::Portable,
            None => PendingKind::None,
        }
    }

    pub fn portable_staged(&self) -> Option<PortableStaged> {
        let state = self.state.lock().unwrap();
        match state.update.as_ref() {
            Some(PendingUpdate::Portable(offer)) => offer.staged.clone(),
            _ => None,
        }
    }

    /// Apply the downloaded update and restart. This launches the Velopack
    /// updater and exits the current process on success.
    #[cfg(target_os = "windows")]
    pub async fn install(&self) -> Result<(), UpdateError> {
        let info = {
            let state = self.state.lock().unwrap();
            match state.update.clone() {
                Some(PendingUpdate::Velopack(info)) => info,
                _ => return Err(UpdateError::Internal("没有已下载的 Velopack 更新".into())),
            }
        };
        self.mark_installing();
        tokio::task::spawn_blocking(move || {
            build_manager()
                .and_then(|manager| manager.apply_updates_and_restart(&info.TargetFullRelease))
        })
        .await
        .map_err(|error| UpdateError::Internal(error.to_string()))?
        .map_err(map_velopack_error)
    }

    #[cfg(not(target_os = "windows"))]
    pub async fn install(&self) -> Result<(), UpdateError> {
        Err(UpdateError::UnsupportedPlatform)
    }

    pub fn mark_installing(&self) {
        self.state.lock().unwrap().phase = Some(UpdatePhase::Installing);
    }

    pub fn mark_completed(&self) {
        self.state.lock().unwrap().phase = Some(UpdatePhase::Completed);
    }

    pub fn set_error(&self, error: UpdateError) {
        log::warn!(target: "pandock::update", "{}", error.diagnostic_log());
        let mut state = self.state.lock().unwrap();
        state.phase = Some(UpdatePhase::Error);
        state.error = Some(error.view());
    }

    pub fn mark_recovery_required(&self, message: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        state.phase = Some(UpdatePhase::Error);
        state.error = Some(UpdateSourceError::new(
            UpdateSourceErrorCode::Internal,
            message,
            true,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "windows")]
    #[test]
    fn velopack_source_points_at_cdn_latest_download() {
        // Constructing the manager must succeed without any credentials;
        // the feed URL is exercised by integration/e2e checks.
        let result = build_manager();
        assert!(result.is_ok() || matches!(result, Err(velopack::Error::NotInstalled(_))));
    }

    #[tokio::test]
    async fn initial_view_is_idle() {
        let runtime = UpdateRuntime::new("1.1.1").unwrap();
        let view = runtime.view().await;
        assert_eq!(view.phase, UpdatePhase::Idle);
        assert_eq!(view.current_version, "1.1.1");
        assert!(view.available_version.is_none());
        assert!(!view.manual_only);
    }

    #[tokio::test]
    async fn manual_download_state_has_no_pending_update() {
        let runtime = UpdateRuntime::new("1.1.1").unwrap();
        runtime.set_manual_download("1.2.0".to_string());
        let view = runtime.view().await;
        assert_eq!(view.phase, UpdatePhase::ManualDownload);
        assert_eq!(view.available_version.as_deref(), Some("1.2.0"));
        assert!(view.manual_only);
        assert_eq!(runtime.pending_kind(), PendingKind::None);
    }
}
