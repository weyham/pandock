//! S2 上传方向（ADR-112）：参数与纯函数。
//!
//! 执行入口在 `manager.rs::drain_upload_queue`（db/platform/backend 都在 manager 手里）；
//! 本模块只放**不碰状态的纯逻辑**（可单测）。定稿参数来自 ADR-112 §3.6
//! （2026-10-13 开发总监终审）：防抖 3s、失败 ≥5 次进「需处理」、
//! 冲突副本模板 `<原名>-<设备名>`、副本再撞名加序号且**不递归**冲突流程。

use pandock_core::cloud::{CloudError, CloudErrorKind};
use std::time::Duration;

/// 上传 worker 定稿参数（ADR-112 §3.6）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadParams {
    /// close-completion 后的防抖窗口（编辑器连续保存合并为一次上传）。
    pub debounce: Duration,
    /// 单文件失败计次达到该值 ⇒ 条目置 error 进「需处理」，不再自动重试。
    pub error_threshold: u32,
}

impl Default for UploadParams {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(3),
            error_threshold: 5,
        }
    }
}

/// 一次 drain 的报告：日志汇总与调度层（退避/唤醒）决策依据。
#[derive(Clone, Debug, Default)]
pub struct UploadDrainReport {
    pub uploaded: u32,
    pub dirs_created: u32,
    pub conflicts: u32,
    pub deleted: u32,
    pub renamed: u32,
    /// 无基线删除护栏 / 本地已消失等安全跳过（不计失败）。
    pub skipped: u32,
    /// 防抖窗口内的条目（留待下一次 drain）。
    pub deferred: u32,
    pub failed: u32,
    /// 被手动对账抢占（文件间让位），不算失败。
    pub interrupted: bool,
    pub first_error: Option<String>,
}

/// Windows 非法文件名字符（`<>:"/\|?*` 与控制字符）替换为 '-'；空结果回退 "device"。
pub fn sanitize_device(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches([' ', '.']);
    if cleaned.is_empty() {
        "device".into()
    } else {
        cleaned.chars().take(64).collect()
    }
}

/// 设备名（Windows 计算机名；取不到回退 "device"）。冲突副本模板的后缀（ADR-112 §3.3）。
///
/// 取值用 Win32 `GetComputerNameW`——`COMPUTERNAME` 环境变量在某些进程上下文
/// （服务/子进程/精简环境块）会缺失，实测生产上回退成了 "device"（2026-10-13
/// 冲突副本被命名为 `…-device.txt` 而非 `…-<计算机名>.txt`）。
pub fn device_name() -> String {
    #[cfg(windows)]
    if let Some(name) = windows_computer_name() {
        return sanitize_device(&name);
    }
    sanitize_device(&std::env::var("COMPUTERNAME").unwrap_or_default())
}

#[cfg(windows)]
fn windows_computer_name() -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::System::SystemInformation::{
        GetComputerNameExW, COMPUTER_NAME_FORMAT,
    };
    let mut buffer = [0u16; 256];
    let mut length = buffer.len() as u32;
    unsafe {
        GetComputerNameExW(
            COMPUTER_NAME_FORMAT(0), // ComputerNameNetBIOS
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()?;
    Some(String::from_utf16_lossy(&buffer[..length as usize]))
}

/// 冲突副本文件名：attempt=1 → `<stem>-<device>[.<ext>]`；
/// attempt≥2 → `<stem>-<device>-<attempt>[.<ext>]`（改名循环终止规则，**不递归**）。
pub fn conflict_copy_name(name: &str, device: &str, attempt: u32) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    if attempt <= 1 {
        format!("{stem}-{device}{ext}")
    } else {
        format!("{stem}-{device}-{attempt}{ext}")
    }
}

/// ADR-112 §3.3 冲突入口：AlreadyExists + 原始 errno -8。
pub fn is_conflict_error(error: &CloudError) -> bool {
    error.kind == CloudErrorKind::AlreadyExists && error.raw_errno == Some(-8)
}

/// 硬错误（不盲重，直接进「需处理」）：配额满/无权限/参数非法/不支持。
/// 其余（Transient/RateLimited/Unknown/NotFound）由调用方计次退避。
pub fn is_hard_error(error: &CloudError) -> bool {
    matches!(
        error.kind,
        CloudErrorKind::QuotaExceeded
            | CloudErrorKind::PermissionDenied
            | CloudErrorKind::InvalidRequest
            | CloudErrorKind::Unsupported
    )
}

/// 计算本地文件 MD5（32 位小写，与百度 block_list 同格式）。流式读，不按整文件载入。
pub fn local_md5(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut context = md5::Context::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        context.consume(&buffer[..read]);
    }
    Ok(format!("{:x}", context.compute()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_params_match_adr112_ratified_values() {
        let p = UploadParams::default();
        assert_eq!(p.debounce, Duration::from_secs(3));
        assert_eq!(p.error_threshold, 5);
    }

    #[test]
    fn sanitize_device_replaces_illegal_chars_and_falls_back() {
        assert_eq!(sanitize_device("DESKTOP-AB12"), "DESKTOP-AB12");
        assert_eq!(sanitize_device("my pc/v2:lab"), "my pc-v2-lab");
        assert_eq!(sanitize_device("   "), "device");
        assert_eq!(sanitize_device(""), "device");
        assert_eq!(sanitize_device("名字-01"), "名字-01", "中文设备名保留");
    }

    #[cfg(windows)]
    #[test]
    fn device_name_resolves_real_computer_name_not_fallback() {
        let name = device_name();
        assert!(!name.is_empty());
        assert_ne!(
            name, "device",
            "GetComputerNameW 必须取到真实计算机名（生产上曾回退成 device）"
        );
        // 注意：本机部分进程上下文 COMPUTERNAME 环境变量本来就缺失（这正是
        // 生产上回退成 "device" 的根源）——所以这里只能断言「取到了合法的真实名字」，
        // 不能拿环境变量做期望值。
        assert!(
            name.chars().all(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')),
            "计算机名不得含 Windows 非法字符: {name}"
        );
    }

    #[test]
    fn conflict_copy_name_appends_device_and_counter() {
        assert_eq!(conflict_copy_name("a.txt", "PC", 1), "a-PC.txt");
        assert_eq!(conflict_copy_name("a.txt", "PC", 2), "a-PC-2.txt");
        assert_eq!(conflict_copy_name("noext", "PC", 1), "noext-PC");
        assert_eq!(conflict_copy_name("noext", "PC", 3), "noext-PC-3");
        assert_eq!(
            conflict_copy_name(".hidden", "PC", 1),
            ".hidden-PC",
            "点前导不算扩展名"
        );
        assert_eq!(conflict_copy_name("a.b.c", "PC", 2), "a.b-PC-2.c");
    }

    #[test]
    fn conflict_error_requires_already_exists_and_errno_minus8() {
        let mut e = CloudError::already_exists("x");
        assert!(!is_conflict_error(&e), "缺 raw_errno 不算");
        e.raw_errno = Some(-8);
        assert!(is_conflict_error(&e));
        e.raw_errno = Some(-9);
        assert!(!is_conflict_error(&e));
        assert!(!is_conflict_error(&CloudError::not_found("x")));
    }

    #[test]
    fn hard_errors_are_quota_permission_invalid_unsupported() {
        let mut quota = CloudError::new(CloudErrorKind::QuotaExceeded, "full");
        assert!(is_hard_error(&quota));
        quota.kind = CloudErrorKind::Transient;
        assert!(!is_hard_error(&quota));
        quota.kind = CloudErrorKind::RateLimited;
        assert!(!is_hard_error(&quota));
        quota.kind = CloudErrorKind::PermissionDenied;
        assert!(is_hard_error(&quota));
    }

    #[test]
    fn local_md5_matches_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.bin");
        std::fs::write(&file, b"hello").unwrap();
        assert_eq!(
            local_md5(&file).unwrap(),
            "5d41402abc4b2a76b9719d911017c592"
        );
        std::fs::write(&file, b"").unwrap();
        assert_eq!(
            local_md5(&file).unwrap(),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
    }
}
