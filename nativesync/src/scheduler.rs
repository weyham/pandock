//! 自动对账调度（ADR-111 无感同步 S1.5）。
//!
//! 本模块只放**纯参数与纯函数**（可单测、不碰 tokio/网络）；运行循环在
//! `src-tauri/src/native_sync.rs`（需要 AppHandle/client/emit）。定稿参数来自
//! ADR-111 §6（2026-10-13 开发总监终审）：每层调用上限 25、间隔 10 分钟、
//! 退避 2m→4m→8m 封顶 60m、连续失败 ≥5 次暂停、交错深扫 K=20、定向下钻 ≤4。
//! **改这些值 = 改设计，必须先回报开发总监。**

use std::time::Duration;

/// 自动对账定稿参数（ADR-111 §6）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoSyncParams {
    /// 启动后首层延迟（让授权/连接/注册先稳定）。
    pub first_delay: Duration,
    /// 成功后的层间隔。
    pub interval: Duration,
    /// 失败退避基数（第 1 次失败后的等待）。
    pub backoff_base: Duration,
    /// 退避封顶。
    pub backoff_cap: Duration,
    /// 连续失败达到该次数即暂停自动对账（`auto_sync_state=paused`）。
    pub pause_threshold: u32,
    /// 每层 API 调用预算（根快查翻页计入，折算 = count / LIST_PAGE_SIZE + 1）。
    pub layer_budget: u32,
    /// 根快查发现可疑顶层目录后的定向下钻上限。
    pub drilldown_max: u32,
    /// 交错深扫每层列目录数（K）。
    pub deep_scan_batch: u32,
}

impl Default for AutoSyncParams {
    fn default() -> Self {
        Self {
            first_delay: Duration::from_secs(60),
            interval: Duration::from_secs(10 * 60),
            backoff_base: Duration::from_secs(2 * 60),
            backoff_cap: Duration::from_secs(60 * 60),
            pause_threshold: 5,
            layer_budget: 25,
            drilldown_max: 4,
            deep_scan_batch: 20,
        }
    }
}

/// 一层结束后的等待时长：成功 → 固定间隔；失败 → 2m→4m→8m…指数退避，封顶 60m。
/// `consecutive_failures` 是**本次失败后**的计数（1 起算）。
pub fn next_delay(success: bool, consecutive_failures: u32, params: &AutoSyncParams) -> Duration {
    if success {
        return params.interval;
    }
    let shift = consecutive_failures.saturating_sub(1).min(20);
    let multiplied = params
        .backoff_base
        .checked_mul(1u32 << shift)
        .unwrap_or(params.backoff_cap);
    multiplied.min(params.backoff_cap)
}

/// 连续失败是否已达暂停阈值（ADR-111：≥5 次 → `auto_sync_state=paused`，
/// 等手动「立即对账」成功或授权恢复后再自动恢复）。
pub fn should_pause(consecutive_failures: u32, params: &AutoSyncParams) -> bool {
    consecutive_failures >= params.pause_threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_params_match_adr111_ratified_values() {
        let p = AutoSyncParams::default();
        assert_eq!(p.first_delay, Duration::from_secs(60));
        assert_eq!(p.interval, Duration::from_secs(600));
        assert_eq!(p.backoff_base, Duration::from_secs(120));
        assert_eq!(p.backoff_cap, Duration::from_secs(3600));
        assert_eq!(p.pause_threshold, 5);
        assert_eq!(p.layer_budget, 25);
        assert_eq!(p.drilldown_max, 4);
        assert_eq!(p.deep_scan_batch, 20);
    }

    #[test]
    fn next_delay_success_is_interval() {
        let p = AutoSyncParams::default();
        assert_eq!(next_delay(true, 0, &p), Duration::from_secs(600));
        // 成功即复位：即便之前连续失败，成功后也是固定间隔。
        assert_eq!(next_delay(true, 7, &p), Duration::from_secs(600));
    }

    #[test]
    fn backoff_doubles_2_4_8_and_caps_at_60m() {
        let p = AutoSyncParams::default();
        assert_eq!(next_delay(false, 1, &p), Duration::from_secs(120));
        assert_eq!(next_delay(false, 2, &p), Duration::from_secs(240));
        assert_eq!(next_delay(false, 3, &p), Duration::from_secs(480));
        assert_eq!(next_delay(false, 4, &p), Duration::from_secs(960));
        assert_eq!(next_delay(false, 5, &p), Duration::from_secs(1920));
        assert_eq!(next_delay(false, 6, &p), Duration::from_secs(3600));
        assert_eq!(next_delay(false, 30, &p), Duration::from_secs(3600));
    }

    #[test]
    fn pause_at_threshold() {
        let p = AutoSyncParams::default();
        assert!(!should_pause(4, &p));
        assert!(should_pause(5, &p));
        assert!(should_pause(9, &p));
    }
}
