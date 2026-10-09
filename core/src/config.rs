use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default = "default_config_schema")]
    pub config_schema: u32,
    pub app_key: String,
    #[serde(default, skip_serializing)]
    pub app_secret: String,
    #[serde(default, skip_serializing)]
    pub webdav_password: String,
    pub app_name: String,
    pub listen: SocketAddr,
    pub dav_prefix: String,
    pub basic_auth: bool,
    pub username: String,
    pub cache_size_mb: u64,
    pub launch_at_login: bool,
    pub start_webdav_automatically: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_schema: default_config_schema(),
            app_key: String::new(),
            app_secret: String::new(),
            webdav_password: String::new(),
            app_name: String::new(),
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 19090),
            dav_prefix: "/dav".to_string(),
            basic_auth: false,
            username: "dav".to_string(),
            cache_size_mb: 256,
            launch_at_login: false,
            start_webdav_automatically: true,
        }
    }
}

fn default_config_schema() -> u32 {
    1
}

pub fn migrate_config_file(legacy_config: &Path, new_config: &Path) -> Result<bool, String> {
    if new_config.exists() || !legacy_config.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(legacy_config).map_err(|e| e.to_string())?;
    let mut value: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if let Some(object) = value.as_object_mut() {
        object.remove("app_secret");
        object.remove("webdav_password");
        object.insert("config_schema".into(), serde_json::json!(1));
    }
    if let Some(parent) = new_config.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let sanitized = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    std::fs::write(new_config, sanitized).map_err(|e| e.to_string())?;
    Ok(true)
}

impl Config {
    pub fn validate(&self, has_basic_password: bool) -> Result<(), String> {
        if self.app_key.trim().is_empty() || self.app_name.trim().is_empty() {
            return Err("App Key 与应用名称不能为空".into());
        }
        if self.app_name.contains('/') || self.app_name == "." || self.app_name == ".." {
            return Err("应用名称不能包含路径分隔符".into());
        }
        if !self.dav_prefix.starts_with('/') || self.dav_prefix.contains("..") {
            return Err("WebDAV 路径前缀无效".into());
        }
        if !self.listen.ip().is_loopback() && (!self.basic_auth || !has_basic_password) {
            return Err("非本机监听必须启用 WebDAV 密码".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_defaults_are_safe() {
        let cfg = Config::default();
        assert!(cfg.listen.ip().is_loopback());
        assert!(!cfg.basic_auth);
        assert_eq!(cfg.dav_prefix, "/dav");
    }

    #[test]
    fn remote_listen_requires_auth() {
        let cfg = Config {
            app_key: "k".into(),
            app_name: "a".into(),
            listen: "0.0.0.0:19090".parse().unwrap(),
            ..Default::default()
        };
        assert!(cfg.validate(false).is_err());
        let cfg = Config {
            basic_auth: true,
            ..cfg
        };
        assert!(cfg.validate(true).is_ok());
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn migrates_non_secret_config_and_removes_legacy_secrets() {
        let root = std::env::temp_dir().join(format!("pandock-migration-{}", uuid::Uuid::new_v4()));
        let legacy = root.join("legacy/config.json");
        let current = root.join("app/data/config.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, r#"{"app_key":"key","app_name":"App","app_secret":"secret","webdav_password":"pw","listen":"127.0.0.1:19090"}"#).unwrap();
        assert!(migrate_config_file(&legacy, &current).unwrap());
        let migrated = std::fs::read_to_string(&current).unwrap();
        assert!(migrated.contains("config_schema"));
        assert!(!migrated.contains("app_secret"));
        assert!(!migrated.contains("webdav_password"));
        assert!(!migrate_config_file(&legacy, &current).unwrap());
    }
}
