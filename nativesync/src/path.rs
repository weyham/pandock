use std::path::{Path, PathBuf};

#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW};

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn sanitize_component(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        let invalid = matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            || ch.is_control()
            || ch == '%';
        if invalid {
            out.push_str(&format!("%{:02X}", ch as u32));
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        out.push_str("%00");
    }
    if out.ends_with('.') {
        out.pop();
        out.push_str("%2E");
    }
    if out.ends_with(' ') {
        out.pop();
        out.push_str("%20");
    }
    let stem = out.trim_end_matches('.').to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        format!("%00-{}", out)
    } else {
        out
    }
}
#[cfg(windows)]
pub fn default_root_path() -> Result<PathBuf, String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};
    let raw = unsafe {
        SHGetKnownFolderPath(&FOLDERID_Documents, KNOWN_FOLDER_FLAG(0), HANDLE::default())
    }
    .map_err(|e| format!("无法获取文档目录: {e}"))?;
    let value = unsafe { raw.to_string().map_err(|e| e.to_string())? };
    unsafe {
        CoTaskMemFree(Some(raw.as_ptr() as _));
    }
    Ok(PathBuf::from(value).join("Pandock"))
}
#[cfg(not(windows))]
pub fn default_root_path() -> Result<PathBuf, String> {
    Err("NativeSync 仅支持 Windows Cloud Files API".into())
}

pub fn validate_root_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("同步根不能为空".into());
    }
    let normalized = path.to_string_lossy().replace('/', "\\");
    let lower = normalized.to_ascii_lowercase();
    if lower.contains("\\app\\") || lower.ends_with("\\app") {
        return Err("同步根不能位于 app/ 目录".into());
    }
    if path.exists()
        && std::fs::read_dir(path)
            .map_err(|e| e.to_string())?
            .next()
            .is_some()
    {
        return Err("同步根必须为空目录".into());
    }
    #[cfg(windows)]
    {
        validate_windows_volume(path)?;
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_volume(path: &Path) -> Result<(), String> {
    let text = path.to_string_lossy().replace('/', "\\");
    let root = if text.len() >= 3 && text.as_bytes()[1] == b':' {
        format!("{}\\", &text[..3])
    } else {
        return Err("同步根必须位于本地盘符卷".into());
    };
    let w = wide(&root);
    let kind = unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) };
    if kind == 2 || kind == 4 || kind == 1 {
        return Err("同步根不能位于可移动盘、网络盘或无效卷".into());
    }
    if kind != 3 {
        return Err("同步根必须位于固定本地卷".into());
    }
    let mut fs = [0u16; 32];
    let ok =
        unsafe { GetVolumeInformationW(PCWSTR(w.as_ptr()), None, None, None, None, Some(&mut fs)) }
            .is_ok();
    if !ok {
        return Err("无法读取同步根卷信息".into());
    }
    let name = String::from_utf16_lossy(&fs)
        .trim_end_matches('\0')
        .to_ascii_uppercase();
    if name != "NTFS" {
        return Err(format!("同步根必须位于 NTFS 卷，当前为 {name}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escapes_windows_names_reversibly_enough() {
        assert_eq!(sanitize_component("a:b"), "a%3A b".replace(" ", ""));
    }
    #[test]
    fn rejects_app_and_non_empty() {
        assert!(validate_root_path(Path::new("C:\\app\\Pandock")).is_err());
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;
    #[test]
    fn percent_and_trailing_chars_are_escaped() {
        assert_eq!(sanitize_component("a%b."), "a%25b%2E");
        assert_eq!(sanitize_component("a:b"), "a%3Ab");
    }
}
