use serde::{Deserialize, Serialize};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthPhase {
    Disconnected,
    EditingCredentials,
    RequestingDeviceCode,
    WaitingAuthorization,
    ExchangingToken,
    StartingWebdav,
    Connected,
    Cancelling,
    Cancelled,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthErrorCode {
    InvalidClient,
    AccessDenied,
    ExpiredToken,
    NetworkUnreachable,
    Timeout,
    KeyringFailed,
    WebdavStartFailed,
    Internal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthErrorView {
    pub code: AuthErrorCode,
    pub message: String,
    pub retryable: bool,
    pub request_id: Option<String>,
}

impl AuthErrorView {
    pub fn new(code: AuthErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            request_id: None,
        }
    }

    pub fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        let lower = message.to_lowercase();
        let (code, retryable) = if lower.contains("invalid_client") {
            (AuthErrorCode::InvalidClient, false)
        } else if lower.contains("access_denied") || lower.contains("拒绝") {
            (AuthErrorCode::AccessDenied, false)
        } else if lower.contains("expired") || lower.contains("过期") {
            (AuthErrorCode::ExpiredToken, true)
        } else if lower.contains("timeout") || lower.contains("超时") {
            (AuthErrorCode::Timeout, true)
        } else if lower.contains("网络")
            || lower.contains("connection")
            || lower.contains("unreachable")
            || lower.contains("dns")
        {
            (AuthErrorCode::NetworkUnreachable, true)
        } else {
            (AuthErrorCode::Internal, true)
        };
        Self::new(code, message, retryable)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationChallenge {
    pub flow_id: String,
    pub user_code: Option<String>,
    pub verification_url: Option<String>,
    pub qr_code_url: Option<String>,
    pub expires_at: i64,
    pub poll_interval_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStateView {
    pub phase: AuthPhase,
    pub app_key: Option<String>,
    pub app_name: Option<String>,
    pub challenge: Option<AuthorizationChallenge>,
    pub error: Option<AuthErrorView>,
    pub connected_at: Option<i64>,
}

impl Default for AuthStateView {
    fn default() -> Self {
        Self {
            phase: AuthPhase::Disconnected,
            app_key: None,
            app_name: None,
            challenge: None,
            error: None,
            connected_at: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AuthStateMachine {
    state: AuthStateView,
}

impl Default for AuthStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthStateMachine {
    pub fn new() -> Self {
        Self {
            state: AuthStateView::default(),
        }
    }

    pub fn view(&self) -> AuthStateView {
        self.state.clone()
    }

    pub fn editing_credentials(&mut self, app_key: impl Into<String>, app_name: impl Into<String>) {
        self.state.phase = AuthPhase::EditingCredentials;
        self.state.app_key = Some(app_key.into());
        self.state.app_name = Some(app_name.into());
        self.state.challenge = None;
        self.state.error = None;
    }

    pub fn requesting_device_code(&mut self) {
        self.state.phase = AuthPhase::RequestingDeviceCode;
        self.state.challenge = None;
        self.state.error = None;
    }

    pub fn waiting_authorization(&mut self, challenge: AuthorizationChallenge) {
        self.state.phase = AuthPhase::WaitingAuthorization;
        self.state.challenge = Some(challenge);
        self.state.error = None;
    }

    pub fn exchanging_token(&mut self) {
        self.state.phase = AuthPhase::ExchangingToken;
    }

    pub fn starting_webdav(&mut self) {
        self.state.phase = AuthPhase::StartingWebdav;
    }

    pub fn connected(&mut self, connected_at: i64) {
        self.state.phase = AuthPhase::Connected;
        self.state.challenge = None;
        self.state.error = None;
        self.state.connected_at = Some(connected_at);
    }

    pub fn cancelling(&mut self) {
        self.state.phase = AuthPhase::Cancelling;
    }

    pub fn cancelled(&mut self) {
        self.state.phase = AuthPhase::Cancelled;
        self.state.challenge = None;
        self.state.error = None;
    }

    pub fn failed(&mut self, error: AuthErrorView) {
        self.state.phase = AuthPhase::Error;
        self.state.error = Some(error);
    }

    pub fn disconnected(&mut self) {
        self.state.phase = AuthPhase::Disconnected;
        self.state.challenge = None;
        self.state.error = None;
        self.state.connected_at = None;
    }
}

#[derive(Clone)]
pub struct AuthSession {
    flow_id: String,
    app_key: String,
    app_name: String,
    app_secret: String,
}

impl AuthSession {
    pub fn new(
        flow_id: impl Into<String>,
        app_key: impl Into<String>,
        app_name: impl Into<String>,
        app_secret: impl Into<String>,
    ) -> Self {
        Self {
            flow_id: flow_id.into(),
            app_key: app_key.into(),
            app_name: app_name.into(),
            app_secret: app_secret.into(),
        }
    }

    pub fn flow_id(&self) -> &str {
        &self.flow_id
    }

    pub fn app_key(&self) -> &str {
        &self.app_key
    }

    pub fn app_name(&self) -> &str {
        &self.app_name
    }

    pub fn app_secret(&self) -> &str {
        &self.app_secret
    }
}

struct ActiveAuthFlow {
    session: AuthSession,
    cancel_tx: watch::Sender<bool>,
}

pub enum CancelOutcome {
    Ignored,
    Cancelled(AuthSession),
}

pub struct AuthFlowCoordinator {
    state: AuthStateMachine,
    active: Option<ActiveAuthFlow>,
}

impl Default for AuthFlowCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthFlowCoordinator {
    pub fn new() -> Self {
        Self {
            state: AuthStateMachine::new(),
            active: None,
        }
    }

    pub fn view(&self) -> AuthStateView {
        self.state.view()
    }

    pub fn is_active(&self, flow_id: &str) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.session.flow_id() == flow_id)
    }

    pub fn active_session(&self, flow_id: &str) -> Option<AuthSession> {
        self.active
            .as_ref()
            .filter(|active| active.session.flow_id() == flow_id)
            .map(|active| active.session.clone())
    }

    pub fn editing_credentials(&mut self, app_key: impl Into<String>, app_name: impl Into<String>) {
        self.state.editing_credentials(app_key, app_name);
    }

    pub fn requesting_device_code(&mut self) {
        self.state.requesting_device_code();
    }

    pub fn waiting_authorization(
        &mut self,
        session: AuthSession,
        challenge: AuthorizationChallenge,
        cancel_tx: watch::Sender<bool>,
    ) {
        self.active = Some(ActiveAuthFlow { session, cancel_tx });
        self.state.waiting_authorization(challenge);
    }

    pub fn exchanging_token(&mut self) {
        self.state.exchanging_token();
    }

    pub fn starting_webdav(&mut self) {
        self.state.starting_webdav();
    }

    pub fn connected(&mut self, connected_at: i64) {
        self.state.connected(connected_at);
    }

    pub fn failed(&mut self, error: AuthErrorView) {
        self.state.failed(error);
    }

    pub fn cancel(&mut self, flow_id: &str) -> CancelOutcome {
        if !self.is_active(flow_id) {
            return CancelOutcome::Ignored;
        }

        let active = self
            .active
            .take()
            .expect("active flow checked immediately above");
        let _ = active.cancel_tx.send(true);
        self.state.cancelling();
        self.state.cancelled();
        CancelOutcome::Cancelled(active.session)
    }

    pub fn finish_cancelled(&mut self) {
        if self.state.view().phase == AuthPhase::Cancelled {
            self.state.disconnected();
        }
    }

    pub fn invalidate_active(&mut self) -> Option<AuthSession> {
        let active = self.active.take()?;
        let _ = active.cancel_tx.send(true);
        Some(active.session)
    }

    pub fn accept_token(&mut self, flow_id: &str) -> Option<AuthSession> {
        if !self.is_active(flow_id) {
            return None;
        }
        self.active.take().map(|active| active.session)
    }

    pub fn invalidate_and_disconnect(&mut self) {
        let _ = self.invalidate_active();
        self.state.disconnected();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge() -> AuthorizationChallenge {
        AuthorizationChallenge {
            flow_id: "flow-1".into(),
            user_code: Some("ABCD".into()),
            verification_url: Some("https://example.invalid/device".into()),
            qr_code_url: Some("https://example.invalid/qr.png".into()),
            expires_at: 123,
            poll_interval_seconds: 6,
        }
    }

    fn session(flow_id: &str) -> AuthSession {
        AuthSession::new(flow_id, "key", "App", "secret")
    }

    #[test]
    fn happy_path_transitions_to_connected() {
        let mut state = AuthStateMachine::new();
        state.editing_credentials("key", "App");
        assert_eq!(state.view().phase, AuthPhase::EditingCredentials);
        state.requesting_device_code();
        state.waiting_authorization(challenge());
        state.exchanging_token();
        state.starting_webdav();
        state.connected(456);
        let view = state.view();
        assert_eq!(view.phase, AuthPhase::Connected);
        assert_eq!(view.connected_at, Some(456));
        assert!(view.challenge.is_none());
    }

    #[test]
    fn cancel_clears_challenge() {
        let mut state = AuthStateMachine::new();
        state.waiting_authorization(challenge());
        state.cancelling();
        state.cancelled();
        let view = state.view();
        assert_eq!(view.phase, AuthPhase::Cancelled);
        assert!(view.challenge.is_none());
    }

    #[test]
    fn errors_are_classified_with_retry_policy() {
        let invalid = AuthErrorView::from_message("invalid_client");
        assert_eq!(invalid.code, AuthErrorCode::InvalidClient);
        assert!(!invalid.retryable);
        let timeout = AuthErrorView::from_message("request timeout");
        assert_eq!(timeout.code, AuthErrorCode::Timeout);
        assert!(timeout.retryable);
        let denied = AuthErrorView::from_message("access_denied");
        assert_eq!(denied.code, AuthErrorCode::AccessDenied);
        assert!(!denied.retryable);
    }

    #[test]
    fn expired_network_and_internal_errors_are_classified() {
        let expired = AuthErrorView::from_message("token expired");
        assert_eq!(expired.code, AuthErrorCode::ExpiredToken);
        assert!(expired.retryable);

        for message in [
            "网络不可达",
            "connection reset",
            "host unreachable",
            "dns failure",
        ] {
            let error = AuthErrorView::from_message(message);
            assert_eq!(error.code, AuthErrorCode::NetworkUnreachable, "{message}");
            assert!(error.retryable);
        }

        let internal = AuthErrorView::from_message("unexpected upstream response");
        assert_eq!(internal.code, AuthErrorCode::Internal);
        assert!(internal.retryable);
    }

    #[test]
    fn default_and_terminal_transitions_clear_transient_data() {
        let mut state = AuthStateMachine::default();
        assert_eq!(state.view().phase, AuthPhase::Disconnected);

        state.waiting_authorization(challenge());
        state.failed(AuthErrorView::new(AuthErrorCode::Internal, "boom", true));
        assert_eq!(state.view().phase, AuthPhase::Error);
        assert!(state.view().error.is_some());

        state.disconnected();
        let view = state.view();
        assert_eq!(view.phase, AuthPhase::Disconnected);
        assert!(view.challenge.is_none());
        assert!(view.error.is_none());
        assert!(view.connected_at.is_none());
    }

    #[test]
    fn coordinator_wrappers_delegate_state_transitions() {
        let mut coordinator = AuthFlowCoordinator::default();
        coordinator.editing_credentials("key", "App");
        assert_eq!(coordinator.view().phase, AuthPhase::EditingCredentials);
        coordinator.requesting_device_code();
        assert_eq!(coordinator.view().phase, AuthPhase::RequestingDeviceCode);

        let (cancel_tx, _cancel_rx) = watch::channel(false);
        coordinator.waiting_authorization(session("flow-1"), challenge(), cancel_tx);
        assert_eq!(coordinator.view().phase, AuthPhase::WaitingAuthorization);

        coordinator.exchanging_token();
        assert_eq!(coordinator.view().phase, AuthPhase::ExchangingToken);
        coordinator.starting_webdav();
        assert_eq!(coordinator.view().phase, AuthPhase::StartingWebdav);
        coordinator.connected(123);
        assert_eq!(coordinator.view().phase, AuthPhase::Connected);
        coordinator.failed(AuthErrorView::new(AuthErrorCode::Internal, "boom", true));
        assert_eq!(coordinator.view().phase, AuthPhase::Error);

        coordinator.invalidate_and_disconnect();
        assert_eq!(coordinator.view().phase, AuthPhase::Disconnected);
    }

    #[test]
    fn stale_cancel_is_noop_and_keeps_current_flow() {
        let (old_tx, _old_rx) = watch::channel(false);
        let (current_tx, _current_rx) = watch::channel(false);
        let mut coordinator = AuthFlowCoordinator::new();
        coordinator.waiting_authorization(session("old"), challenge(), old_tx);
        coordinator.waiting_authorization(session("current"), challenge(), current_tx);

        assert!(matches!(coordinator.cancel("old"), CancelOutcome::Ignored));
        assert!(coordinator.is_active("current"));
        assert!(!coordinator.is_active("old"));
        assert_eq!(coordinator.view().phase, AuthPhase::WaitingAuthorization);
        assert!(coordinator.accept_token("current").is_some());
    }

    #[test]
    fn current_cancel_clears_session_and_signal_and_transitions_to_disconnected() {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let mut coordinator = AuthFlowCoordinator::new();
        coordinator.waiting_authorization(session("flow-1"), challenge(), cancel_tx);

        let session = coordinator.active_session("flow-1").unwrap();
        assert_eq!(session.app_key(), "key");
        assert_eq!(session.app_name(), "App");
        assert_eq!(session.app_secret(), "secret");
        assert!(coordinator.active_session("missing").is_none());

        let outcome = coordinator.cancel("flow-1");
        assert!(matches!(outcome, CancelOutcome::Cancelled(_)));
        assert!(!coordinator.is_active("flow-1"));
        assert_eq!(coordinator.view().phase, AuthPhase::Cancelled);
        assert!(*cancel_rx.borrow());
        assert!(matches!(
            coordinator.cancel("flow-1"),
            CancelOutcome::Ignored
        ));

        coordinator.finish_cancelled();
        assert_eq!(coordinator.view().phase, AuthPhase::Disconnected);
        assert!(matches!(
            coordinator.cancel("flow-1"),
            CancelOutcome::Ignored
        ));
    }

    #[test]
    fn cancelled_flow_cannot_accept_token_after_invalidation() {
        let (cancel_tx, _cancel_rx) = watch::channel(false);
        let mut coordinator = AuthFlowCoordinator::new();
        coordinator.waiting_authorization(session("flow-1"), challenge(), cancel_tx);
        assert!(matches!(
            coordinator.cancel("flow-1"),
            CancelOutcome::Cancelled(_)
        ));

        assert!(coordinator.accept_token("flow-1").is_none());
        assert_eq!(coordinator.view().phase, AuthPhase::Cancelled);
    }

    #[test]
    fn successful_flow_takes_precedence_over_late_cancel() {
        let (cancel_tx, _cancel_rx) = watch::channel(false);
        let mut coordinator = AuthFlowCoordinator::new();
        coordinator.waiting_authorization(session("flow-1"), challenge(), cancel_tx);

        assert!(coordinator.accept_token("flow-1").is_some());
        assert!(matches!(
            coordinator.cancel("flow-1"),
            CancelOutcome::Ignored
        ));
    }

    #[test]
    fn invalidating_active_flow_stops_stale_completion() {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let mut coordinator = AuthFlowCoordinator::new();
        coordinator.waiting_authorization(session("flow-1"), challenge(), cancel_tx);

        assert!(coordinator.invalidate_active().is_some());
        assert!(*cancel_rx.borrow());
        assert!(coordinator.accept_token("flow-1").is_none());
    }
}
