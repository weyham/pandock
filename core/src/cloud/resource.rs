use serde::{Deserialize, Serialize};
use std::time::SystemTime;

use super::error::unix_time;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemotePath(String);

impl RemotePath {
    pub fn parse(raw: &str) -> Result<Self, super::CloudError> {
        let trimmed = raw.trim();
        if !trimmed.starts_with('/') {
            return Err(super::CloudError::invalid("远端路径必须以 / 开头"));
        }
        if trimmed.contains('\0') {
            return Err(super::CloudError::invalid("远端路径包含 NUL"));
        }
        let mut parts = Vec::new();
        for part in trimmed.split('/') {
            if part.is_empty() {
                continue;
            }
            if part == "." || part == ".." {
                return Err(super::CloudError::invalid("远端路径包含非法组件"));
            }
            parts.push(part.to_string());
        }
        if parts.is_empty() {
            Ok(Self("/".to_string()))
        } else {
            Ok(Self(format!("/{}", parts.join("/"))))
        }
    }

    pub fn root() -> Self {
        Self("/".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() {
            None
        } else {
            self.0.rsplit('/').next()
        }
    }

    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        let parent = self
            .0
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("/");
        Some(Self(if parent.is_empty() {
            "/".into()
        } else {
            parent.into()
        }))
    }

    pub fn join(&self, child: &str) -> Result<Self, super::CloudError> {
        if child.is_empty() || child.contains('/') || child == "." || child == ".." {
            return Err(super::CloudError::invalid("非法子路径"));
        }
        if self.is_root() {
            Self::parse(&format!("/{child}"))
        } else {
            Self::parse(&format!("{}/{child}", self.0))
        }
    }
}

impl std::fmt::Display for RemotePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceKind {
    File,
    Collection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub weak: bool,
    pub value: String,
}

impl Revision {
    pub fn from_fields(
        kind: ResourceKind,
        fs_id: Option<u64>,
        size: u64,
        modified: i64,
        md5: Option<&str>,
    ) -> Option<Self> {
        let fs_id = fs_id?;
        let prefix = match kind {
            ResourceKind::File => "f",
            ResourceKind::Collection => "d",
        };
        let md5 = md5.unwrap_or("");
        let value = format!("{prefix}-{fs_id}-{size}-{modified}-{md5}");
        Some(Self { weak: true, value })
    }
}

impl std::fmt::Display for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.weak {
            write!(f, "W/\"{}\"", self.value)
        } else {
            write!(f, "\"{}\"", self.value)
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResourceSnapshot {
    pub path: RemotePath,
    pub kind: ResourceKind,
    pub size: u64,
    pub modified: SystemTime,
    pub fs_id: Option<u64>,
    pub md5: Option<String>,
    pub server_filename: Option<String>,
    pub content_type: Option<String>,
    pub revision: Option<Revision>,
    pub dlink: Option<String>,
}

impl ResourceSnapshot {
    pub fn is_dir(&self) -> bool {
        self.kind == ResourceKind::Collection
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_entry(
        path: RemotePath,
        fs_id: Option<u64>,
        size: u64,
        isdir: u8,
        modified: Option<i64>,
        md5: Option<String>,
        server_filename: Option<String>,
        content_type: Option<String>,
        dlink: Option<String>,
    ) -> Self {
        let kind = if isdir != 0 {
            ResourceKind::Collection
        } else {
            ResourceKind::File
        };
        let modified_seconds = modified.unwrap_or(0);
        let revision = Revision::from_fields(kind, fs_id, size, modified_seconds, md5.as_deref());
        Self {
            path,
            kind,
            size,
            modified: unix_time(modified),
            fs_id,
            md5,
            server_filename,
            content_type,
            revision,
            dlink,
        }
    }

    pub fn synthetic_dir(path: RemotePath) -> Self {
        Self {
            path,
            kind: ResourceKind::Collection,
            size: 0,
            modified: SystemTime::UNIX_EPOCH,
            fs_id: None,
            md5: None,
            server_filename: None,
            content_type: None,
            revision: None,
            dlink: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::CloudErrorKind;

    #[test]
    fn normalizes_root_and_nested_paths() {
        assert_eq!(RemotePath::parse("/").unwrap().as_str(), "/");
        assert_eq!(RemotePath::parse("/a//b/").unwrap().as_str(), "/a/b");
        assert_eq!(
            RemotePath::parse("/a/b")
                .unwrap()
                .parent()
                .unwrap()
                .as_str(),
            "/a"
        );
        assert_eq!(RemotePath::parse("/a/b").unwrap().file_name(), Some("b"));
    }

    #[test]
    fn rejects_traversal_and_nul() {
        assert_eq!(
            RemotePath::parse("/a/../b").unwrap_err().kind,
            CloudErrorKind::InvalidRequest
        );
        assert!(RemotePath::parse("/a\0b").is_err());
    }

    #[test]
    fn revision_is_stable_from_baidu_fields() {
        let revision =
            Revision::from_fields(ResourceKind::File, Some(7), 42, 100, Some("abc")).unwrap();
        assert_eq!(revision.to_string(), "W/\"f-7-42-100-abc\"");
        let same =
            Revision::from_fields(ResourceKind::File, Some(7), 42, 100, Some("abc")).unwrap();
        assert_eq!(revision, same);
    }
}
