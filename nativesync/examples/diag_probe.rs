//! 只读诊断探针：把 Cloud Files 的**真值**打出来（不需要在提供程序进程内）。
//!
//! 用途：任何"状态看起来不对"的时候，用一条命令替代"从图标反推"。
//! 实测结论（2026-10-12）：**读**探针（`CfGetPlaceholderInfo`）在外部进程也能成功，
//! 对**目录**同样成功；"必须在提供程序进程内"只适用于 `CfSetInSyncState` /
//! `CfConvertToPlaceholder` 这类**写**操作。
//!
//! 用法：
//! ```text
//! cargo run -p pandock-nativesync --example diag_probe -- <路径> [更多路径...]
//! ```
//! 输出（每行一个条目）：`pin / inSync / hr / FileSize / OnDiskDataSize / attrs / reparse tag`。
#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFileAttributesW, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

/// `FSCTL_GET_REPARSE_POINT`（权威的重解析标签来源；比 `GetFileAttributes*` 可靠）。
const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
/// 只读打开（属性访问即可；目录必须带 `FILE_FLAG_BACKUP_SEMANTICS`）。
const FILE_READ_ATTRIBUTES: u32 = 0x0080;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_read_attrs(path: &str) -> Option<HANDLE> {
    let w = wide(path);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_BACKUP_SEMANTICS.0),
            HANDLE::default(),
        )
    };
    handle.ok().filter(|h| !h.is_invalid())
}

fn reparse_tag(handle: HANDLE) -> Option<u32> {
    let mut buf = [0u8; 1024];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_GET_REPARSE_POINT,
            None,
            0,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut returned),
            None,
        )
    };
    if ok.is_err() || returned < 4 {
        return None;
    }
    Some(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

/// `(hr, pin, in_sync, file_size, on_disk)`；读不到时后四项为 `None`。
fn cf_probe(
    handle: HANDLE,
    raw: &str,
) -> (u32, Option<i32>, Option<i32>, Option<i64>, Option<i64>) {
    let mut basic = vec![0u8; std::mem::size_of::<CF_PLACEHOLDER_BASIC_INFO>() + 64];
    let mut returned = 0u32;
    let r = unsafe {
        CfGetPlaceholderInfo(
            handle,
            CF_PLACEHOLDER_INFO_BASIC,
            basic.as_mut_ptr() as *mut _,
            basic.len() as u32,
            Some(&mut returned),
        )
    };
    let hr = match &r {
        Ok(()) => 0u32,
        Err(e) => e.code().0 as u32,
    };
    let (pin, in_sync) = if r.is_ok() {
        let info = unsafe { &*(basic.as_ptr() as *const CF_PLACEHOLDER_BASIC_INFO) };
        (Some(info.PinState.0), Some(info.InSyncState.0))
    } else {
        (None, None)
    };
    let mut std_buf = vec![0u8; std::mem::size_of::<CF_PLACEHOLDER_STANDARD_INFO>() + 64];
    let mut std_ret = 0u32;
    let std_ok = unsafe {
        CfGetPlaceholderInfo(
            handle,
            CF_PLACEHOLDER_INFO_STANDARD,
            std_buf.as_mut_ptr() as *mut _,
            std_buf.len() as u32,
            Some(&mut std_ret),
        )
    }
    .is_ok();
    // 注意：CF_PLACEHOLDER_STANDARD_INFO 里**没有** FileSize（文件大小来自文件系统元数据），
    // 所以这里用本地元数据长度，与 OnDiskDataSize 对照即可判断水合程度。
    let file_size = std::fs::metadata(&raw).ok().map(|m| m.len() as i64);
    let on_disk = if std_ok {
        let info = unsafe { &*(std_buf.as_ptr() as *const CF_PLACEHOLDER_STANDARD_INFO) };
        Some(info.OnDiskDataSize)
    } else {
        None
    };
    (hr, pin, in_sync, file_size, on_disk)
}

fn pin_name(pin: i32) -> &'static str {
    match pin {
        0 => "UNSPECIFIED",
        1 => "PINNED",
        2 => "UNPINNED",
        3 => "EXCLUDED",
        _ => "?",
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "用法: cargo run -p pandock-nativesync --example diag_probe -- <路径> [更多路径...]"
        );
        std::process::exit(2);
    }
    println!(
        "{:<44} {:>12} {:>9} {:>10} {:>12} {:>12} {:>10} {:>8}",
        "path", "pin", "inSync", "hr", "FileSize", "OnDisk", "attrs", "tag"
    );
    for raw in args {
        let w = wide(&raw);
        let attrs = unsafe { GetFileAttributesW(PCWSTR(w.as_ptr())) };
        let Some(handle) = open_read_attrs(&raw) else {
            println!("{raw:<44} (打开失败：CreateFile READ_ATTRIBUTES|BACKUP_SEMANTICS 被拒)");
            continue;
        };
        let (hr, pin, in_sync, file_size, on_disk) = cf_probe(handle, &raw);
        let tag = reparse_tag(handle);
        unsafe {
            let _ = CloseHandle(handle);
        }
        println!(
            "{:<44} {:>12} {:>9} {:>10} {:>12} {:>12} {:#10X} {:>8}",
            raw,
            pin.map(pin_name).unwrap_or("-"),
            in_sync.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            format!("{hr:#010X}"),
            file_size
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            on_disk.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            attrs,
            tag.map(|t| format!("{t:#010X}"))
                .unwrap_or_else(|| "-".into()),
        );
    }
}
