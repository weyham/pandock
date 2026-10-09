//! Shared integrity helpers used by the Velopack-era update flows.
//!
//! The minisign manifest verification of the pre-Velopack custom channel
//! was removed with ADR-106; what remains is SHA-256/size verification for
//! downloaded packages and the install allowlist used by the portable
//! helper swap.

use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::update::UpdateError;

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex::encode(digest)
}

pub fn verify_sha256(data: &[u8], expected_hex: &str) -> Result<(), UpdateError> {
    let actual = sha256_hex(data);
    if !actual.eq_ignore_ascii_case(expected_hex.trim()) {
        return Err(UpdateError::HashMismatch);
    }
    Ok(())
}

pub fn verify_size(data_len: usize, expected_size: u64) -> Result<(), UpdateError> {
    if expected_size != 0 && data_len as u64 != expected_size {
        return Err(UpdateError::InvalidArchive(format!(
            "下载大小 {} 与清单大小 {} 不一致",
            data_len, expected_size
        )));
    }
    Ok(())
}

pub fn default_install_allowlist(helper_path: Option<&str>) -> HashSet<String> {
    // 1.2.0 起更新链全部为 pandock.exe（ADR-104 双名过渡随开源迁移结束）。
    let mut allowed = HashSet::from([
        "pandock.exe".to_string(),
        "WebView2Loader.dll".to_string(),
        "VERSION.txt".to_string(),
        "LICENSE.txt".to_string(),
    ]);
    if let Some(helper_path) = helper_path {
        allowed.insert(helper_path.replace('\\', "/"));
    }
    allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_and_size_checks_are_enforced() {
        let data = b"pandock";
        let digest = sha256_hex(data);
        assert!(verify_sha256(data, &digest).is_ok());
        assert!(verify_sha256(data, &"0".repeat(64)).is_err());
        assert!(verify_size(data.len(), data.len() as u64).is_ok());
        assert!(verify_size(data.len(), data.len() as u64 + 1).is_err());
    }

    #[test]
    fn allowlist_accepts_app_files_and_helper() {
        let allowed = default_install_allowlist(Some("updater.exe"));
        assert!(allowed.contains("pandock.exe"));
        assert!(allowed.contains("WebView2Loader.dll"));
        assert!(allowed.contains("updater.exe"));
    }
}
