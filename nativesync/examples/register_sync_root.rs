//! S0 spike：把同步根注册给 Windows 外壳，以获得导航栏节点与同步状态角标。
//!
//! 与 `CfRegisterSyncRoot`（文件系统层）是**两件不同的事**：
//!   - CfRegisterSyncRoot：占位符、按需下载、水合回调（已实现）
//!   - StorageProviderSyncRootManager.Register：外壳集成——导航栏条目、
//!     自动水合状态角标、状态列、原生「始终保留/释放空间」（本 spike 验证）
//!
//! 用法：
//!   cargo run -p pandock-nativesync --example register_sync_root -- <同步根路径> [register|unregister|list]

#![cfg(windows)]

use windows::core::{Interface, Result, HSTRING};
use windows::Storage::Provider::{
    StorageProviderHydrationPolicy, StorageProviderHydrationPolicyModifier, StorageProviderInSyncPolicy,
    StorageProviderPopulationPolicy, StorageProviderSyncRootInfo, StorageProviderSyncRootManager,
};
use windows::Storage::StorageFolder;

/// 我们的 provider id；与注册 Id 的第一段一致。
const PROVIDER_ID: &str = "Pandock";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .unwrap_or_else(|| r"D:\Documents\CloudDock".to_string());
    let action = args.next().unwrap_or_else(|| "register".to_string());

    let sid = current_user_sid()?;
    let id = format!("{PROVIDER_ID}!{sid}!Personal");

    match action.as_str() {
        "register" => register(&root, &id),
        "unregister" => {
            StorageProviderSyncRootManager::Unregister(&HSTRING::from(id.as_str()))?;
            println!("已注销: {id}");
            Ok(())
        }
        "list" => list(),
        other => {
            eprintln!("未知动作: {other}（可用 register / unregister / list）");
            Ok(())
        }
    }
}

fn register(root: &str, id: &str) -> Result<()> {
    println!("注册同步根");
    println!("  路径 : {root}");
    println!("  标识 : {id}");

    let folder = StorageFolder::GetFolderFromPathAsync(&HSTRING::from(root))?.get()?;
    let folder: windows::Storage::IStorageFolder = folder.cast()?;

    let info = StorageProviderSyncRootInfo::new()?;
    info.SetId(&HSTRING::from(id))?;
    info.SetPath(&folder)?;
    info.SetDisplayNameResource(&HSTRING::from("Pandock"))?;
    // 先用系统自带图标占位；正式实现应指向 pandock.exe 内的图标组。
    info.SetIconResource(&HSTRING::from(r"%SystemRoot%\system32\imageres.dll,-1096"))?;
    info.SetVersion(&HSTRING::from("1.0.0"))?;

    // 占位符按需水合；用户可在资源管理器里固定到本地
    info.SetHydrationPolicy(StorageProviderHydrationPolicy::Full)?;
    info.SetHydrationPolicyModifier(StorageProviderHydrationPolicyModifier::None)?;
    info.SetPopulationPolicy(StorageProviderPopulationPolicy::Full)?;
    info.SetInSyncPolicy(StorageProviderInSyncPolicy::Default)?;
    info.SetHardlinkPolicy(windows::Storage::Provider::StorageProviderHardlinkPolicy::None)?;
    info.SetAllowPinning(true)?;
    info.SetShowSiblingsAsGroup(false)?;

    StorageProviderSyncRootManager::Register(&info)?;
    println!("✅ Register 调用成功");
    Ok(())
}

fn list() -> Result<()> {
    let roots = StorageProviderSyncRootManager::GetCurrentSyncRoots()?;
    let count = roots.Size()?;
    println!("已注册的同步根（{count} 个）:");
    for i in 0..count {
        let item = roots.GetAt(i)?;
        let id = item.Id()?;
        let path = item.Path()?.Path()?;
        println!("  {}  ->  {}", id, path);
    }
    Ok(())
}

/// 当前登录用户的 SID（注册 Id 的第二段；确保每用户一份注册）。
fn current_user_sid() -> Result<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        let mut buf = vec![0u8; needed as usize];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            needed,
            &mut needed,
        )?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut sid_str = windows::core::PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid_str)?;
        let result = sid_str.to_string()?;
        let _ = CloseHandle(token);
        Ok(result)
    }
}
