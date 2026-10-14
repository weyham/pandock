//! Velopack release feed support for portable (non-Velopack) instances.
//!
//! Velopack-installed instances use `velopack::UpdateManager` end to end.
//! Portable instances cannot (they lack the Velopack layout), so they read
//! the same published artifacts directly: the `releases.win.json` feed and
//! the full `.nupkg` package. The package is a plain zip whose `lib/app/`
//! entries contain the application files; only those are extracted, never
//! the Velopack plumbing (sq.version, Squirrel.exe, stubs), so a portable
//! directory never becomes detectable as a Velopack install.

use semver::Version;
use serde::Deserialize;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use super::verify::{verify_sha256, verify_size};
use super::UpdateError;

/// Asset name of the Velopack JSON feed inside a GitHub release.
pub const FEED_ASSET_NAME: &str = "releases.win.json";
/// Velopack package id used by `scripts/build-velopack.ps1`.
pub const PACKAGE_ID: &str = "Pandock";
/// Application files extracted from the nupkg for a portable update.
/// Velopack plumbing (sq.version, Squirrel.exe, *Stub.exe) is deliberately
/// excluded.
pub const APP_FILES: [&str; 2] = ["lib/app/pandock.exe", "lib/app/updater.exe"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedAsset {
    pub package_id: String,
    pub version: Version,
    pub asset_type: String,
    pub file_name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct FeedFile {
    #[serde(rename = "Assets", default)]
    assets: Vec<FeedEntry>,
}

#[derive(Deserialize)]
struct FeedEntry {
    #[serde(rename = "PackageId")]
    package_id: String,
    #[serde(rename = "Version")]
    version: String,
    #[serde(rename = "Type")]
    asset_type: String,
    #[serde(rename = "FileName")]
    file_name: String,
    #[serde(rename = "SHA256")]
    sha256: String,
    #[serde(rename = "Size")]
    size: u64,
}

pub fn parse_feed(bytes: &[u8]) -> Result<Vec<FeedAsset>, UpdateError> {
    let feed: FeedFile = serde_json::from_slice(bytes).map_err(|error| {
        UpdateError::InvalidManifest(format!("Velopack feed 解析失败: {error}"))
    })?;
    let mut assets = Vec::with_capacity(feed.assets.len());
    for entry in feed.assets {
        let version = Version::parse(&entry.version).map_err(|error| {
            UpdateError::InvalidManifest(format!(
                "Velopack feed 版本号无效 {}: {error}",
                entry.version
            ))
        })?;
        assets.push(FeedAsset {
            package_id: entry.package_id,
            version,
            asset_type: entry.asset_type,
            file_name: entry.file_name,
            sha256: entry.sha256,
            size: entry.size,
        });
    }
    Ok(assets)
}

/// Newest full package strictly newer than `current`, if any.
pub fn select_update(
    assets: &[FeedAsset],
    package_id: &str,
    current: &Version,
) -> Option<FeedAsset> {
    assets
        .iter()
        .filter(|asset| {
            asset.package_id == package_id
                && asset.asset_type.eq_ignore_ascii_case("Full")
                && asset.version > *current
        })
        .max_by(|left, right| left.version.cmp(&right.version))
        .cloned()
}

pub fn verify_package(
    bytes: &[u8],
    expected_sha256: &str,
    expected_size: u64,
) -> Result<(), UpdateError> {
    verify_size(bytes.len(), expected_size)?;
    verify_sha256(bytes, expected_sha256)
}

/// Extract the application files from a full nupkg into `staging_dir`,
/// returning the extracted paths in `APP_FILES` order.
pub fn extract_app_files(package: &[u8], staging_dir: &Path) -> Result<Vec<PathBuf>, UpdateError> {
    std::fs::create_dir_all(staging_dir)
        .map_err(|error| UpdateError::Internal(error.to_string()))?;
    let mut archive = zip::ZipArchive::new(Cursor::new(package))
        .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
    let mut extracted = Vec::with_capacity(APP_FILES.len());
    for entry_name in APP_FILES {
        let mut entry = archive
            .by_name(entry_name)
            .map_err(|_| UpdateError::InvalidArchive(format!("更新包缺少 {entry_name}")))?;
        let file_name = Path::new(entry_name)
            .file_name()
            .ok_or_else(|| UpdateError::InvalidArchive(format!("更新包条目无效 {entry_name}")))?;
        let target = staging_dir.join(file_name);
        let mut buffer = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buffer)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        std::fs::write(&target, &buffer)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        extracted.push(target);
    }
    Ok(extracted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const FEED_JSON: &str = r#"{"Assets":[
        {"PackageId":"Pandock","Version":"1.1.0","Type":"Full","FileName":"Pandock-1.1.0-full.nupkg","SHA1":"x","SHA256":"ABC","Size":10},
        {"PackageId":"Pandock","Version":"1.2.0","Type":"Full","FileName":"Pandock-1.2.0-full.nupkg","SHA1":"y","SHA256":"DEF","Size":20},
        {"PackageId":"Pandock","Version":"1.2.0","Type":"Delta","FileName":"Pandock-1.2.0-delta.nupkg","SHA1":"z","SHA256":"GHI","Size":5},
        {"PackageId":"Other","Version":"9.9.9","Type":"Full","FileName":"Other-9.9.9-full.nupkg","SHA1":"w","SHA256":"JKL","Size":1}
    ]}"#;

    #[test]
    fn parse_feed_reads_assets() {
        let assets = parse_feed(FEED_JSON.as_bytes()).unwrap();
        assert_eq!(assets.len(), 4);
        assert_eq!(assets[0].version, Version::new(1, 1, 0));
        assert_eq!(assets[1].sha256, "DEF");
    }

    #[test]
    fn select_update_picks_newest_full_above_current() {
        let assets = parse_feed(FEED_JSON.as_bytes()).unwrap();
        let selected = select_update(&assets, PACKAGE_ID, &Version::new(1, 1, 0)).unwrap();
        assert_eq!(selected.version, Version::new(1, 2, 0));
        assert_eq!(selected.file_name, "Pandock-1.2.0-full.nupkg");
        assert_eq!(selected.asset_type, "Full");
    }

    #[test]
    fn select_update_rejects_same_and_older() {
        let assets = parse_feed(FEED_JSON.as_bytes()).unwrap();
        assert!(select_update(&assets, PACKAGE_ID, &Version::new(1, 2, 0)).is_none());
        assert!(select_update(&assets, PACKAGE_ID, &Version::new(1, 3, 0)).is_none());
    }

    #[test]
    fn parse_feed_rejects_invalid_version() {
        let bad = r#"{"Assets":[{"PackageId":"Pandock","Version":"not-semver","Type":"Full","FileName":"x","SHA256":"A","Size":1}]}"#;
        assert!(parse_feed(bad.as_bytes()).is_err());
    }

    fn build_test_nupkg() -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for name in APP_FILES {
            writer.start_file(name, options).unwrap();
            writer
                .write_all(format!("content-of-{name}").as_bytes())
                .unwrap();
        }
        writer.start_file("lib/app/sq.version", options).unwrap();
        writer.write_all(b"plumbing").unwrap();
        // ADR-113 兼容桥：包里继续携带 WebView2Loader.dll（1.4.0 及更早客户端的
        // extract_app_files 把它当必需项）；新客户端的 APP_FILES 不含它，
        // fixture 据此同时断言"包里有"（下方回归测试）与"新客户端不提取"（本测试）。
        writer
            .start_file("lib/app/WebView2Loader.dll", options)
            .unwrap();
        writer.write_all(b"compat-bridge-dll").unwrap();
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn extract_app_files_extracts_only_app_entries() {
        let package = build_test_nupkg();
        let staging = tempfile::tempdir().unwrap();
        let extracted = extract_app_files(&package, staging.path()).unwrap();
        assert_eq!(extracted.len(), 2);
        assert!(staging.path().join("pandock.exe").exists());
        assert!(staging.path().join("updater.exe").exists());
        assert!(!staging.path().join("WebView2Loader.dll").exists());
        assert!(!staging.path().join("sq.version").exists());
        let content = std::fs::read_to_string(staging.path().join("pandock.exe")).unwrap();
        assert_eq!(content, "content-of-lib/app/pandock.exe");
    }

    /// ADR-113 兼容桥回归测试（1.4.1 线上事故）：1.4.0 及更早客户端的
    /// extract_app_files 用硬编码三项清单（含 DLL）且逐条 by_name 必需。
    /// 新版包必须继续携带 DLL 条目，否则旧客户端拒收更新包。
    /// 摘除条件：所有 1.4.0 及更早客户端退市（见 docs/release.md）。
    #[test]
    fn legacy_three_item_extraction_succeeds_on_bridge_package() {
        let package = build_test_nupkg();
        // 旧客户端（tag 1.4.0）的硬编码清单，逐条必需——刻意不引用 APP_FILES，
        // 钉死"旧清单"这个历史形态。
        let legacy_required = [
            "lib/app/pandock.exe",
            "lib/app/WebView2Loader.dll",
            "lib/app/updater.exe",
        ];
        let mut zip = zip::ZipArchive::new(Cursor::new(&package)).unwrap();
        for name in legacy_required {
            assert!(
                zip.by_name(name).is_ok(),
                "旧客户端（≤1.4.0）必需条目在新包中缺失: {name}"
            );
        }
    }

    #[test]
    fn extract_app_files_rejects_missing_entry() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("lib/app/pandock.exe", options).unwrap();
        writer.write_all(b"x").unwrap();
        let package = writer.finish().unwrap().into_inner();
        let staging = tempfile::tempdir().unwrap();
        assert!(extract_app_files(&package, staging.path()).is_err());
    }

    #[test]
    fn verify_package_checks_size_and_hash() {
        let data = b"hello velopack";
        let sha = super::super::verify::sha256_hex(data).to_uppercase();
        assert!(verify_package(data, &sha, data.len() as u64).is_ok());
        assert!(verify_package(data, &sha, (data.len() + 1) as u64).is_err());
        assert!(verify_package(data, "0000", data.len() as u64).is_err());
    }
}
