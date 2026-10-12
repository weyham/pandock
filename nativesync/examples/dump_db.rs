//! 诊断用：把 sync.db 的 remote_entry / local_state 打印出来。
//!
//! 用法：cargo run -p pandock-nativesync --example dump_db -- <sync.db 路径>

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("缺少 db 路径")?;
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    println!("== sync_root ==");
    let mut st = conn.prepare("SELECT id,root_path,state,enabled FROM sync_root")?;
    let rows = st.query_map([], |r| {
        Ok(format!(
            "  id={} root={} state={} enabled={}",
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?
        ))
    })?;
    for r in rows {
        println!("{}", r?);
    }

    println!("== remote_entry ==");
    let mut st = conn.prepare(
        "SELECT fs_id,parent_fs_id,remote_path,is_dir,tombstone,size FROM remote_entry ORDER BY remote_path",
    )?;
    let rows = st.query_map([], |r| {
        Ok(format!(
            "  fs_id={:<22} dir={} size={:<12} path={}",
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(5)?,
            r.get::<_, String>(2)?
        ))
    })?;
    let mut count = 0;
    for r in rows {
        println!("{}", r?);
        count += 1;
    }
    println!("  共 {count} 行");

    println!("== local_state ==");
    let mut st = conn.prepare(
        "SELECT l.fs_id,l.relative_path,l.hydration_state,l.pin_state FROM local_state l ORDER BY l.relative_path",
    )?;
    let rows = st.query_map([], |r| {
        Ok(format!(
            "  fs_id={:<22} state={:<12} pin={:<9} rel={}",
            r.get::<_, i64>(0)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(1)?
        ))
    })?;
    for r in rows {
        println!("{}", r?);
    }
    Ok(())
}
