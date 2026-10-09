use crate::update::verify::{default_install_allowlist, sha256_hex};
use crate::update::UpdateError;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub fn validate_helper_hash(path: &Path, expected_sha256: &str) -> Result<(), UpdateError> {
    let bytes = fs::read(path).map_err(|error| UpdateError::Internal(error.to_string()))?;
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
        return Err(UpdateError::HashMismatch);
    }
    Ok(())
}

pub fn prepare_rollback(
    app_dir: &Path,
    rollback_dir: &Path,
    names: &HashSet<String>,
) -> Result<Vec<String>, UpdateError> {
    fs::create_dir_all(rollback_dir).map_err(|error| UpdateError::Internal(error.to_string()))?;
    let mut backed_up = Vec::new();
    for name in names {
        validate_relative_name(name)?;
        let source = app_dir.join(name);
        if !source.is_file() {
            continue;
        }
        let destination = rollback_dir.join(name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        fs::copy(&source, &destination)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        backed_up.push(name.clone());
    }
    Ok(backed_up)
}

pub fn apply_staging_files(
    staging_dir: &Path,
    app_dir: &Path,
    rollback_dir: &Path,
    names: &HashSet<String>,
) -> Result<Vec<String>, UpdateError> {
    let mut replaced = Vec::new();
    for name in names {
        validate_relative_name(name)?;
        let source = staging_dir.join(name);
        if !source.is_file() {
            continue;
        }
        let destination = app_dir.join(name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        if destination.exists() {
            let backup = rollback_dir.join(name);
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| UpdateError::Internal(error.to_string()))?;
            }
            if backup.exists() {
                fs::remove_file(&backup)
                    .map_err(|error| UpdateError::Internal(error.to_string()))?;
            }
            fs::rename(&destination, &backup)
                .map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        fs::rename(&source, &destination)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        replaced.push(name.clone());
    }
    Ok(replaced)
}

pub fn restore_rollback(
    app_dir: &Path,
    rollback_dir: &Path,
    names: &[String],
) -> Result<(), UpdateError> {
    for name in names {
        validate_relative_name(name)?;
        let source = rollback_dir.join(name);
        if !source.is_file() {
            continue;
        }
        let destination = app_dir.join(name);
        if destination.exists() {
            fs::remove_file(&destination)
                .map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        fs::rename(&source, &destination)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
    }
    Ok(())
}

pub fn cleanup_staging(staging_dir: &Path) -> Result<(), UpdateError> {
    if staging_dir.exists() {
        fs::remove_dir_all(staging_dir)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
    }
    Ok(())
}

pub fn write_readiness_after_startup(
    app_dir: &Path,
    token: &str,
    version: &str,
    initialized: bool,
    requires_webdav: bool,
    webdav_ok: bool,
) -> Result<PathBuf, UpdateError> {
    if !initialized {
        return Err(UpdateError::Internal("核心运行时尚未初始化完成".into()));
    }
    if requires_webdav && !webdav_ok {
        return Err(UpdateError::Internal("自动启动 WebDAV 尚未成功".into()));
    }
    write_readiness_marker(app_dir, token, version)
}

pub fn readiness_path(app_dir: &Path, token: &str) -> PathBuf {
    app_dir
        .join("updates")
        .join("readiness")
        .join(format!("{token}.json"))
}

pub fn write_readiness_marker(
    app_dir: &Path,
    token: &str,
    version: &str,
) -> Result<PathBuf, UpdateError> {
    let path = readiness_path(app_dir, token);
    let parent = path
        .parent()
        .ok_or_else(|| UpdateError::Internal("readiness 路径没有父目录".into()))?;
    fs::create_dir_all(parent).map_err(|error| UpdateError::Internal(error.to_string()))?;
    let value = serde_json::json!({
        "protocol": 1,
        "version": version,
        "ready": true,
    });
    fs::write(
        &path,
        serde_json::to_vec_pretty(&value)
            .map_err(|error| UpdateError::Internal(error.to_string()))?,
    )
    .map_err(|error| UpdateError::Internal(error.to_string()))?;
    Ok(path)
}

/// 更新包内的主程序文件名。1.2.0 起更新链全部为 pandock.exe
///（ADR-104 的双名过渡随开源迁移结束，见 ADR-107）。
pub const LAUNCH_EXE_NAME: &str = "pandock.exe";
pub fn install_allowlist(helper_path: Option<&str>) -> HashSet<String> {
    default_install_allowlist(helper_path)
}

fn validate_relative_name(name: &str) -> Result<(), UpdateError> {
    let path = Path::new(name);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(UpdateError::InvalidArchive(format!("路径越界: {name}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, value: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, value).unwrap();
    }

    #[test]
    fn allowlist_accepts_both_exe_names_for_transition() {
        let names = install_allowlist(None);
        assert!(names.contains("pandock.exe"));
        assert!(names.contains("pandock.exe"));
    }

    #[test]
    fn launch_exe_name_is_pandock() {
        assert_eq!(LAUNCH_EXE_NAME, "pandock.exe");
    }
    #[test]
    fn replaces_only_allowlisted_files_and_keeps_data() {
        let root = std::env::temp_dir().join(format!("pandock-install-{}", uuid::Uuid::new_v4()));
        let app = root.join("app");
        let staging = app.join("updates/staging/1.0.1");
        let rollback = app.join("updates/rollback/1.0.0");
        write(&app.join("pandock.exe"), "old");
        write(&app.join("data/config.json"), "user-data");
        write(&staging.join("pandock.exe"), "new");
        write(&staging.join("VERSION.txt"), "1.0.1");

        let names = install_allowlist(None);
        let backed_up = prepare_rollback(&app, &rollback, &names).unwrap();
        assert!(backed_up.contains(&"pandock.exe".to_string()));
        let applied = apply_staging_files(&staging, &app, &rollback, &names).unwrap();
        assert!(applied.contains(&"pandock.exe".to_string()));
        assert!(applied.contains(&"VERSION.txt".to_string()));
        assert_eq!(fs::read_to_string(app.join("pandock.exe")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(app.join("data/config.json")).unwrap(),
            "user-data"
        );

        restore_rollback(&app, &rollback, &backed_up).unwrap();
        assert_eq!(fs::read_to_string(app.join("pandock.exe")).unwrap(), "old");
    }

    #[test]
    fn rejects_path_traversal_names() {
        assert!(validate_relative_name("../pandock.exe").is_err());
        assert!(validate_relative_name("pandock.exe").is_ok());
    }
    #[test]
    fn readiness_gate_requires_init_and_optional_webdav() {
        let root = std::env::temp_dir().join(format!("pandock-readiness-{}", uuid::Uuid::new_v4()));
        assert!(write_readiness_after_startup(&root, "token", "1.0.1", true, false, false).is_ok());
        assert!(readiness_path(&root, "token").is_file());

        let root = std::env::temp_dir().join(format!("pandock-readiness-{}", uuid::Uuid::new_v4()));
        assert!(
            write_readiness_after_startup(&root, "token", "1.0.1", false, false, false).is_err()
        );
        assert!(!readiness_path(&root, "token").exists());

        let root = std::env::temp_dir().join(format!("pandock-readiness-{}", uuid::Uuid::new_v4()));
        assert!(write_readiness_after_startup(&root, "token", "1.0.1", true, true, false).is_err());
        assert!(!readiness_path(&root, "token").exists());
    }
}
