//! Update primitives shared by the Velopack (installed) and portable
//! (helper + journal) update channels.
//!
//! Pandock is distributed from a public GitHub repository. Release
//! discovery and asset downloads use the anonymous
//! `releases/latest/download/` CDN routes (see `cdn`); there is no
//! GitHub App, OAuth device flow, or stored credential involved.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod cdn;
pub mod install;
pub mod journal;
pub mod velopack_feed;
pub mod verify;

pub const UPDATE_PROTOCOL: u32 = 1;
pub const UPDATE_SCHEMA: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateSourceErrorCode {
    Forbidden,
    NotFound,
    RateLimited,
    NetworkUnreachable,
    InvalidManifest,
    HashMismatch,
    DowngradeRejected,
    InvalidArchive,
    UnsupportedPlatform,
    UpdaterBusy,
    Internal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSourceError {
    pub code: UpdateSourceErrorCode,
    pub message: String,
    pub retryable: bool,
    pub retry_after_seconds: Option<u64>,
    pub status: Option<u16>,
    pub host: Option<String>,
}

impl UpdateSourceError {
    pub fn new(code: UpdateSourceErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            retry_after_seconds: None,
            status: None,
            host: None,
        }
    }

    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }
}

#[derive(Clone, Debug, Error)]
pub enum UpdateError {
    #[error("更新服务请求被限流")]
    RateLimited(Option<u64>),
    #[error("更新服务拒绝了请求（HTTP 403）")]
    Forbidden,
    #[error("更新资源不存在（HTTP 404）")]
    NotFound,
    #[error("网络不可达：{0}")]
    Network(String),
    #[error("更新清单无效：{0}")]
    InvalidManifest(String),
    #[error("SHA-256 校验失败")]
    HashMismatch,
    #[error("拒绝降级更新")]
    DowngradeRejected,
    #[error("更新包格式无效：{0}")]
    InvalidArchive(String),
    #[error("当前平台不支持自动更新")]
    UnsupportedPlatform,
    #[error("更新器正忙")]
    UpdaterBusy,
    #[error("本地更新状态错误：{0}")]
    Internal(String),
}

impl UpdateError {
    pub fn diagnostic_log(&self) -> String {
        let view = self.view();
        format!(
            "code={:?} status={:?} host={:?} retryable={} retry_after_seconds={:?} message={}",
            view.code,
            view.status,
            view.host,
            view.retryable,
            view.retry_after_seconds,
            view.message
        )
    }

    pub fn view(&self) -> UpdateSourceError {
        match self {
            Self::RateLimited(seconds) => {
                let error = UpdateSourceError::new(
                    UpdateSourceErrorCode::RateLimited,
                    self.to_string(),
                    true,
                );
                seconds.map_or(error.clone(), |value| error.with_retry_after(value))
            }
            Self::Forbidden => {
                UpdateSourceError::new(UpdateSourceErrorCode::Forbidden, self.to_string(), false)
            }
            Self::NotFound => {
                UpdateSourceError::new(UpdateSourceErrorCode::NotFound, self.to_string(), false)
            }
            Self::Network(message) => UpdateSourceError::new(
                UpdateSourceErrorCode::NetworkUnreachable,
                message.clone(),
                true,
            ),
            Self::InvalidManifest(message) => UpdateSourceError::new(
                UpdateSourceErrorCode::InvalidManifest,
                message.clone(),
                false,
            ),
            Self::HashMismatch => {
                UpdateSourceError::new(UpdateSourceErrorCode::HashMismatch, self.to_string(), false)
            }
            Self::DowngradeRejected => UpdateSourceError::new(
                UpdateSourceErrorCode::DowngradeRejected,
                self.to_string(),
                false,
            ),
            Self::InvalidArchive(message) => UpdateSourceError::new(
                UpdateSourceErrorCode::InvalidArchive,
                message.clone(),
                false,
            ),
            Self::UnsupportedPlatform => UpdateSourceError::new(
                UpdateSourceErrorCode::UnsupportedPlatform,
                self.to_string(),
                false,
            ),
            Self::UpdaterBusy => {
                UpdateSourceError::new(UpdateSourceErrorCode::UpdaterBusy, self.to_string(), true)
            }
            Self::Internal(message) => {
                UpdateSourceError::new(UpdateSourceErrorCode::Internal, message.clone(), true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_view_carries_retry_after_for_rate_limit() {
        let view = UpdateError::RateLimited(Some(30)).view();
        assert_eq!(view.code, UpdateSourceErrorCode::RateLimited);
        assert_eq!(view.retry_after_seconds, Some(30));
        assert!(view.retryable);
    }

    #[test]
    fn error_view_marks_integrity_failures_non_retryable() {
        assert!(!UpdateError::HashMismatch.view().retryable);
        assert!(!UpdateError::DowngradeRejected.view().retryable);
        assert!(UpdateError::Network("x".into()).view().retryable);
    }
}
