//! 「有未同步本地改动」的内存镜像：脱水前的安全闸门。
//!
//! 为什么需要它：`NOTIFY_DEHYDRATE` 是**阻塞型**回调（发起脱水的应用正在等我们的响应），
//! 回调线程里绝不能查库/联网；但脱水前又**必须**知道该条目有没有没上传到云端的本地改动，
//! 否则一次脱水就能把用户的改动丢掉（S1 是单向同步，本地改动不会自动上传）。
//!
//! 因此：**SQLite 是权威，这个镜像是派生视图**。
//!   * 唯一更新点是 `db::IndexDb::set_pending_op`（设置与清除都覆盖）；
//!   * 进程启动 / 重连时用 [`preload`] 从库里预热一次；
//!   * 回调线程只读内存（`RwLock` 读一个 `HashSet<u64>`），不做任何 I/O。
//!
//! 失败方向刻意保守：镜像不可用（锁中毒等）时 [`is_pending`] 返回 `true` ——
//! 宁可不脱水，也不冒丢用户数据的风险。

use std::collections::HashSet;
use std::sync::{OnceLock, RwLock};

fn registry() -> &'static RwLock<HashSet<u64>> {
    static REGISTRY: OnceLock<RwLock<HashSet<u64>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashSet::new()))
}

/// 标记该 `fs_id` 有未同步的本地改动。
pub fn mark(fs_id: u64) {
    if let Ok(mut set) = registry().write() {
        set.insert(fs_id);
    }
}

/// 清除标记（本地改动已不存在：上传完成、撤销或条目已删除）。
pub fn clear(fs_id: u64) {
    if let Ok(mut set) = registry().write() {
        set.remove(&fs_id);
    }
}

/// 用库里的权威集合整体替换镜像（启动 / 重连预热）。
pub fn preload(ids: impl IntoIterator<Item = u64>) {
    if let Ok(mut set) = registry().write() {
        set.clear();
        set.extend(ids);
    }
}

/// 该 `fs_id` 是否有未同步的本地改动。镜像不可用时返回 `true`（拒绝脱水，保守）。
pub fn is_pending(fs_id: u64) -> bool {
    match registry().read() {
        Ok(set) => set.contains(&fs_id),
        Err(_) => true,
    }
}

/// 镜像当前条目数（测试与诊断用）。
#[cfg(test)]
pub fn tracked() -> usize {
    registry().read().map(|set| set.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全局镜像在测试函数之间共享，因此断言在一个测试里顺序做完，避免互相干扰。
    #[test]
    fn mirror_tracks_pending_ids_and_preload_replaces_contents() {
        preload(Vec::new());
        assert!(!is_pending(900_001));
        mark(900_001);
        assert!(is_pending(900_001));
        assert!(!is_pending(900_002), "只标记命中的那一条");
        // 重复标记不产生重复项。
        mark(900_001);
        assert_eq!(tracked(), 1);
        // 清除后立即失效（上传完成路径）。
        clear(900_001);
        assert!(!is_pending(900_001));
        // 预热用库里的权威集合**整体替换**镜像：旧值不得残留，新值必须可见。
        mark(900_003);
        preload(vec![900_004, 900_005]);
        assert!(!is_pending(900_003));
        assert!(is_pending(900_004));
        assert!(is_pending(900_005));
        assert_eq!(tracked(), 2);
        preload(Vec::new());
        assert_eq!(tracked(), 0);
    }
}
