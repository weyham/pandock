use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloudErrorKind {
    NotFound,
    AlreadyExists,
    ParentMissing,
    PermissionDenied,
    Authentication,
    QuotaExceeded,
    RateLimited,
    Transient,
    InvalidRequest,
    Unsupported,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct CloudError {
    pub kind: CloudErrorKind,
    pub raw_errno: Option<i64>,
    pub http_status: Option<u16>,
    pub request_id: Option<String>,
    pub retryable: bool,
    pub message: String,
}

impl CloudError {
    pub fn new(kind: CloudErrorKind, message: impl Into<String>) -> Self {
        let retryable = matches!(
            kind,
            CloudErrorKind::RateLimited | CloudErrorKind::Transient
        );
        Self {
            kind,
            raw_errno: None,
            http_status: None,
            request_id: None,
            retryable,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(CloudErrorKind::NotFound, message)
    }

    pub fn already_exists(message: impl Into<String>) -> Self {
        Self::new(CloudErrorKind::AlreadyExists, message)
    }

    pub fn parent_missing(message: impl Into<String>) -> Self {
        Self::new(CloudErrorKind::ParentMissing, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(CloudErrorKind::PermissionDenied, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(CloudErrorKind::Unsupported, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(CloudErrorKind::InvalidRequest, message)
    }

    pub fn transient(message: impl Into<String>) -> Self {
        Self::new(CloudErrorKind::Transient, message)
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self::new(CloudErrorKind::Unknown, message)
    }

    pub fn from_message(message: String) -> Self {
        let mut error = Self::unknown(message.clone());
        error.message = message.clone();

        if let Some(json) = extract_json(&message) {
            error.raw_errno = json
                .get("errno")
                .and_then(|value| value.as_i64())
                .or_else(|| json.get("error_code").and_then(|value| value.as_i64()));
            error.request_id = json
                .get("request_id")
                .and_then(|value| value.as_str().map(str::to_string))
                .or_else(|| {
                    json.get("request_id")
                        .and_then(|value| value.as_i64())
                        .map(|value| value.to_string())
                });
            error.kind = classify_json_error(&json, &message);
        } else if message.contains("429") || message.contains("rate limit") {
            error.kind = CloudErrorKind::RateLimited;
            error.http_status = Some(429);
        } else if message.contains("500")
            || message.contains("502")
            || message.contains("503")
            || message.contains("504")
            || message.contains("超时")
            || message.contains("timeout")
            || message.contains("网络")
        {
            error.kind = CloudErrorKind::Transient;
        }

        error.retryable = matches!(
            error.kind,
            CloudErrorKind::RateLimited | CloudErrorKind::Transient
        );
        error
    }

    pub fn with_baidu_errno(mut self, errno: i64) -> Self {
        self.raw_errno = Some(errno);
        self.kind = classify_errno(errno, &self.message);
        self.retryable = matches!(
            self.kind,
            CloudErrorKind::RateLimited | CloudErrorKind::Transient
        );
        self
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self.kind, CloudErrorKind::NotFound)
    }

    pub fn is_parent_missing(&self) -> bool {
        matches!(self.kind, CloudErrorKind::ParentMissing)
    }
}

fn extract_json(message: &str) -> Option<serde_json::Value> {
    let start = message.find('{')?;
    serde_json::from_str(&message[start..]).ok()
}

fn classify_json_error(json: &serde_json::Value, message: &str) -> CloudErrorKind {
    if let Some(errno) = json
        .get("errno")
        .and_then(|value| value.as_i64())
        .or_else(|| json.get("error_code").and_then(|value| value.as_i64()))
    {
        let nested = json
            .get("info")
            .and_then(|value| value.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item.get("errno"))
            .and_then(|value| value.as_i64());
        return classify_errno(nested.unwrap_or(errno), message);
    }
    CloudErrorKind::Unknown
}

fn classify_errno(errno: i64, message: &str) -> CloudErrorKind {
    match errno {
        -9 | 31066 => CloudErrorKind::NotFound,
        -8 => CloudErrorKind::AlreadyExists,
        -10 => CloudErrorKind::QuotaExceeded,
        -7 => {
            if message.contains("权限") {
                CloudErrorKind::PermissionDenied
            } else {
                CloudErrorKind::InvalidRequest
            }
        }
        _ => CloudErrorKind::Unknown,
    }
}

impl fmt::Display for CloudError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CloudError {}

pub fn unix_time(seconds: Option<i64>) -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_secs(seconds.unwrap_or(0).max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_baidu_errors() {
        assert_eq!(
            CloudError::from_message("百度 API 错误：{\"errno\":-9}".into()).kind,
            CloudErrorKind::NotFound
        );
        assert_eq!(
            CloudError::from_message("百度 API 错误：{\"errno\":31066}".into()).kind,
            CloudErrorKind::NotFound
        );
        assert_eq!(
            CloudError::from_message("百度 API 错误：{\"errno\":-8}".into()).kind,
            CloudErrorKind::AlreadyExists
        );
        assert_eq!(
            CloudError::from_message("百度 API 错误：{\"errno\":-10}".into()).kind,
            CloudErrorKind::QuotaExceeded
        );
    }

    #[test]
    fn classifies_nested_item_errors_and_transport_failures() {
        let error = CloudError::from_message(
            "百度 API 错误：{\"errno\":0,\"info\":[{\"errno\":31066}]}".into(),
        );
        assert_eq!(error.kind, CloudErrorKind::NotFound);
        assert!(!error.retryable);

        let error = CloudError::from_message("HTTP 429 Too Many Requests".into());
        assert_eq!(error.kind, CloudErrorKind::RateLimited);
        assert!(error.retryable);
    }
}
