//! 实验：对占位符调用 CfSetInSyncState 后，观察 (a) 是否仍是占位符 (b) 状态列是否改善。
//!
//! 背景：代码里有一条实测结论说「完整 hydration 后设置 in-sync 会把占位符提升为普通文件」。
//! 本探针用于在真机上验证该结论，以决定是否可以在创建/水合后标记已同步。
//!
//! 用法：cargo run -p pandock-nativesync --example mark_in_sync -- <文件路径> [in_sync|not_in_sync|query]

#![cfg(windows)]

use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use windows::Win32::Storage::CloudFilters::{
    CfSetInSyncState, CF_IN_SYNC_STATE_IN_SYNC, CF_IN_SYNC_STATE_NOT_IN_SYNC,
    CF_SET_IN_SYNC_FLAG_NONE,
};

const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("缺少文件路径")?;
    let action = args.next().unwrap_or_else(|| "in_sync".to_string());

    // 先看调用前状态
    report(&path, "调用前")?;

    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&path)?;
    let handle = windows::Win32::Foundation::HANDLE(f.as_raw_handle() as *mut core::ffi::c_void);

    match action.as_str() {
        "query" => {}
        "not_in_sync" => unsafe {
            CfSetInSyncState(
                handle,
                CF_IN_SYNC_STATE_NOT_IN_SYNC,
                CF_SET_IN_SYNC_FLAG_NONE,
                None,
            )?;
        },
        _ => {
            let mut usn = 0i64;
            unsafe {
                CfSetInSyncState(
                    handle,
                    CF_IN_SYNC_STATE_IN_SYNC,
                    CF_SET_IN_SYNC_FLAG_NONE,
                    Some(&mut usn),
                )?;
            }
            println!("CfSetInSyncState(IN_SYNC) 成功，usn={usn}");
        }
    }
    drop(f);

    report(&path, "调用后")?;
    Ok(())
}

fn report(path: &str, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let md = std::fs::metadata(path)?;
    let rp = md.file_attributes() & 0x400 != 0; // FILE_ATTRIBUTE_REPARSE_POINT
    println!("[{label}] {path}");
    println!("    ReparsePoint = {rp}   属性 = 0x{:X}", md.file_attributes());
    println!("    大小         = {}", md.len());

    Ok(())
}
