//! Adapter-neutral outbound events.
//!
//! Core flows produce semantic outbound messages here. Concrete adapters decide
//! whether those messages become plain text, buttons, templates, embeds, or
//! other platform-specific UI.

use async_trait::async_trait;
use miette::Result;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Per-account adapter delivery targets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterAccountTarget {
    pub account_id: String,
    pub line_user_id: String,
    pub discord_user_id: String,
}

impl AdapterAccountTarget {
    pub fn new(
        account_id: impl Into<String>,
        line_user_id: impl Into<String>,
        discord_user_id: impl Into<String>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            line_user_id: line_user_id.into(),
            discord_user_id: discord_user_id.into(),
        }
    }
}

/// Common messaging capability implemented by concrete adapters.
#[async_trait]
pub trait AdapterMessenger: Send + Sync {
    fn adapter_name(&self) -> &'static str;

    fn admin_user_id(&self) -> &str;

    async fn reply(&self, reply_token: &str, message: &OutboundMessage) -> Result<()>;

    async fn push(&self, to: &str, message: &OutboundMessage) -> Result<()>;

    async fn push_to_user_or_admin(&self, user_id: &str, message: &OutboundMessage) -> Result<()> {
        let to = if !user_id.is_empty() {
            user_id
        } else if !self.admin_user_id().is_empty() {
            self.admin_user_id()
        } else {
            return Err(miette::miette!(
                "user_id 與 admin_user_id 均未設定，無法推送通知"
            ));
        };

        self.push(to, message).await
    }

    async fn push_to_account_or_admin(
        &self,
        target: &AdapterAccountTarget,
        message: &OutboundMessage,
    ) -> Result<()> {
        self.push_to_user_or_admin(&target.line_user_id, message)
            .await
    }
}

/// Fan-out messenger used by monitor/rollcall flows when multiple adapters are enabled.
pub struct AdapterHub {
    messengers: Vec<std::sync::Arc<dyn AdapterMessenger>>,
}

impl AdapterHub {
    pub fn new(messengers: Vec<std::sync::Arc<dyn AdapterMessenger>>) -> Self {
        Self { messengers }
    }
}

#[async_trait]
impl AdapterMessenger for AdapterHub {
    fn adapter_name(&self) -> &'static str {
        "hub"
    }

    fn admin_user_id(&self) -> &str {
        ""
    }

    async fn reply(&self, _reply_token: &str, _message: &OutboundMessage) -> Result<()> {
        Err(miette::miette!(
            "AdapterHub 不支援 reply；請使用具體 adapter"
        ))
    }

    async fn push(&self, to: &str, message: &OutboundMessage) -> Result<()> {
        for messenger in &self.messengers {
            if let Err(e) = messenger.push(to, message).await {
                warn!(
                    adapter = messenger.adapter_name(),
                    error = %e,
                    "adapter hub push 失敗"
                );
            }
        }
        Ok(())
    }

    async fn push_to_account_or_admin(
        &self,
        target: &AdapterAccountTarget,
        message: &OutboundMessage,
    ) -> Result<()> {
        for messenger in &self.messengers {
            if let Err(e) = messenger.push_to_account_or_admin(target, message).await {
                warn!(
                    adapter = messenger.adapter_name(),
                    account = %target.account_id,
                    error = %e,
                    "adapter hub account push 失敗"
                );
            }
        }
        Ok(())
    }
}

/// Adapter-neutral outbound content.
///
/// Core logic produces these semantic messages. Each adapter decides whether to
/// render them as plain text, buttons, templates, embeds, or platform-specific
/// interactive UI.
#[derive(Debug, Clone, PartialEq)]
pub enum OutboundMessage {
    Text(String),
    SystemStarted(SystemStartedEvent),
    RollcallDetected(RollcallEvent),
    QrCodeRequested(QrCodeRequest),
    RollcallFinished(RollcallResultEvent),
    Help,
    Welcome,
    UnsupportedMedia,
    LocationReceived { latitude: f64, longitude: f64 },
    Status(StatusMessage),
    NotAuthorized,
    MonitorPaused,
    MonitorResumed,
    ForcePollTriggered,
    ReauthTriggered,
    QrAccepted,
    QrAmbiguousTarget,
    QrNoBoundAccount,
    QrNoPendingRequest,
    UnknownCommand { text: String },
}

/// Outbound event emitted when a monitored account starts.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemStartedEvent {
    pub account: String,
    pub user_name: String,
    pub poll_interval_secs: u64,
    pub adapter_name: String,
}

/// Outbound event emitted when a pending rollcall is found.
#[derive(Debug, Clone, PartialEq)]
pub struct RollcallEvent {
    pub rollcall_id: u64,
    pub account: String,
    pub course_name: String,
    pub teacher_name: String,
    pub attendance_type: String,
}

/// Outbound event asking the user to provide QR Code input.
#[derive(Debug, Clone, PartialEq)]
pub struct QrCodeRequest {
    pub rollcall_id: u64,
    pub account: String,
    pub course_name: String,
    pub teacher_name: String,
    pub scan_url: String,
    pub timeout_secs: u64,
}

/// Outbound event emitted when a rollcall attempt finishes.
#[derive(Debug, Clone, PartialEq)]
pub struct RollcallResultEvent {
    pub rollcall_id: u64,
    pub account: String,
    pub course_name: String,
    pub attendance_type: String,
    pub success: bool,
    pub result: String,
    pub elapsed_ms: u64,
}

/// Status payload to be rendered by an adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusMessage {
    NoAccounts,
    Single(MonitorStatus),
    UserAccount {
        account_id: String,
        status: MonitorStatus,
    },
    AdminAccounts(Vec<AccountStatusMessage>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountStatusMessage {
    pub account_id: String,
    pub status: MonitorStatus,
}

/// Shared monitor status used by adapter status replies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorStatus {
    pub is_running: bool,
    pub user_name: String,
    pub last_poll_timestamp: Option<i64>,
    pub last_success_course: Option<String>,
    pub consecutive_failures: u32,
    pub started_at: i64,
}
