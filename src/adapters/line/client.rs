//! LINE Messaging API client.

use std::time::Duration;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{Local, TimeZone};
use hmac::{Hmac, Mac};
use miette::{IntoDiagnostic, Result, WrapErr};
use reqwest::Client;
use serde_json::{json, Value};
use sha2::Sha256;
use tracing::{debug, error, warn};

use crate::adapters::events::{AdapterMessenger, MonitorStatus, OutboundMessage, StatusMessage};
use crate::config::LineBotConfig;

use super::types::{
    LineApiError, PushMessageRequest, ReplyMessageRequest, SendFlexMessage, SendMessage,
};

const LINE_REPLY_API: &str = "https://api.line.me/v2/bot/message/reply";
const LINE_PUSH_API: &str = "https://api.line.me/v2/bot/message/push";

/// LINE Bot API client.
#[derive(Clone)]
pub struct LineBotClient {
    http: Client,
    channel_access_token: String,
    channel_secret: String,
    admin_user_id: String,
}

impl LineBotClient {
    pub fn new(config: &LineBotConfig) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .use_rustls_tls()
            .build()
            .into_diagnostic()
            .wrap_err("Failed to build Line Bot HTTP client")?;

        Ok(Self {
            http,
            channel_access_token: config.channel_access_token.clone(),
            channel_secret: config.channel_secret.clone(),
            admin_user_id: config.admin_user_id.clone(),
        })
    }

    pub fn verify_signature(&self, body: &[u8], signature: &str) -> bool {
        let sig_bytes = match BASE64.decode(signature) {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(error = %e, "X-Line-Signature Base64 decode 失敗");
                return false;
            }
        };

        let mut mac = match Hmac::<Sha256>::new_from_slice(self.channel_secret.as_bytes()) {
            Ok(m) => m,
            Err(e) => {
                error!(error = %e, "HMAC 初始化失敗（channel_secret 可能無效）");
                return false;
            }
        };

        mac.update(body);
        let computed = mac.finalize().into_bytes();

        constant_time_compare(&computed, &sig_bytes)
    }

    #[allow(dead_code)]
    pub async fn reply_text(&self, reply_token: &str, text: &str) -> Result<()> {
        let req = ReplyMessageRequest::text(reply_token, text);

        debug!(
            reply_token = %&reply_token[..reply_token.len().min(10)],
            text_len = text.len(),
            "發送 Reply 訊息"
        );

        self.send_reply(req).await
    }

    async fn send_reply(&self, req: ReplyMessageRequest) -> Result<()> {
        let resp = self
            .http
            .post(LINE_REPLY_API)
            .bearer_auth(&self.channel_access_token)
            .json(&req)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("Failed to send Line reply")?;

        handle_line_api_response(resp, "reply").await
    }

    #[allow(dead_code)]
    pub async fn push_message(&self, to: &str, text: &str) -> Result<()> {
        let req = PushMessageRequest::text(to, text);

        debug!(
            to = %&to[..to.len().min(10)],
            text_len = text.len(),
            "發送 Push 訊息"
        );

        self.send_push(req).await
    }

    async fn send_push(&self, req: PushMessageRequest) -> Result<()> {
        let resp = self
            .http
            .post(LINE_PUSH_API)
            .bearer_auth(&self.channel_access_token)
            .json(&req)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("Failed to send Line push message")?;

        handle_line_api_response(resp, "push").await
    }

    pub fn admin_user_id(&self) -> &str {
        &self.admin_user_id
    }
}

#[async_trait]
impl AdapterMessenger for LineBotClient {
    fn adapter_name(&self) -> &'static str {
        "line"
    }

    fn admin_user_id(&self) -> &str {
        self.admin_user_id()
    }

    async fn reply(&self, reply_token: &str, message: &OutboundMessage) -> Result<()> {
        let messages = render_line_messages(message);
        let req = ReplyMessageRequest::messages(reply_token, messages);
        self.send_reply(req).await
    }

    async fn push(&self, to: &str, message: &OutboundMessage) -> Result<()> {
        let messages = render_line_messages(message);
        let req = PushMessageRequest::messages(to, messages);
        self.send_push(req).await
    }
}

fn render_line_messages(message: &OutboundMessage) -> Vec<SendMessage> {
    match message {
        OutboundMessage::SystemStarted(event) => vec![flex_bubble(
            "Tronclass Rollcall 已啟動",
            "Tronclass Rollcall 已啟動",
            "#16A34A",
            vec![
                text_row("帳號", &event.account),
                text_row("使用者", &event.user_name),
                text_row("輪詢間隔", format!("{} 秒", event.poll_interval_secs)),
                text_row("Adapter", &event.adapter_name),
            ],
            vec![
                postback_action("查看狀態", "/status"),
                postback_action("立即檢查", "/force"),
                postback_action("說明", "/help"),
            ],
        )],
        OutboundMessage::RollcallDetected(event) => vec![flex_bubble(
            format!("偵測到新簽到：{}", event.course_name),
            "偵測到新簽到",
            "#2563EB",
            vec![
                text_row("課程", &event.course_name),
                text_row("教師", &event.teacher_name),
                text_row("類型", &event.attendance_type),
                text_row("點名 ID", event.rollcall_id.to_string()),
                text_row("帳號", &event.account),
            ],
            vec![postback_action("查看狀態", "/status")],
        )],
        OutboundMessage::QrCodeRequested(request) => vec![flex_bubble(
            format!("QR Code 簽到：{}", request.course_name),
            "需要 QR Code 簽到",
            "#F59E0B",
            vec![
                text_row("課程", &request.course_name),
                text_row("教師", &request.teacher_name),
                text_row("點名 ID", request.rollcall_id.to_string()),
                text_row("帳號", &request.account),
                text_row("期限", format!("{} 秒內回覆", request.timeout_secs)),
            ],
            vec![
                uri_action("開啟掃碼頁", &request.scan_url),
                postback_action("查看狀態", "/status"),
            ],
        )],
        OutboundMessage::RollcallFinished(event) => {
            let (header, color) = if event.success {
                ("簽到成功", "#16A34A")
            } else {
                ("簽到未成功", "#DC2626")
            };
            vec![flex_bubble(
                format!("{header}：{}", event.course_name),
                header,
                color,
                vec![
                    text_row("課程", &event.course_name),
                    text_row("類型", &event.attendance_type),
                    text_row("點名 ID", event.rollcall_id.to_string()),
                    text_row("帳號", &event.account),
                    text_row("結果", &event.result),
                    text_row("耗時", format!("{} ms", event.elapsed_ms)),
                ],
                vec![postback_action("查看狀態", "/status")],
            )]
        }
        OutboundMessage::Status(status) => render_status_messages(status),
        OutboundMessage::Help => vec![help_flex("使用說明", "Tronclass Rollcall 指令")],
        OutboundMessage::Welcome => vec![help_flex("歡迎使用", "Tronclass Rollcall")],
        _ => vec![SendMessage::text(render_message(message))],
    }
}

fn render_message(message: &OutboundMessage) -> String {
    match message {
        OutboundMessage::Text(text) => text.clone(),
        OutboundMessage::SystemStarted(event) => format!(
            "🚀 Tronclass Rollcall 已啟動\n\
             帳號：{} / {}\n\
             輪詢間隔：{} 秒\n\
             Adapter：{}\n\n\
             輸入 /help 查看可用指令",
            event.account, event.user_name, event.poll_interval_secs, event.adapter_name,
        ),
        OutboundMessage::RollcallDetected(event) => format!(
            "📋 偵測到新簽到\n\
             帳號：{}\n\
             課程：{}\n\
             教師：{}\n\
             類型：{}\n\
             點名 ID：{}\n\
             開始自動簽到...",
            event.account,
            event.course_name,
            event.teacher_name,
            event.attendance_type,
            event.rollcall_id,
        ),
        OutboundMessage::QrCodeRequested(request) => format!(
            "📷 需要 QR Code 簽到\n\
             課程：{}\n\
             帳號：{}\n\
             教師：{}\n\
             點名 ID：{}\n\
             \n\
             請到以下連結掃描 QR Code，\n\
             然後將掃描結果（URL）傳送到此對話：\n\
             {}\n\
             \n\
             ⏰ 請在 {} 秒內回覆，否則簽到逾時",
            request.course_name,
            request.account,
            request.teacher_name,
            request.rollcall_id,
            request.scan_url,
            request.timeout_secs,
        ),
        OutboundMessage::RollcallFinished(event) => {
            let emoji = if event.success { "✅" } else { "❌" };
            format!(
                "{emoji} 簽到結果\n\
                 帳號：{}\n\
                 課程：{}\n\
                 類型：{}\n\
                 點名 ID：{}\n\
                 結果：{}\n\
                 耗時：{}ms",
                event.account,
                event.course_name,
                event.attendance_type,
                event.rollcall_id,
                event.result,
                event.elapsed_ms,
            )
        }
        OutboundMessage::Help => help_text().to_string(),
        OutboundMessage::Welcome => format!(
            "👋 歡迎使用 Tronclass Rollcall！\n\n我會自動幫你完成 Tronclass 簽到。\n\n{}",
            help_text()
        ),
        OutboundMessage::UnsupportedMedia => "⚠️ 不支援媒體訊息，請傳送文字指令。".to_string(),
        OutboundMessage::LocationReceived {
            latitude,
            longitude,
        } => format!(
            "📍 收到位置訊息：\n緯度：{latitude:.6}\n經度：{longitude:.6}\n\n（位置功能尚未實作）"
        ),
        OutboundMessage::Status(status) => render_status_message(status),
        OutboundMessage::NotAuthorized => {
            "⚠️ 你可以查詢自己的帳號狀態，但此操作僅限管理員使用。".to_string()
        }
        OutboundMessage::MonitorPaused => {
            "⏸️ 簽到監控已暫停。\n輸入 /start 或 啟動 來恢復。".to_string()
        }
        OutboundMessage::MonitorResumed => "▶️ 簽到監控已恢復，立即執行一次簽到檢查...".to_string(),
        OutboundMessage::ForcePollTriggered => "🔄 已觸發立即簽到檢查，請稍候...".to_string(),
        OutboundMessage::ReauthTriggered => "🔐 已觸發重新認證，請稍候...".to_string(),
        OutboundMessage::QrAccepted => "✅ 已收到 QR Code，正在嘗試簽到，請稍候...".to_string(),
        OutboundMessage::QrAmbiguousTarget => {
            "⚠️ 多帳號模式下 QR Code 目標不明，請由綁定該帳號的使用者傳送 QR Code。".to_string()
        }
        OutboundMessage::QrNoBoundAccount => {
            "⚠️ 找不到與你的帳號綁定。請確認帳號資料中的 user_id 是否正確。".to_string()
        }
        OutboundMessage::QrNoPendingRequest => {
            "⚠️ 無法傳送 QR Code：目前沒有等待 QR Code 的簽到，或簽到已逾時。".to_string()
        }
        OutboundMessage::UnknownCommand { text } => format!(
            "❓ 不認識的指令：{}\n\n輸入 /help 或 幫助 查看可用指令。",
            &text[..text.len().min(50)]
        ),
    }
}

fn render_status_messages(message: &StatusMessage) -> Vec<SendMessage> {
    match message {
        StatusMessage::NoAccounts => vec![SendMessage::text(render_status_message(message))],
        StatusMessage::Single(status) => vec![status_flex(
            "系統狀態",
            None,
            status,
            vec![
                postback_action("立即檢查", "/force"),
                postback_action("重新認證", "/reauth"),
            ],
        )],
        StatusMessage::UserAccount { account_id, status } => vec![status_flex(
            "你的帳號狀態",
            Some(account_id),
            status,
            vec![postback_action("立即檢查", "/force")],
        )],
        StatusMessage::AdminAccounts(_) => vec![SendMessage::text(render_status_message(message))],
    }
}

fn render_status_message(message: &StatusMessage) -> String {
    match message {
        StatusMessage::NoAccounts => "📊 系統狀態\n目前沒有可查詢的 Tronclass 帳號。".to_string(),
        StatusMessage::Single(status) => render_monitor_status(status),
        StatusMessage::UserAccount { account_id, status } => format!(
            "📊 你的 Tronclass 帳號狀態\n帳號 ID：{}\n{}",
            account_id,
            render_monitor_status(status)
        ),
        StatusMessage::AdminAccounts(accounts) => {
            let mut lines = vec![format!("📊 系統狀態（{} 個帳號）", accounts.len())];
            for account in accounts {
                let status = &account.status;
                let running = if status.is_running {
                    "運行中"
                } else {
                    "已暫停"
                };
                let last_poll = status
                    .last_poll_timestamp
                    .map(format_unix_time)
                    .unwrap_or_else(|| "尚未輪詢".to_string());
                let last_success = status.last_success_course.as_deref().unwrap_or("無");
                lines.push(format!(
                    "\n帳號：{}\n使用者：{}\n狀態：{}\n最後輪詢：{}\n最後成功：{}\n連續失敗：{} 次",
                    account.account_id,
                    status.user_name,
                    running,
                    last_poll,
                    last_success,
                    status.consecutive_failures,
                ));
            }
            lines.join("\n")
        }
    }
}

fn flex_bubble(
    alt_text: impl Into<String>,
    header: impl Into<String>,
    status_color: &str,
    rows: Vec<Value>,
    actions: Vec<Value>,
) -> SendMessage {
    let mut contents = json!({
        "type": "bubble",
        "size": "mega",
        "header": {
            "type": "box",
            "layout": "vertical",
            "backgroundColor": status_color,
            "paddingAll": "16px",
            "contents": [
                {
                    "type": "text",
                    "text": header.into(),
                    "weight": "bold",
                    "size": "lg",
                    "color": "#FFFFFF",
                    "wrap": true
                }
            ]
        },
        "body": {
            "type": "box",
            "layout": "vertical",
            "spacing": "md",
            "paddingAll": "18px",
            "contents": rows
        }
    });

    if !actions.is_empty() {
        contents["footer"] = json!({
            "type": "box",
            "layout": "vertical",
            "spacing": "sm",
            "paddingAll": "14px",
            "contents": actions
        });
    }

    SendMessage::Flex(SendFlexMessage {
        alt_text: alt_text.into(),
        contents,
    })
}

fn text_row(label: impl Into<String>, value: impl Into<String>) -> Value {
    json!({
        "type": "box",
        "layout": "baseline",
        "spacing": "sm",
        "contents": [
            {
                "type": "text",
                "text": label.into(),
                "size": "sm",
                "color": "#6B7280",
                "flex": 2
            },
            {
                "type": "text",
                "text": value.into(),
                "size": "sm",
                "color": "#111827",
                "wrap": true,
                "flex": 5
            }
        ]
    })
}

fn uri_action(label: impl Into<String>, uri: impl Into<String>) -> Value {
    json!({
        "type": "button",
        "style": "primary",
        "height": "sm",
        "color": "#2563EB",
        "action": {
            "type": "uri",
            "label": label.into(),
            "uri": uri.into()
        }
    })
}

fn postback_action(label: impl Into<String>, data: impl Into<String>) -> Value {
    let data = data.into();
    json!({
        "type": "button",
        "style": "secondary",
        "height": "sm",
        "action": {
            "type": "postback",
            "label": label.into(),
            "data": data.clone(),
            "displayText": data
        }
    })
}

fn status_flex(
    header: &str,
    account_id: Option<&str>,
    status: &MonitorStatus,
    actions: Vec<Value>,
) -> SendMessage {
    let running = if status.is_running {
        "運行中"
    } else {
        "已暫停"
    };
    let last_poll = status
        .last_poll_timestamp
        .map(format_unix_time)
        .unwrap_or_else(|| "尚未輪詢".to_string());
    let last_success = status.last_success_course.as_deref().unwrap_or("無");
    let color = if status.is_running {
        "#16A34A"
    } else {
        "#6B7280"
    };

    let mut rows = vec![
        text_row("狀態", running),
        text_row("使用者", &status.user_name),
        text_row("最後輪詢", last_poll),
        text_row("最後成功", last_success),
        text_row("連續失敗", format!("{} 次", status.consecutive_failures)),
    ];

    if let Some(account_id) = account_id {
        rows.insert(1, text_row("帳號 ID", account_id));
    }

    flex_bubble(format!("{header}：{running}"), header, color, rows, actions)
}

fn help_flex(alt_text: &str, header: &str) -> SendMessage {
    flex_bubble(
        alt_text,
        header,
        "#2563EB",
        vec![
            text_row("/status", "查看目前監控狀態"),
            text_row("/start", "啟動簽到監控"),
            text_row("/stop", "暫停簽到監控"),
            text_row("/force", "立即觸發一次簽到檢查"),
            text_row("/reauth", "重新登入"),
            text_row("QR Code", "直接貼上掃描到的 URL 或 p 參數"),
        ],
        vec![
            postback_action("查看狀態", "/status"),
            postback_action("立即檢查", "/force"),
            postback_action("說明", "/help"),
        ],
    )
}

fn render_monitor_status(status: &MonitorStatus) -> String {
    let status_emoji = if status.is_running { "✅" } else { "⏸️" };
    let status_text = if status.is_running {
        "運行中"
    } else {
        "已暫停"
    };

    let last_poll = status
        .last_poll_timestamp
        .map(format_unix_time)
        .unwrap_or_else(|| "尚未輪詢".to_string());

    let last_success = status.last_success_course.as_deref().unwrap_or("無");

    format!(
        "📊 系統狀態\n\
         狀態：{status_emoji} {status_text}\n\
         帳號：{}\n\
         最後輪詢：{last_poll}\n\
         最後成功：{last_success}\n\
         連續失敗：{} 次",
        status.user_name, status.consecutive_failures,
    )
}

fn format_unix_time(timestamp: i64) -> String {
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S %Z").to_string(),
        None => format!("{timestamp}"),
    }
}

fn help_text() -> &'static str {
    "📚 可用指令：\n\
     \n\
     /status - 查看目前監控狀態\n\
     /start  - 啟動簽到監控\n\
     /stop   - 暫停簽到監控\n\
     /force  - 立即觸發一次簽到檢查\n\
     /reauth - 重新登錄（Session 過期時使用）\n\
     /help   - 顯示此說明\n\
     \n\
     💡 當有 QR Code 簽到時，\n\
     直接貼上掃描到的 URL 或 p 參數即可"
}

impl std::fmt::Debug for LineBotClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LineBotClient")
            .field("admin_user_id", &self.admin_user_id)
            .field("channel_access_token", &"<redacted>")
            .field("channel_secret", &"<redacted>")
            .finish()
    }
}

async fn handle_line_api_response(resp: reqwest::Response, api_name: &str) -> Result<()> {
    let status = resp.status();

    if status.is_success() {
        debug!(api = api_name, status = %status, "Line API 呼叫成功");
        return Ok(());
    }

    let body = resp.text().await.unwrap_or_default();

    if let Ok(err) = serde_json::from_str::<LineApiError>(&body) {
        Err(miette::miette!(
            "Line {} API 失敗（HTTP {}）：{}",
            api_name,
            status,
            err
        ))
    } else {
        Err(miette::miette!(
            "Line {} API 失敗（HTTP {}）：{}",
            api_name,
            status,
            &body[..body.len().min(200)]
        ))
    }
}

fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        let dummy = &vec![0u8; a.len()][..];
        let _ = constant_time_compare_inner(a, dummy);
        return false;
    }
    constant_time_compare_inner(a, b)
}

fn constant_time_compare_inner(a: &[u8], b: &[u8]) -> bool {
    debug_assert_eq!(a.len(), b.len());
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::events::{
        QrCodeRequest, RollcallEvent, RollcallResultEvent, StatusMessage, SystemStartedEvent,
    };

    fn make_test_bot_client() -> LineBotClient {
        LineBotClient {
            http: Client::new(),
            channel_access_token: "test_token".to_string(),
            channel_secret: "test_channel_secret".to_string(),
            admin_user_id: "Uadmin123".to_string(),
        }
    }

    fn flex_json(message: &SendMessage) -> (&str, &Value) {
        match message {
            SendMessage::Flex(flex) => (&flex.alt_text, &flex.contents),
            other => panic!("expected flex message, got {other:?}"),
        }
    }

    fn json_contains(value: &Value, needle: &str) -> bool {
        value.to_string().contains(needle)
    }

    #[test]
    fn test_verify_signature_correct() {
        let bot = make_test_bot_client();
        let body = b"Hello, Line!";

        let mut mac = Hmac::<Sha256>::new_from_slice(b"test_channel_secret").unwrap();
        mac.update(body);
        let sig_b64 = BASE64.encode(mac.finalize().into_bytes());

        assert!(bot.verify_signature(body, &sig_b64));
    }

    #[test]
    fn test_verify_signature_wrong_body() {
        let bot = make_test_bot_client();
        let body = b"Hello, Line!";

        let mut mac = Hmac::<Sha256>::new_from_slice(b"test_channel_secret").unwrap();
        mac.update(body);
        let sig_b64 = BASE64.encode(mac.finalize().into_bytes());

        assert!(!bot.verify_signature(b"Hello, World!", &sig_b64));
    }

    #[test]
    fn test_bot_client_debug_redacts_secrets() {
        let bot = make_test_bot_client();
        let debug_str = format!("{bot:?}");
        assert!(debug_str.contains("<redacted>"));
        assert!(debug_str.contains("Uadmin123"));
        assert!(!debug_str.contains("test_token"));
        assert!(!debug_str.contains("test_channel_secret"));
    }

    #[test]
    fn test_rollcall_detected_renders_flex() {
        let messages = render_line_messages(&OutboundMessage::RollcallDetected(RollcallEvent {
            rollcall_id: 42,
            account: "acc1".to_string(),
            course_name: "計算機網路".to_string(),
            teacher_name: "王老師".to_string(),
            attendance_type: "QR Code".to_string(),
        }));

        assert_eq!(messages.len(), 1);
        let (alt_text, contents) = flex_json(&messages[0]);
        assert_eq!(alt_text, "偵測到新簽到：計算機網路");
        assert!(json_contains(contents, "計算機網路"));
        assert!(json_contains(contents, "42"));
    }

    #[test]
    fn test_system_started_renders_flex() {
        let messages = render_line_messages(&OutboundMessage::SystemStarted(SystemStartedEvent {
            account: "acc1".to_string(),
            user_name: "王小明".to_string(),
            poll_interval_secs: 60,
            adapter_name: "line".to_string(),
        }));

        assert_eq!(messages.len(), 1);
        let (alt_text, contents) = flex_json(&messages[0]);
        assert_eq!(alt_text, "Tronclass Rollcall 已啟動");
        assert!(json_contains(contents, "王小明"));
        assert!(json_contains(contents, "60 秒"));
        assert!(json_contains(contents, "/status"));
        assert!(json_contains(contents, "/force"));
    }

    #[test]
    fn test_qrcode_request_renders_url_button() {
        let scan_url = "https://example.com/scanner?rollcall_id=42";
        let messages = render_line_messages(&OutboundMessage::QrCodeRequested(QrCodeRequest {
            rollcall_id: 42,
            account: "acc1".to_string(),
            course_name: "資料結構".to_string(),
            teacher_name: "李老師".to_string(),
            scan_url: scan_url.to_string(),
            timeout_secs: 120,
        }));

        let (alt_text, contents) = flex_json(&messages[0]);
        assert_eq!(alt_text, "QR Code 簽到：資料結構");
        assert!(json_contains(contents, "\"type\":\"uri\""));
        assert!(json_contains(contents, scan_url));
        assert!(json_contains(contents, "/status"));
    }

    #[test]
    fn test_rollcall_finished_uses_success_and_failure_colors() {
        let success =
            render_line_messages(&OutboundMessage::RollcallFinished(RollcallResultEvent {
                rollcall_id: 1,
                account: "acc1".to_string(),
                course_name: "英文".to_string(),
                attendance_type: "Number".to_string(),
                success: true,
                result: "成功".to_string(),
                elapsed_ms: 321,
            }));
        let failure =
            render_line_messages(&OutboundMessage::RollcallFinished(RollcallResultEvent {
                rollcall_id: 2,
                account: "acc1".to_string(),
                course_name: "英文".to_string(),
                attendance_type: "Number".to_string(),
                success: false,
                result: "失敗".to_string(),
                elapsed_ms: 654,
            }));

        let (success_alt, success_contents) = flex_json(&success[0]);
        let (failure_alt, failure_contents) = flex_json(&failure[0]);
        assert_eq!(success_alt, "簽到成功：英文");
        assert_eq!(failure_alt, "簽到未成功：英文");
        assert!(json_contains(success_contents, "#16A34A"));
        assert!(json_contains(failure_contents, "#DC2626"));
    }

    #[test]
    fn test_help_and_welcome_include_postback_actions() {
        for message in [OutboundMessage::Help, OutboundMessage::Welcome] {
            let messages = render_line_messages(&message);
            let (_alt_text, contents) = flex_json(&messages[0]);
            assert!(json_contains(contents, "\"type\":\"postback\""));
            assert!(json_contains(contents, "/status"));
            assert!(json_contains(contents, "/force"));
            assert!(json_contains(contents, "/help"));
        }
    }

    #[test]
    fn test_simple_message_still_renders_text() {
        let messages = render_line_messages(&OutboundMessage::QrAccepted);
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            SendMessage::Text(text) => assert!(text.text.contains("已收到 QR Code")),
            other => panic!("expected text message, got {other:?}"),
        }
    }

    #[test]
    fn test_single_status_renders_flex_with_actions() {
        let messages = render_line_messages(&OutboundMessage::Status(StatusMessage::Single(
            MonitorStatus {
                is_running: true,
                user_name: "王小明".to_string(),
                last_poll_timestamp: Some(1_700_000_000),
                last_success_course: Some("資料結構".to_string()),
                consecutive_failures: 0,
                started_at: 1_699_000_000,
            },
        )));

        let (alt_text, contents) = flex_json(&messages[0]);
        assert_eq!(alt_text, "系統狀態：運行中");
        assert!(json_contains(contents, "王小明"));
        assert!(json_contains(contents, "/force"));
        assert!(json_contains(contents, "/reauth"));
        assert!(json_contains(contents, "2023"));
        assert!(!json_contains(contents, "1700000000"));
        assert!(!json_contains(contents, "Unix"));
    }

    #[test]
    fn test_text_status_uses_human_readable_time() {
        let text = render_monitor_status(&MonitorStatus {
            is_running: true,
            user_name: "王小明".to_string(),
            last_poll_timestamp: Some(1_700_000_000),
            last_success_course: Some("資料結構".to_string()),
            consecutive_failures: 0,
            started_at: 1_699_000_000,
        });

        assert!(text.contains("2023"));
        assert!(!text.contains("1700000000"));
        assert!(!text.contains("Unix"));
    }
}
