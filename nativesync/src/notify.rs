//! Cloud Files 回调参数 → 本地变更事件的纯函数层。
//!
//! 这一层刻意**不依赖 `windows` crate**：平台侧把回调参数解码成 [`NotifyInput`]，
//! 再由这里的纯函数做范围判定、路径归一化、来源判定与事件构造。因此回调语义
//! （撤消删除、跨界改名、保留名净化、去重来源）可以在任意平台单测，不需要真实
//! 的 Explorer 或云文件驱动。

use crate::model::NativeSyncChangeEvent;
use crate::path::sanitize_component;
use std::collections::HashMap;

/// 本地变更通知捕获的紧急开关（环境变量）。
///
/// 优先级高于配置文件：用于 CF 回调卡死时免编辑恢复（例如删掉配置里的字段
/// 也没法启动 TUI 的场景）。
pub const NOTIFICATIONS_ENV: &str = "PANDOCK_NATIVESYNC_NOTIFICATIONS";

/// 解析「通知捕获是否启用」的有效值：环境变量优先，其次配置文件字段。
pub fn resolve_notifications_enabled(config_value: bool) -> bool {
    match std::env::var(NOTIFICATIONS_ENV) {
        Ok(raw) => !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => config_value,
    }
}

/// 「打开者身份」日志的限频窗口：同一 `(fs_id, image)` 60 秒最多记一条。
pub const IDENTITY_LOG_WINDOW_MS: i64 = 60_000;

/// 回调发起者身份（`CF_CALLBACK_INFO.ProcessInfo` 的解码结果）。
///
/// **隐私口径**：`image` 只保留可执行文件基名（小写）；完整映像路径可能含用户名，
/// 绝不记录。被打开文件的文件名同样绝不记录。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: Option<u32>,
    pub image: Option<String>,
}

/// 从映像路径取出可安全记录的标签：最后一个路径分量、小写、去空白。
pub fn image_label(image: &str) -> Option<String> {
    let trimmed = image.trim().trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        return None;
    }
    let base = trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed).trim();
    if base.is_empty() {
        None
    } else {
        Some(base.to_ascii_lowercase())
    }
}

/// `(fs_id, image)` → 上次记录时刻 的限频器。时钟由调用方注入，便于单测。
#[derive(Debug)]
pub struct IdentityLogLimiter {
    window_ms: i64,
    last: HashMap<(u64, String), i64>,
}

impl IdentityLogLimiter {
    pub fn new(window_ms: i64) -> Self {
        Self {
            window_ms: window_ms.max(0),
            last: HashMap::new(),
        }
    }

    /// 这一次打开者身份该不该写日志；返回 `true` 时顺带记账。
    pub fn should_log(&mut self, fs_id: u64, image: &str, now_ms: i64) -> bool {
        let key = (fs_id, image.to_string());
        let allow = match self.last.get(&key) {
            Some(previous) => now_ms.saturating_sub(*previous) >= self.window_ms,
            None => true,
        };
        if allow {
            // 顺手清掉过期项：键集合最坏是「fs_id × 见过的映像数」，不会无界增长。
            self.last
                .retain(|_, at| now_ms.saturating_sub(*at) < self.window_ms);
            self.last.insert(key, now_ms);
        }
        allow
    }

    /// 当前记账的键数量（测试用；发布构建不保留）。
    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.last.len()
    }
}

/// 回调种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyKind {
    Dehydrate,
    DehydrateCompleted,
    Delete,
    DeleteCompleted,
    Rename,
    RenameCompleted,
    OpenCompleted,
    CloseCompleted,
    ValidateData,
    PlaceholdersCancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyPhase {
    Intent,
    Completion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifySource {
    App,
    Explorer,
    System,
    Unknown,
}

impl NotifySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Explorer => "explorer",
            Self::System => "system",
            Self::Unknown => "unknown",
        }
    }
}

impl NotifyKind {
    /// 事件里的稳定 kind 字串（UI 契约）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dehydrate | Self::DehydrateCompleted => "dehydrate",
            Self::Delete | Self::DeleteCompleted => "delete",
            Self::Rename | Self::RenameCompleted => "rename",
            Self::OpenCompleted => "open",
            Self::CloseCompleted => "close",
            Self::ValidateData => "validate",
            Self::PlaceholdersCancelled => "placeholders_cancelled",
        }
    }

    pub fn phase(self) -> NotifyPhase {
        match self {
            Self::DehydrateCompleted
            | Self::DeleteCompleted
            | Self::RenameCompleted
            | Self::CloseCompleted => NotifyPhase::Completion,
            _ => NotifyPhase::Intent,
        }
    }

    /// 阻塞型回调：平台必须**同步内联** ACK，否则用户操作会挂起。
    pub fn is_blocking(self) -> bool {
        matches!(self, Self::Dehydrate | Self::Delete | Self::Rename)
    }
}

/// `CF_CALLBACK_PARAMETERS_FLAGS_*` 语义位（值与 CFAPI 定义一致）。
pub mod flags {
    pub const IS_DIRECTORY: u32 = 0x0000_0001;
    pub const DELETE_IS_UNDELETE: u32 = 0x0000_0002;
    pub const RENAME_SOURCE_IN_SCOPE: u32 = 0x0000_0002;
    pub const RENAME_TARGET_IN_SCOPE: u32 = 0x0000_0004;
    pub const DEHYDRATE_BACKGROUND: u32 = 0x0000_0001;
    pub const OPEN_PLACEHOLDER_UNKNOWN: u32 = 0x0000_0001;
    pub const OPEN_PLACEHOLDER_UNSUPPORTED: u32 = 0x0000_0002;
}

/// 平台侧解码后的回调参数。
#[derive(Clone, Debug, Default)]
pub struct NotifyInput {
    pub kind: Option<NotifyKind>,
    pub flags: u32,
    /// RENAME 的新路径（`params.TargetPath`）。只有它给出改名后的名字。
    pub target_path: Option<String>,
    /// RENAME_COMPLETION 的原路径（`params.SourcePath`）。
    pub source_path: Option<String>,
    /// `CF_CALLBACK_INFO.NormalizedPath`。
    pub normalized_path: Option<String>,
    /// DEHYDRATE 的 `Reason`。
    pub reason: Option<u32>,
}

/// 判定结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotifyDecision {
    /// 产生一条本地变更事件。
    Event(Box<NativeSyncChangeEvent>),
    /// 有意忽略（不产生事件）。
    Ignore(&'static str),
    /// 不在同步根范围内。
    OutOfScope,
}

/// 平台侧在提交任务时给出的上下文（越界判定所需的全部信息）。
pub struct NotifyScope<'a> {
    pub root: &'a str,
    pub fs_id: Option<u64>,
    pub source: NotifySource,
    pub detected_at: &'a str,
}

/// 去掉 `\\?\` 前缀、盘符与前导分隔符，统一成 `/` 分隔的「无盘符尾部」。
///
/// CFAPI 的 `NormalizedPath` 不含盘符，而本地同步根一定是全路径；
/// 两边都必须先落到这个规范形态才能比较。
pub fn path_tail(path: &str) -> String {
    let p = path
        .trim_start_matches("//?/")
        .trim_start_matches(['/', '\\']);
    let p = if p.len() > 2 && p.as_bytes()[1] == b':' {
        &p[2..]
    } else {
        p
    };
    p.trim_start_matches(['/', '\\'])
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_string()
}

/// 归一化路径 → 同步根内相对路径（大小写不敏感，保留原始大小写）。
///
/// 容忍 `/`、`\`、盘符与 `\\?\` 前缀差异，以及根路径本身带不带盘符。
pub fn relative_from_normalized(normalized: &str, root: &str) -> Option<String> {
    let root_tail = path_tail(root).to_ascii_lowercase();
    if root_tail.is_empty() {
        return None;
    }
    let tail = path_tail(normalized);
    if tail.len() <= root_tail.len() {
        return None;
    }
    let head = tail.get(..root_tail.len())?;
    if head.to_ascii_lowercase() != root_tail {
        return None;
    }
    let rest = tail.get(root_tail.len()..)?.trim_start_matches('/');
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

/// 相对路径 → 逐段可逆净化的相对路径（`%XX`，见 `path::sanitize_component`）。
pub fn sanitize_relative(relative: &str) -> Option<String> {
    let parts: Vec<String> = relative
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .map(sanitize_component)
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

pub fn dehydrate_reason(code: u32) -> &'static str {
    match code {
        1 => "user_manual",
        2 => "low_disk",
        3 => "inactivity",
        4 => "os_upgrade",
        _ => "unknown",
    }
}

/// 把一次回调解码结果判定成「事件 / 有意忽略 / 越界」。
pub fn classify(input: &NotifyInput, scope: &NotifyScope<'_>) -> NotifyDecision {
    let Some(kind) = input.kind else {
        return NotifyDecision::Ignore("unknown callback kind");
    };
    // 撤消删除（UNDELETE）不得当成删除：用户把刚删掉的东西放回来了。
    if matches!(kind, NotifyKind::Delete) && input.flags & flags::DELETE_IS_UNDELETE != 0 {
        return NotifyDecision::Ignore("undelete");
    }
    if matches!(kind, NotifyKind::PlaceholdersCancelled) {
        // S1 不做按需填充（FETCH_PLACEHOLDERS 恒空），这里只观察。
        return NotifyDecision::Ignore("placeholders cancelled");
    }

    let is_dir = input.flags & flags::IS_DIRECTORY != 0;
    let from_relative = input
        .normalized_path
        .as_deref()
        .and_then(|p| relative_from_normalized(p, scope.root))
        .and_then(|rel| sanitize_relative(&rel));

    // 改名的原路径来自回调的 NormalizedPath；新路径只由 TargetPath 给出。
    // 同步根内的相对化：只有落在同步根内、且净化后仍是合法相对路径的才算数。
    // 移出同步根的目标返回 None，调用方据此知道「该条目已离开同步根」。
    let inside_root = |p: &str| -> Option<String> {
        relative_from_normalized(p, scope.root).and_then(|rel| sanitize_relative(&rel))
    };
    let rename_target = match kind {
        NotifyKind::Rename => input.target_path.as_deref().and_then(inside_root),
        NotifyKind::RenameCompleted => input
            .source_path
            .as_deref()
            .and_then(|p| relative_from_normalized(p, scope.root))
            .and_then(|rel| sanitize_relative(&rel)),
        _ => None,
    };
    // TargetPath 存在但不在根内 => 文件被移出同步根（S1 只记账，不移动文件）。
    let target_left_root = matches!(kind, NotifyKind::Rename)
        && input.target_path.is_some()
        && rename_target.is_none();

    let source_in_scope =
        matches!(kind, NotifyKind::Rename) && input.flags & flags::RENAME_SOURCE_IN_SCOPE != 0;
    let target_in_scope =
        matches!(kind, NotifyKind::Rename) && input.flags & flags::RENAME_TARGET_IN_SCOPE != 0;

    // 范围判定：改名的两侧任一侧在范围内都算相关（移入/移出同步根都要记账）。
    let in_scope = match kind {
        NotifyKind::Rename => source_in_scope || target_in_scope || from_relative.is_some(),
        _ => from_relative.is_some(),
    };
    if !in_scope {
        return NotifyDecision::OutOfScope;
    }

    let (from_relative, to_relative) = match kind {
        NotifyKind::Rename | NotifyKind::RenameCompleted => {
            let (from, to) = if matches!(kind, NotifyKind::RenameCompleted) {
                (rename_target, from_relative)
            } else {
                (from_relative, rename_target)
            };
            if from.is_none() && to.is_none() {
                return NotifyDecision::OutOfScope;
            }
            (from, to)
        }
        NotifyKind::Delete => {
            if from_relative.is_none() {
                return NotifyDecision::OutOfScope;
            }
            (from_relative, None)
        }
        _ => (from_relative, None),
    };

    let reason = match kind {
        NotifyKind::Dehydrate | NotifyKind::DehydrateCompleted => input
            .reason
            .map(dehydrate_reason)
            .map(str::to_string)
            .or_else(|| {
                if input.flags & flags::DEHYDRATE_BACKGROUND != 0 {
                    Some("background".to_string())
                } else {
                    None
                }
            }),
        NotifyKind::Rename if target_left_root => Some("target_out_of_scope".to_string()),
        NotifyKind::OpenCompleted => {
            if input.flags & flags::OPEN_PLACEHOLDER_UNSUPPORTED != 0 {
                Some("placeholder_unsupported".to_string())
            } else if input.flags & flags::OPEN_PLACEHOLDER_UNKNOWN != 0 {
                Some("placeholder_unknown".to_string())
            } else {
                None
            }
        }
        _ => None,
    };

    NotifyDecision::Event(Box::new(NativeSyncChangeEvent {
        kind: kind.as_str().to_string(),
        phase: match kind.phase() {
            NotifyPhase::Intent => "intent".to_string(),
            NotifyPhase::Completion => "completion".to_string(),
        },
        fs_id: scope.fs_id,
        from_relative,
        to_relative,
        is_dir,
        reason,
        source: scope.source.as_str().to_string(),
        detected_at: scope.detected_at.to_string(),
        // S1：本地已生效，但绝不传播到云端。
        pending_cloud: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "c:/users/tester/documents/pandock";

    fn scope() -> NotifyScope<'static> {
        NotifyScope {
            root: ROOT,
            fs_id: Some(77),
            source: NotifySource::Explorer,
            detected_at: "2026-10-12T00:00:00Z",
        }
    }

    fn event(input: &NotifyInput) -> NativeSyncChangeEvent {
        match classify(input, &scope()) {
            NotifyDecision::Event(e) => *e,
            other => panic!("期望事件，实际 {other:?}"),
        }
    }

    #[test]
    fn delete_intent_is_captured_with_relative_path() {
        let e = event(&NotifyInput {
            kind: Some(NotifyKind::Delete),
            flags: flags::IS_DIRECTORY,
            normalized_path: Some("/users/tester/documents/pandock/子目录".into()),
            ..Default::default()
        });
        assert_eq!((e.kind.as_str(), e.phase.as_str()), ("delete", "intent"));
        assert_eq!(e.from_relative.as_deref(), Some("子目录"));
        assert!(e.is_dir);
        assert!(e.pending_cloud, "S1 恒为本地待同步");
        assert_eq!(e.source, "explorer");
    }

    #[test]
    fn undelete_is_never_treated_as_delete() {
        let decision = classify(
            &NotifyInput {
                kind: Some(NotifyKind::Delete),
                flags: flags::DELETE_IS_UNDELETE,
                normalized_path: Some("/users/tester/documents/pandock/a.txt".into()),
                ..Default::default()
            },
            &scope(),
        );
        assert_eq!(decision, NotifyDecision::Ignore("undelete"));
    }

    #[test]
    fn rename_uses_target_path_for_new_name_and_keeps_case() {
        let e = event(&NotifyInput {
            kind: Some(NotifyKind::Rename),
            flags: flags::RENAME_SOURCE_IN_SCOPE | flags::RENAME_TARGET_IN_SCOPE,
            normalized_path: Some("/users/tester/documents/pandock/Old.TXT".into()),
            target_path: Some("/users/tester/documents/pandock/New.TXT".into()),
            ..Default::default()
        });
        assert_eq!(e.from_relative.as_deref(), Some("Old.TXT"));
        assert_eq!(e.to_relative.as_deref(), Some("New.TXT"));
    }

    #[test]
    fn rename_into_root_has_no_source_but_is_still_recorded() {
        let e = event(&NotifyInput {
            kind: Some(NotifyKind::Rename),
            flags: flags::RENAME_TARGET_IN_SCOPE,
            normalized_path: None,
            target_path: Some("/users/tester/documents/pandock/移入.txt".into()),
            ..Default::default()
        });
        assert_eq!(e.from_relative, None);
        assert_eq!(e.to_relative.as_deref(), Some("移入.txt"));
    }

    #[test]
    fn rename_out_of_root_is_recorded_as_leaving() {
        let e = event(&NotifyInput {
            kind: Some(NotifyKind::Rename),
            flags: flags::RENAME_SOURCE_IN_SCOPE,
            normalized_path: Some("/users/tester/documents/pandock/移出.txt".into()),
            target_path: Some("c:/其他位置/移出.txt".into()),
            ..Default::default()
        });
        assert_eq!(e.from_relative.as_deref(), Some("移出.txt"));
        assert_eq!(e.to_relative, None, "移出同步根没有根内相对路径");
        assert_eq!(e.reason.as_deref(), Some("target_out_of_scope"));
    }

    #[test]
    fn rename_completion_reports_source_path() {
        let e = event(&NotifyInput {
            kind: Some(NotifyKind::RenameCompleted),
            normalized_path: Some("/users/tester/documents/pandock/new.txt".into()),
            source_path: Some("/users/tester/documents/pandock/old.txt".into()),
            ..Default::default()
        });
        assert_eq!(e.kind, "rename");
        assert_eq!(e.phase, "completion");
        assert_eq!(e.from_relative.as_deref(), Some("old.txt"));
        assert_eq!(e.to_relative.as_deref(), Some("new.txt"));
    }

    #[test]
    fn paths_outside_the_sync_root_are_out_of_scope() {
        for (kind, path) in [
            (NotifyKind::Delete, "/users/tester/other/a.txt"),
            (NotifyKind::OpenCompleted, "/users/tester/other/a.txt"),
            (NotifyKind::CloseCompleted, "d:/elsewhere/a.txt"),
        ] {
            let decision = classify(
                &NotifyInput {
                    kind: Some(kind),
                    normalized_path: Some(path.into()),
                    ..Default::default()
                },
                &scope(),
            );
            assert_eq!(decision, NotifyDecision::OutOfScope, "{kind:?} {path}");
        }
    }

    #[test]
    fn relative_paths_get_reversible_sanitizing() {
        // 远端/本地名字里的 Windows 保留字符必须可逆净化，否则占位符建不出来。
        assert_eq!(
            sanitize_relative("a<b>c.txt").as_deref(),
            Some("a%3Cb%3Ec.txt")
        );
        assert_eq!(sanitize_relative("dir/CON").as_deref(), Some("dir/%00-CON"));
        assert_eq!(
            sanitize_relative("trailing.").as_deref(),
            Some("trailing%2E")
        );
        // 只有末尾空格属于 Windows 非法形态，sanitize_component 只替换末位
        assert_eq!(sanitize_relative("  ").as_deref(), Some(" %20"));
        assert_eq!(sanitize_relative("/"), None);
        assert_eq!(sanitize_relative(""), None);
    }

    #[test]
    fn dehydrate_reason_and_background_flag_are_reported() {
        let by_reason = event(&NotifyInput {
            kind: Some(NotifyKind::Dehydrate),
            reason: Some(2),
            normalized_path: Some("/users/tester/documents/pandock/a.txt".into()),
            ..Default::default()
        });
        assert_eq!(by_reason.reason.as_deref(), Some("low_disk"));
        let by_flag = event(&NotifyInput {
            kind: Some(NotifyKind::Dehydrate),
            flags: flags::DEHYDRATE_BACKGROUND,
            normalized_path: Some("/users/tester/documents/pandock/a.txt".into()),
            ..Default::default()
        });
        assert_eq!(by_flag.reason.as_deref(), Some("background"));
    }

    #[test]
    fn open_completion_flags_become_attention_reasons() {
        let unsupported = event(&NotifyInput {
            kind: Some(NotifyKind::OpenCompleted),
            flags: flags::OPEN_PLACEHOLDER_UNSUPPORTED,
            normalized_path: Some("/users/tester/documents/pandock/a.txt".into()),
            ..Default::default()
        });
        assert_eq!(
            unsupported.reason.as_deref(),
            Some("placeholder_unsupported")
        );
        let known = event(&NotifyInput {
            kind: Some(NotifyKind::OpenCompleted),
            normalized_path: Some("/users/tester/documents/pandock/a.txt".into()),
            ..Default::default()
        });
        assert_eq!(known.reason, None);
    }

    #[test]
    fn blocking_kinds_are_exactly_the_three_that_expect_a_response() {
        assert!(NotifyKind::Delete.is_blocking());
        assert!(NotifyKind::Rename.is_blocking());
        assert!(NotifyKind::Dehydrate.is_blocking());
        for kind in [
            NotifyKind::OpenCompleted,
            NotifyKind::CloseCompleted,
            NotifyKind::ValidateData,
            NotifyKind::DeleteCompleted,
            NotifyKind::PlaceholdersCancelled,
        ] {
            assert!(!kind.is_blocking(), "{kind:?} 不应阻塞 Explorer");
        }
    }

    #[test]
    fn placeholders_cancelled_is_observed_only() {
        assert_eq!(
            classify(
                &NotifyInput {
                    kind: Some(NotifyKind::PlaceholdersCancelled),
                    normalized_path: Some("/users/tester/documents/pandock/a.txt".into()),
                    ..Default::default()
                },
                &scope()
            ),
            NotifyDecision::Ignore("placeholders cancelled")
        );
    }

    #[test]
    fn notifications_switch_prefers_environment_variable() {
        // 环境变量优先于配置文件，保证卡死时免编辑恢复。
        let key = NOTIFICATIONS_ENV;
        std::env::remove_var(key);
        assert!(resolve_notifications_enabled(true));
        assert!(!resolve_notifications_enabled(false));
        std::env::set_var(key, "0");
        assert!(!resolve_notifications_enabled(true), "环境变量必须覆盖配置");
        std::env::set_var(key, "off");
        assert!(!resolve_notifications_enabled(true));
        std::env::set_var(key, "1");
        assert!(resolve_notifications_enabled(false), "显式开启也要生效");
        std::env::remove_var(key);
    }

    #[test]
    fn image_label_keeps_only_the_executable_basename() {
        assert_eq!(
            image_label("C:\\Windows\\explorer.exe").as_deref(),
            Some("explorer.exe")
        );
        assert_eq!(
            image_label("C:/Program Files/Pandock/pandock.exe").as_deref(),
            Some("pandock.exe")
        );
        // 设备前缀（\\?\ 形态）也要能吃下。
        assert_eq!(
            image_label("\\\\?\\C:\\Windows\\System32\\dllhost.exe").as_deref(),
            Some("dllhost.exe")
        );
        // 大小写归一，便于按映像聚合。
        assert_eq!(
            image_label("  C:\\Windows\\Explorer.EXE  ").as_deref(),
            Some("explorer.exe")
        );
        assert_eq!(image_label(""), None);
        assert_eq!(image_label("   "), None);
        assert_eq!(image_label("C:\\Windows\\").as_deref(), Some("windows"));
    }

    #[test]
    fn identity_log_limiter_collapses_repeats_inside_the_window() {
        let mut limiter = IdentityLogLimiter::new(IDENTITY_LOG_WINDOW_MS);
        assert!(limiter.should_log(1, "explorer.exe", 1_000));
        // 同一 (fs_id, image) 在窗口内只记一条。
        assert!(!limiter.should_log(1, "explorer.exe", 1_000 + IDENTITY_LOG_WINDOW_MS - 1));
        // 换文件（fs_id）或换打开者（image）都算新键，必须能看到。
        assert!(limiter.should_log(2, "explorer.exe", 1_100));
        assert!(limiter.should_log(1, "pandock.exe", 1_100));
        // 到期后再次放行。
        assert!(limiter.should_log(1, "explorer.exe", 1_000 + IDENTITY_LOG_WINDOW_MS));
        assert_eq!(limiter.tracked(), 3);
    }

    #[test]
    fn identity_log_limiter_prunes_expired_keys() {
        let mut limiter = IdentityLogLimiter::new(IDENTITY_LOG_WINDOW_MS);
        for fs_id in 0..50 {
            assert!(limiter.should_log(fs_id, "explorer.exe", 10_000));
        }
        assert_eq!(limiter.tracked(), 50);
        // 一个窗口之后旧键全部过期，只留刚记下的这一条。
        assert!(limiter.should_log(999, "explorer.exe", 10_000 + IDENTITY_LOG_WINDOW_MS));
        assert_eq!(limiter.tracked(), 1);
    }
}
