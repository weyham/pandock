use dav_server::fs::FsError;

use crate::cloud::{CloudError, CloudErrorKind};

pub fn to_fs_error(error: CloudError) -> FsError {
    match error.kind {
        CloudErrorKind::NotFound => FsError::NotFound,
        CloudErrorKind::AlreadyExists => FsError::Exists,
        CloudErrorKind::PermissionDenied | CloudErrorKind::Authentication => FsError::Forbidden,
        CloudErrorKind::QuotaExceeded => FsError::InsufficientStorage,
        CloudErrorKind::Unsupported => FsError::NotImplemented,
        CloudErrorKind::InvalidRequest => FsError::GeneralFailure,
        CloudErrorKind::ParentMissing => FsError::GeneralFailure,
        CloudErrorKind::RateLimited | CloudErrorKind::Transient | CloudErrorKind::Unknown => {
            FsError::GeneralFailure
        }
    }
}
