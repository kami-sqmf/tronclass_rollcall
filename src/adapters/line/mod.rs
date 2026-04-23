//! Line Bot Webhook 伺服器模組
//!
//! 使用 `axum` 架設 HTTP 伺服器，接收 Line Platform 發送的 Webhook 事件。
//!
//! # 功能
//! - 驗證 `X-Line-Signature` Header（HMAC-SHA256）
//! - 解析 Webhook 事件
//! - 處理指令（`/status`、`/stop`、`/start`、`/force`、`/reauth`）
//! - 接收 QR code URL 並轉發到簽到邏輯
//! - 主動推送通知給管理員
//!
//! # 安全性
//! 所有 Webhook 請求必須通過 `X-Line-Signature` 驗證才會被處理。
//! 未通過驗證的請求會返回 HTTP 400。
//!
//! # 架構
//! ```
//! [Line Platform]
//!      │  POST /webhook
//!      ▼
//! [Axum Router]
//!      │  verify_signature middleware
//!      ▼
//! [webhook_handler]
//!      │  parse events
//!      ▼
//! [handle_event]
//!      ├── MessageEvent → parse BotCommand
//!      │       ├── QrCode(url) → qr_tx.send(url)
//!      │       ├── Status      → push status message
//!      │       ├── Stop/Start  → update monitor state
//!      │       └── ...
//!      └── other events → ignore
//! ```

pub mod types;

use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use hmac::{Hmac, Mac};
use miette::{IntoDiagnostic, Result, WrapErr};
use reqwest::Client;
use sha2::Sha256;
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, instrument, warn};

use crate::config::LineBotConfig;
use crate::rollcalls::QrCodeSender;

use self::types::{
    BotCommand, Event, LineMessage, MonitorStatus, PushMessageRequest, ReplyMessageRequest,
    SendMessage, WebhookPayload,
};

// ─── 常數 ─────────────────────────────────────────────────────────────────────

/// Line Reply API Endpoint
const LINE_REPLY_API: &str = "https://api.line.me/v2/bot/message/reply";

/// Line Push API Endpoint
const LINE_PUSH_API: &str = "https://api.line.me/v2/bot/message/push";

/// Line Webhook 簽名 Header 名稱
const SIGNATURE_HEADER: &str = "x-line-signature";

/// 請求 Body 最大大小（5 MB）
const MAX_BODY_SIZE: usize = 5 * 1024 * 1024;

// ─── Line Bot 客戶端 ──────────────────────────────────────────────────────────

/// Line Bot API 客戶端
///
/// 封裝 Line Messaging API 的發送功能，
/// 以及 Webhook Signature 驗證功能。
#[derive(Clone)]
pub struct LineBotClient {
    /// HTTP 客戶端
    http: Client,

    /// Channel Access Token
    channel_access_token: String,

    /// Channel Secret（用於簽名驗證）
    channel_secret: String,

    /// 管理員 Line User ID
    admin_user_id: String,
}

impl LineBotClient {
    /// 建立新的 Line Bot 客戶端
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

    /// 驗證 Line Webhook 簽名
    ///
    /// Line Platform 用 HMAC-SHA256 對 request body 計算簽名，
    /// 再以 Base64 編碼放入 `X-Line-Signature` Header。
    ///
    /// # 參數
    /// - `body`：原始 request body bytes
    /// - `signature`：`X-Line-Signature` Header 的值（Base64 字串）
    ///
    /// # 返回
    /// `true` 表示簽名合法，`false` 表示偽造或篡改
    pub fn verify_signature(&self, body: &[u8], signature: &str) -> bool {
        // Base64 decode 簽名
        let sig_bytes = match BASE64.decode(signature) {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(error = %e, "X-Line-Signature Base64 decode 失敗");
                return false;
            }
        };

        // 計算 HMAC-SHA256
        let mut mac = match Hmac::<Sha256>::new_from_slice(self.channel_secret.as_bytes()) {
            Ok(m) => m,
            Err(e) => {
                error!(error = %e, "HMAC 初始化失敗（channel_secret 可能無效）");
                return false;
            }
        };

        mac.update(body);
        let computed = mac.finalize().into_bytes();

        // 恆時比較（避免 timing attack）
        constant_time_compare(&computed, &sig_bytes)
    }

    /// 發送回覆訊息（Reply API）
    ///
    /// 使用 `reply_token` 回覆使用者，每個 token 只能使用一次。
    /// `reply_token` 在收到事件後 30 秒內有效。
    pub async fn reply_text(&self, reply_token: &str, text: &str) -> Result<()> {
        let req = ReplyMessageRequest::text(reply_token, text);

        debug!(
            reply_token = %&reply_token[..reply_token.len().min(10)],
            text_len = text.len(),
            "發送 Reply 訊息"
        );

        self.send_reply(req).await
    }

    /// 發送多則回覆訊息
    pub async fn reply_messages(
        &self,
        reply_token: &str,
        messages: Vec<SendMessage>,
    ) -> Result<()> {
        let req = ReplyMessageRequest::messages(reply_token, messages);
        self.send_reply(req).await
    }

    /// 內部：執行 Reply API 呼叫
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

    /// 主動推送訊息給指定使用者（Push API）
    ///
    /// 不需要 reply_token，可以在任何時間主動發送。
    /// 注意：Push API 可能需要付費方案。
    pub async fn push_message(&self, to: &str, text: &str) -> Result<()> {
        let req = PushMessageRequest::text(to, text);

        debug!(
            to = %&to[..to.len().min(10)],
            text_len = text.len(),
            "發送 Push 訊息"
        );

        self.send_push(req).await
    }

    /// 推送多則訊息
    pub async fn push_messages(&self, to: &str, messages: Vec<SendMessage>) -> Result<()> {
        let req = PushMessageRequest::messages(to, messages);
        self.send_push(req).await
    }

    /// 推送通知給管理員
    pub async fn push_message_to_admin(&self, text: &str) -> Result<()> {
        if self.admin_user_id.is_empty() {
            return Err(miette::miette!(
                "admin_user_id 未設定，無法推送通知給管理員"
            ));
        }
        self.push_message(&self.admin_user_id.clone(), text).await
    }

    /// 內部：執行 Push API 呼叫
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

    /// 取得管理員 User ID
    pub fn admin_user_id(&self) -> &str {
        &self.admin_user_id
    }
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

// ─── Axum 應用狀態 ────────────────────────────────────────────────────────────

/// 單一 Tronclass 帳號在 Line Webhook 中可操作的共享狀態
#[derive(Clone)]
pub struct WebhookAccountState {
    /// Tronclass Rollcall 內部帳號 ID
    pub account_id: String,

    /// 可查詢此帳號狀態的 Line User ID
    pub line_user_id: String,

    /// QR code 輸入通道（傳送給 rollcall 模組）
    pub qr_tx: QrCodeSender,

    /// 監控狀態（供 /status 指令查詢）
    pub monitor_status: Arc<Mutex<MonitorStatus>>,

    /// 監控是否正在運行（可被 /stop、/start 控制）
    pub is_running: Arc<Mutex<bool>>,

    /// 強制觸發一次輪詢的信號通道
    pub force_poll_tx: Arc<tokio::sync::Notify>,

    /// 重新認證請求信號通道
    pub reauth_tx: Arc<tokio::sync::Notify>,
}

impl WebhookAccountState {
    pub fn new(
        account_id: impl Into<String>,
        line_user_id: impl Into<String>,
        qr_tx: QrCodeSender,
        monitor_status: Arc<Mutex<MonitorStatus>>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            line_user_id: line_user_id.into(),
            qr_tx,
            monitor_status,
            is_running: Arc::new(Mutex::new(true)),
            force_poll_tx: Arc::new(tokio::sync::Notify::new()),
            reauth_tx: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

/// Webhook 伺服器的共享狀態
///
/// 管理員可操作全部帳號；一般 Line 使用者只能查詢與自己
/// `line_user_id` 綁定的帳號狀態。
#[derive(Clone)]
pub struct WebhookState {
    /// Line Bot 客戶端
    pub bot: Arc<LineBotClient>,

    /// 所有可透過 Line Bot 查詢/控制的帳號狀態
    pub accounts: Arc<Vec<WebhookAccountState>>,
}

impl WebhookState {
    pub fn new(bot: Arc<LineBotClient>, accounts: Vec<WebhookAccountState>) -> Self {
        Self {
            bot,
            accounts: Arc::new(accounts),
        }
    }

    fn first_account(&self) -> Option<&WebhookAccountState> {
        self.accounts.first()
    }

    fn account_for_line_user(&self, user_id: &str) -> Option<&WebhookAccountState> {
        if user_id.is_empty() {
            return None;
        }

        self.accounts
            .iter()
            .find(|account| !account.line_user_id.is_empty() && account.line_user_id == user_id)
    }

    fn is_admin_user(&self, user_id: &str) -> bool {
        !self.bot.admin_user_id.is_empty() && user_id == self.bot.admin_user_id
    }

    fn admin_is_unrestricted(&self) -> bool {
        self.bot.admin_user_id.is_empty()
    }
}

// ─── Axum Router 建立 ─────────────────────────────────────────────────────────

/// 建立 Axum Router
///
/// 路由表：
/// - `POST {webhook_path}` → `webhook_handler`
pub fn build_router(state: WebhookState, webhook_path: &str) -> Router {
    Router::new()
        .route(webhook_path, post(webhook_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// 啟動 Webhook 伺服器
///
/// 在背景 tokio task 中執行，綁定到 `0.0.0.0:{port}`。
/// 這個函式會阻塞直到伺服器關閉（收到 Ctrl+C 或呼叫 shutdown）。
pub async fn start_webhook_server(
    state: WebhookState,
    port: u16,
    webhook_path: &str,
) -> Result<()> {
    let app = build_router(state, webhook_path);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));

    info!(port = port, path = %webhook_path, "Line Bot Webhook 伺服器啟動：http://{}{}", addr, webhook_path);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to bind to port {port}"))?;

    axum::serve(listener, app)
        .await
        .into_diagnostic()
        .wrap_err("Webhook server error")
}

// ─── Webhook Handler ──────────────────────────────────────────────────────────

/// Webhook 主處理函式
///
/// 流程：
/// 1. 讀取原始 body bytes（用於簽名驗證）
/// 2. 驗證 `X-Line-Signature` Header
/// 3. 反序列化 JSON Payload
/// 4. 對每個事件呼叫 `handle_event`
///
/// 返回：
/// - `200 OK`：正常處理（包含事件處理錯誤的情況，Line 只需要 2xx）
/// - `400 Bad Request`：簽名驗證失敗
/// - `500 Internal Server Error`：嚴重內部錯誤
async fn webhook_handler(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // ── 1. 檢查 body 大小 ─────────────────────────────────────────────────────
    if body.len() > MAX_BODY_SIZE {
        warn!(size = body.len(), "Webhook body 過大，拒絕處理");
        return (StatusCode::PAYLOAD_TOO_LARGE, "Request body too large").into_response();
    }

    // ── 2. 驗證簽名 ───────────────────────────────────────────────────────────
    let signature = match headers.get(SIGNATURE_HEADER).and_then(|v| v.to_str().ok()) {
        Some(sig) => sig.to_string(),
        None => {
            warn!("缺少 X-Line-Signature Header");
            return (StatusCode::BAD_REQUEST, "Missing X-Line-Signature header").into_response();
        }
    };

    if !state.bot.verify_signature(&body, &signature) {
        warn!(signature = %&signature[..signature.len().min(20)], "X-Line-Signature 驗證失敗");
        return (StatusCode::BAD_REQUEST, "Invalid signature").into_response();
    }

    debug!("Webhook 簽名驗證通過");

    // ── 3. 反序列化 Payload ───────────────────────────────────────────────────
    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            error!(error = %e, "Webhook payload 反序列化失敗");
            // 仍然返回 200，避免 Line 重試
            return (StatusCode::OK, "").into_response();
        }
    };

    debug!(
        destination = %payload.destination,
        event_count = payload.events.len(),
        "收到 Webhook Payload"
    );

    // ── 4. 處理每個事件 ───────────────────────────────────────────────────────
    for event in &payload.events {
        if let Err(e) = handle_event(event, &state).await {
            // 事件處理錯誤不影響整體回應，記錄後繼續
            error!(error = %e, "處理 Webhook 事件失敗");
        }
    }

    // Line 只需要 2xx 回應，Body 內容無所謂
    (StatusCode::OK, "").into_response()
}

// ─── 事件處理 ─────────────────────────────────────────────────────────────────

/// 處理單個 Webhook 事件
#[instrument(skip(event, state), fields(event_type = ?std::mem::discriminant(event)))]
async fn handle_event(event: &Event, state: &WebhookState) -> Result<()> {
    match event {
        Event::Message(msg_event) => {
            if !msg_event.common.source.is_user() {
                debug!(
                    source = ?msg_event.common.source,
                    "忽略非個人對話來源的訊息事件"
                );
                return Ok(());
            }

            let user_id = msg_event.common.source.user_id().unwrap_or("");
            handle_message_event(msg_event, state, user_id).await?;
        }

        Event::Follow(follow_event) => {
            info!(
                user_id = ?follow_event.common.source.user_id(),
                "新使用者追蹤 Bot"
            );

            if let Some(reply_token) = follow_event.common.reply_token.as_deref() {
                let welcome = format!(
                    "👋 歡迎使用 Tronclass Rollcall！\n\n\
                     我會自動幫你完成 Tronclass 簽到。\n\n\
                     {}",
                    BotCommand::help_text()
                );
                let _ = state.bot.reply_text(reply_token, &welcome).await;
            }
        }

        Event::Unfollow(unfollow_event) => {
            info!(
                user_id = ?unfollow_event.common.source.user_id(),
                "使用者取消追蹤 Bot"
            );
        }

        Event::Join(_) => {
            info!("Bot 被加入群組或多人聊天室");
        }

        Event::Leave(_) => {
            info!("Bot 離開群組或多人聊天室");
        }

        Event::Postback(pb_event) => {
            if !pb_event.common.source.is_user() {
                debug!(
                    source = ?pb_event.common.source,
                    "忽略非個人對話來源的 Postback 事件"
                );
                return Ok(());
            }

            debug!(data = %pb_event.postback.data, "收到 Postback 事件");
            // 將 Postback data 當作指令處理
            let cmd = BotCommand::parse(&pb_event.postback.data);
            if let Some(reply_token) = pb_event.common.reply_token.as_deref() {
                let user_id = pb_event.common.source.user_id().unwrap_or("");
                execute_command(cmd, reply_token, state, user_id).await?;
            }
        }

        Event::Unknown => {
            debug!("收到未知類型的 Webhook 事件，忽略");
        }

        _ => {
            debug!("收到其他類型的 Webhook 事件，忽略");
        }
    }

    Ok(())
}

/// 處理訊息事件
async fn handle_message_event(
    event: &types::MessageEvent,
    state: &WebhookState,
    user_id: &str,
) -> Result<()> {
    let message = &event.message;
    let reply_token = event.common.reply_token.as_deref().unwrap_or("");

    match message {
        LineMessage::Text(text_msg) => {
            let text = &text_msg.text;
            debug!(text = %text, "收到文字訊息");

            let cmd = BotCommand::parse(text);
            info!(command = %cmd, "解析指令");

            execute_command(cmd, reply_token, state, user_id).await?;
        }

        LineMessage::Image(_) | LineMessage::Video(_) | LineMessage::Audio(_) => {
            debug!("收到媒體訊息，忽略");
            if !reply_token.is_empty() {
                let _ = state
                    .bot
                    .reply_text(reply_token, "⚠️ 不支援媒體訊息，請傳送文字指令。")
                    .await;
            }
        }

        LineMessage::Location(loc) => {
            // 使用者可以傳送位置用於雷達簽到（未來功能）
            debug!(lat = loc.latitude, lon = loc.longitude, "收到位置訊息");
            if !reply_token.is_empty() {
                let msg = format!(
                    "📍 收到位置訊息：\n緯度：{:.6}\n經度：{:.6}\n\n（位置功能尚未實作）",
                    loc.latitude, loc.longitude,
                );
                let _ = state.bot.reply_text(reply_token, &msg).await;
            }
        }

        LineMessage::Sticker(_) => {
            debug!("收到貼圖訊息，回覆目前狀態");
            reply_current_status(reply_token, state, user_id).await?;
        }

        _ => {
            debug!("收到其他類型訊息，忽略");
        }
    }

    Ok(())
}

// ─── 指令執行 ─────────────────────────────────────────────────────────────────

/// 執行解析出的 Bot 指令
#[instrument(skip(reply_token, state), fields(command = %cmd))]
async fn execute_command(
    cmd: BotCommand,
    reply_token: &str,
    state: &WebhookState,
    user_id: &str,
) -> Result<()> {
    let is_admin = state.admin_is_unrestricted() || state.is_admin_user(user_id);

    match cmd {
        // ── /status ───────────────────────────────────────────────────────────
        BotCommand::Status => {
            reply_current_status(reply_token, state, user_id).await?;
        }

        // ── /stop ─────────────────────────────────────────────────────────────
        BotCommand::Stop => {
            if !is_admin {
                reply_not_authorized(reply_token, state).await?;
                return Ok(());
            }

            set_all_running(state, false).await;

            info!("收到 /stop 指令，暫停監控");

            let msg = "⏸️ 簽到監控已暫停。\n輸入 /start 或 啟動 來恢復。";
            if !reply_token.is_empty() {
                state.bot.reply_text(reply_token, msg).await?;
            }
        }

        // ── /start ────────────────────────────────────────────────────────────
        BotCommand::Start => {
            if !is_admin {
                reply_not_authorized(reply_token, state).await?;
                return Ok(());
            }

            set_all_running(state, true).await;

            info!("收到 /start 指令，恢復監控");

            // 立即觸發一次輪詢
            notify_all_force_poll(state);

            let msg = "▶️ 簽到監控已恢復，立即執行一次簽到檢查...";
            if !reply_token.is_empty() {
                state.bot.reply_text(reply_token, msg).await?;
            }
        }

        // ── /force ────────────────────────────────────────────────────────────
        BotCommand::ForceAttend => {
            if !is_admin {
                reply_not_authorized(reply_token, state).await?;
                return Ok(());
            }

            info!("收到 /force 指令，強制觸發一次簽到檢查");

            // 通知監控循環立即執行一次
            notify_all_force_poll(state);

            let msg = "🔄 已觸發立即簽到檢查，請稍候...";
            if !reply_token.is_empty() {
                state.bot.reply_text(reply_token, msg).await?;
            }
        }

        // ── /reauth ───────────────────────────────────────────────────────────
        BotCommand::ReAuth => {
            if !is_admin {
                reply_not_authorized(reply_token, state).await?;
                return Ok(());
            }

            info!("收到 /reauth 指令，觸發重新認證");

            notify_all_reauth(state);

            let msg = "🔐 已觸發重新認證，請稍候...";
            if !reply_token.is_empty() {
                state.bot.reply_text(reply_token, msg).await?;
            }
        }

        // ── QR code ───────────────────────────────────────────────────────────
        BotCommand::QrCode(qr_data) => {
            info!(data_len = qr_data.len(), "收到 QR code 資料");

            let account = if is_admin {
                if state.accounts.len() == 1 {
                    state.first_account()
                } else {
                    None
                }
            } else {
                state.account_for_line_user(user_id)
            };

            let Some(account) = account else {
                let msg = if is_admin {
                    "⚠️ 多帳號模式下 QR Code 目標不明，請由綁定該帳號的 Line 使用者傳送 QR Code。"
                } else {
                    "⚠️ 找不到與你的 Line 帳號綁定的 Tronclass 帳號，無法傳送 QR Code。"
                };
                if !reply_token.is_empty() {
                    state.bot.reply_text(reply_token, msg).await?;
                }
                return Ok(());
            };

            // 傳送到簽到模組
            match account.qr_tx.send(qr_data.clone()).await {
                Ok(_) => {
                    debug!("QR code 資料已傳送到簽到模組");
                    let msg = "✅ 已收到 QR Code，正在嘗試簽到，請稍候...";
                    if !reply_token.is_empty() {
                        state.bot.reply_text(reply_token, msg).await?;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "QR code 傳送失敗（通道可能已關閉或沒有等待中的簽到）");
                    let msg = "⚠️ 無法傳送 QR Code：目前沒有等待 QR Code 的簽到，\
                               或簽到已逾時。";
                    if !reply_token.is_empty() {
                        state.bot.reply_text(reply_token, msg).await?;
                    }
                }
            }
        }

        // ── /help ─────────────────────────────────────────────────────────────
        BotCommand::Help => {
            if !reply_token.is_empty() {
                state
                    .bot
                    .reply_text(reply_token, BotCommand::help_text())
                    .await?;
            }
        }

        // ── 未知指令 ──────────────────────────────────────────────────────────
        BotCommand::Unknown(text) => {
            debug!(text = %text, "收到未知指令或純文字");

            // 不回覆未知訊息，避免干擾（可按需求修改）
            // 也可以回覆 Help
            if !reply_token.is_empty() {
                let msg = format!(
                    "❓ 不認識的指令：{}\n\n輸入 /help 或 幫助 查看可用指令。",
                    &text[..text.len().min(50)]
                );
                state.bot.reply_text(reply_token, &msg).await?;
            }
        }
    }

    Ok(())
}

// ─── 輔助函式 ─────────────────────────────────────────────────────────────────

async fn reply_current_status(
    reply_token: &str,
    state: &WebhookState,
    user_id: &str,
) -> Result<()> {
    if reply_token.is_empty() {
        return Ok(());
    }

    let is_admin = state.admin_is_unrestricted() || state.is_admin_user(user_id);
    let msg = status_message_for_user(state, user_id, is_admin).await;
    state.bot.reply_text(reply_token, &msg).await?;
    Ok(())
}

async fn status_message_for_user(state: &WebhookState, user_id: &str, is_admin: bool) -> String {
    if is_admin {
        return admin_status_message(state).await;
    }

    match state.account_for_line_user(user_id) {
        Some(account) => {
            let status = account.monitor_status.lock().await;
            format!(
                "📊 你的 Tronclass 帳號狀態\n帳號 ID：{}\n{}",
                account.account_id,
                status.to_line_message()
            )
        }
        None => "⚠️ 找不到與你的 Line 帳號綁定的 Tronclass 帳號。請確認帳號資料中的 line_user_id 是否正確。"
            .to_string(),
    }
}

async fn admin_status_message(state: &WebhookState) -> String {
    if state.accounts.is_empty() {
        return "📊 系統狀態\n目前沒有可查詢的 Tronclass 帳號。".to_string();
    }

    if state.accounts.len() == 1 {
        let account = &state.accounts[0];
        let status = account.monitor_status.lock().await;
        return status.to_line_message();
    }

    let mut lines = vec![format!("📊 系統狀態（{} 個帳號）", state.accounts.len())];
    for account in state.accounts.iter() {
        let status = account.monitor_status.lock().await;
        let running = if status.is_running {
            "運行中"
        } else {
            "已暫停"
        };
        let last_poll = status
            .last_poll_timestamp
            .map(|ts| format!("{ts}"))
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

async fn set_all_running(state: &WebhookState, is_running: bool) {
    for account in state.accounts.iter() {
        let mut running = account.is_running.lock().await;
        *running = is_running;

        let mut status = account.monitor_status.lock().await;
        status.is_running = is_running;
    }
}

fn notify_all_force_poll(state: &WebhookState) {
    for account in state.accounts.iter() {
        account.force_poll_tx.notify_one();
    }
}

fn notify_all_reauth(state: &WebhookState) {
    for account in state.accounts.iter() {
        account.reauth_tx.notify_one();
    }
}

async fn reply_not_authorized(reply_token: &str, state: &WebhookState) -> Result<()> {
    if !reply_token.is_empty() {
        state
            .bot
            .reply_text(
                reply_token,
                "⚠️ 你可以查詢自己的帳號狀態，但此操作僅限管理員使用。",
            )
            .await?;
    }
    Ok(())
}

/// 處理 Line API 回應
///
/// - 2xx：成功，返回 Ok(())
/// - 4xx/5xx：解析錯誤訊息並返回 Err
async fn handle_line_api_response(resp: reqwest::Response, api_name: &str) -> Result<()> {
    let status = resp.status();

    if status.is_success() {
        debug!(api = api_name, status = %status, "Line API 呼叫成功");
        return Ok(());
    }

    // 嘗試解析 Line API 錯誤回應
    let body = resp.text().await.unwrap_or_default();

    if let Ok(err) = serde_json::from_str::<types::LineApiError>(&body) {
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

/// 恆時比較兩個 byte slice（防止 timing attack）
///
/// 無論 slice 內容如何，執行時間都相同。
/// 若長度不同，仍然執行完整比較後再返回 false。
fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // 為了恆時性，仍然跑一遍比較
        let dummy = &vec![0u8; a.len()][..];
        let _ = constant_time_compare_inner(a, dummy);
        return false;
    }
    constant_time_compare_inner(a, b)
}

/// 恆時比較相同長度的 byte slice
///
/// 所有 byte 都會被比較，不會提前返回。
fn constant_time_compare_inner(a: &[u8], b: &[u8]) -> bool {
    debug_assert_eq!(a.len(), b.len());
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── constant_time_compare ─────────────────────────────────────────────────

    #[test]
    fn test_constant_time_compare_equal() {
        let a = b"hello world";
        let b = b"hello world";
        assert!(constant_time_compare(a, b));
    }

    #[test]
    fn test_constant_time_compare_different() {
        let a = b"hello world";
        let b = b"hello WORLD";
        assert!(!constant_time_compare(a, b));
    }

    #[test]
    fn test_constant_time_compare_different_length() {
        let a = b"hello";
        let b = b"hello world";
        assert!(!constant_time_compare(a, b));
    }

    #[test]
    fn test_constant_time_compare_empty() {
        assert!(constant_time_compare(b"", b""));
    }

    #[test]
    fn test_constant_time_compare_single_byte_diff() {
        let a = [0u8; 32];
        let mut b = [0u8; 32];
        b[31] = 1; // 只有最後一個 byte 不同
        assert!(!constant_time_compare(&a, &b));
    }

    #[test]
    fn test_constant_time_compare_single_byte_same() {
        let a = [0xABu8; 32];
        let b = [0xABu8; 32];
        assert!(constant_time_compare(&a, &b));
    }

    // ── verify_signature ──────────────────────────────────────────────────────

    fn make_test_bot_client() -> LineBotClient {
        LineBotClient {
            http: Client::new(),
            channel_access_token: "test_token".to_string(),
            channel_secret: "test_channel_secret".to_string(),
            admin_user_id: "Uadmin123".to_string(),
        }
    }

    fn make_test_status(user_name: &str, is_running: bool) -> Arc<Mutex<MonitorStatus>> {
        Arc::new(Mutex::new(MonitorStatus {
            is_running,
            user_name: user_name.to_string(),
            last_poll_timestamp: None,
            last_success_course: None,
            consecutive_failures: 0,
            started_at: 0,
        }))
    }

    fn make_test_account(
        account_id: &str,
        line_user_id: &str,
        qr_tx: QrCodeSender,
        status: Arc<Mutex<MonitorStatus>>,
    ) -> WebhookAccountState {
        WebhookAccountState::new(account_id, line_user_id, qr_tx, status)
    }

    fn make_test_event_common(source: types::EventSource) -> types::EventCommon {
        types::EventCommon {
            webhook_event_id: "test-event".to_string(),
            reply_token: None,
            timestamp: 0,
            source,
            delivery_context: types::DeliveryContext::default(),
        }
    }

    #[test]
    fn test_verify_signature_correct() {
        let bot = make_test_bot_client();
        let body = b"Hello, Line!";

        // 計算正確簽名
        let mut mac = Hmac::<Sha256>::new_from_slice(b"test_channel_secret").unwrap();
        mac.update(body);
        let sig_bytes = mac.finalize().into_bytes();
        let sig_b64 = BASE64.encode(sig_bytes);

        assert!(bot.verify_signature(body, &sig_b64), "正確簽名應通過驗證");
    }

    #[test]
    fn test_verify_signature_wrong_body() {
        let bot = make_test_bot_client();
        let body = b"Hello, Line!";
        let wrong_body = b"Hello, World!";

        // 用正確 body 計算簽名，但用錯誤 body 驗證
        let mut mac = Hmac::<Sha256>::new_from_slice(b"test_channel_secret").unwrap();
        mac.update(body);
        let sig_bytes = mac.finalize().into_bytes();
        let sig_b64 = BASE64.encode(sig_bytes);

        assert!(
            !bot.verify_signature(wrong_body, &sig_b64),
            "body 不同應驗證失敗"
        );
    }

    #[test]
    fn test_verify_signature_wrong_secret() {
        let body = b"test body content";

        // 用錯誤的 secret 計算簽名
        let mut mac = Hmac::<Sha256>::new_from_slice(b"wrong_secret").unwrap();
        mac.update(body);
        let sig_bytes = mac.finalize().into_bytes();
        let sig_b64 = BASE64.encode(sig_bytes);

        let bot = make_test_bot_client(); // 使用 "test_channel_secret"
        assert!(
            !bot.verify_signature(body, &sig_b64),
            "secret 不同應驗證失敗"
        );
    }

    #[test]
    fn test_verify_signature_invalid_base64() {
        let bot = make_test_bot_client();
        let body = b"some body";
        let invalid_sig = "not-valid-base64!!!";

        assert!(
            !bot.verify_signature(body, invalid_sig),
            "無效 Base64 應驗證失敗"
        );
    }

    #[test]
    fn test_verify_signature_empty_body() {
        let bot = make_test_bot_client();
        let body = b"";

        let mut mac = Hmac::<Sha256>::new_from_slice(b"test_channel_secret").unwrap();
        mac.update(body);
        let sig_bytes = mac.finalize().into_bytes();
        let sig_b64 = BASE64.encode(sig_bytes);

        assert!(
            bot.verify_signature(body, &sig_b64),
            "空 body 也應可以正確驗證"
        );
    }

    #[test]
    fn test_verify_signature_tampered_signature() {
        let bot = make_test_bot_client();
        let body = b"original body";

        // 計算正確簽名
        let mut mac = Hmac::<Sha256>::new_from_slice(b"test_channel_secret").unwrap();
        mac.update(body);
        let sig_bytes = mac.finalize().into_bytes();
        let sig_b64 = BASE64.encode(sig_bytes);

        // 篡改簽名：修改第一個字元
        let mut tampered = sig_b64.into_bytes();
        tampered[0] ^= 0xFF;
        let tampered_str = String::from_utf8_lossy(&tampered).to_string();

        // 篡改後的簽名可能是無效 Base64，也可能驗證失敗
        // 無論哪種情況，結果都應該是 false
        assert!(
            !bot.verify_signature(body, &tampered_str),
            "篡改的簽名應驗證失敗"
        );
    }

    #[test]
    fn test_verify_signature_real_webhook_example() {
        // 模擬真實的 Line Webhook 驗證場景
        let channel_secret = "abcdef0123456789";
        let body = r#"{"destination":"Uxxxxxxxxxx","events":[]}"#.as_bytes();

        let bot = LineBotClient {
            http: Client::new(),
            channel_access_token: "token".to_string(),
            channel_secret: channel_secret.to_string(),
            admin_user_id: "U123".to_string(),
        };

        // 計算期望的簽名
        let mut mac = Hmac::<Sha256>::new_from_slice(channel_secret.as_bytes()).unwrap();
        mac.update(body);
        let expected_sig = BASE64.encode(mac.finalize().into_bytes());

        assert!(bot.verify_signature(body, &expected_sig));

        // 確認不同的 body 驗證失敗
        assert!(!bot.verify_signature(b"different body", &expected_sig));
    }

    // ── LineBotClient debug ───────────────────────────────────────────────────

    #[test]
    fn test_bot_client_debug_redacts_secrets() {
        let bot = make_test_bot_client();
        let debug_str = format!("{bot:?}");
        // 敏感資訊應被遮罩
        assert!(debug_str.contains("<redacted>"));
        // admin_user_id 應顯示
        assert!(debug_str.contains("Uadmin123"));
        // token 和 secret 不應洩漏
        assert!(!debug_str.contains("test_token"));
        assert!(!debug_str.contains("test_channel_secret"));
    }

    // ── WebhookState ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_webhook_state_initial_is_running() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("test", true);
        let account = make_test_account("acc1", "Uuser1", qr_tx, status);

        let state = WebhookState::new(bot, vec![account]);
        let is_running = state.accounts[0].is_running.lock().await;
        assert!(*is_running, "初始狀態應為運行中");
    }

    #[tokio::test]
    async fn test_webhook_state_toggle_running() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("test", true);
        let account = make_test_account("acc1", "Uuser1", qr_tx, status);

        let state = WebhookState::new(bot, vec![account]);

        // 暫停
        *state.accounts[0].is_running.lock().await = false;
        assert!(!*state.accounts[0].is_running.lock().await);

        // 恢復
        *state.accounts[0].is_running.lock().await = true;
        assert!(*state.accounts[0].is_running.lock().await);
    }

    // ── build_router ──────────────────────────────────────────────────────────

    #[test]
    fn test_build_router_does_not_panic() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("test", true);
        let account = make_test_account("acc1", "Uuser1", qr_tx, status);
        let state = WebhookState::new(bot, vec![account]);
        // 確認 build_router 不會 panic
        let _router = build_router(state, "/webhook");
    }

    // ── BotCommand 執行邏輯（整合測試） ───────────────────────────────────────

    #[tokio::test]
    async fn test_qrcode_command_sends_to_channel() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, mut qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("test", true);
        let account = make_test_account("acc1", "Uuser1", qr_tx, status);
        let state = WebhookState::new(bot, vec![account]);

        // 發送 QrCode 指令
        let cmd = BotCommand::QrCode("0~100!3~secret_data!4~42".to_string());
        // 不帶 reply_token（空字串），跳過 reply API 呼叫
        let _ = execute_command(cmd, "", &state, "Uadmin123").await;

        // 確認資料已傳送到通道
        let received = qr_rx.recv().await;
        assert_eq!(received, Some("0~100!3~secret_data!4~42".to_string()));
    }

    #[tokio::test]
    async fn test_force_poll_command_notifies() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("test", true);
        let account = make_test_account("acc1", "Uuser1", qr_tx, status);
        let notify = Arc::clone(&account.force_poll_tx);
        let state = WebhookState::new(bot, vec![account]);

        // 在背景等待通知
        let handle = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_millis(500), notify.notified())
                .await
                .is_ok()
        });

        // 執行 ForceAttend 指令
        let _ = execute_command(BotCommand::ForceAttend, "", &state, "Uadmin123").await;

        let was_notified = handle.await.unwrap();
        assert!(was_notified, "ForceAttend 應觸發 notify");
    }

    #[tokio::test]
    async fn test_stop_command_sets_not_running() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("test", true);
        let account = make_test_account("acc1", "Uuser1", qr_tx, status);
        let state = WebhookState::new(bot, vec![account]);

        // 執行 Stop 指令
        let _ = execute_command(BotCommand::Stop, "", &state, "Uadmin123").await;
        assert!(
            !*state.accounts[0].is_running.lock().await,
            "Stop 後應為暫停狀態"
        );
    }

    #[tokio::test]
    async fn test_start_command_sets_running() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("test", false);
        let account = make_test_account("acc1", "Uuser1", qr_tx, status);
        let state = WebhookState::new(bot, vec![account]);

        // 先停止
        *state.accounts[0].is_running.lock().await = false;

        // 執行 Start 指令
        let _ = execute_command(BotCommand::Start, "", &state, "Uadmin123").await;
        assert!(
            *state.accounts[0].is_running.lock().await,
            "Start 後應為運行狀態"
        );
    }

    #[tokio::test]
    async fn test_non_admin_can_read_own_account_status() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx_a, _qr_rx_a) = tokio::sync::mpsc::channel(10);
        let (qr_tx_b, _qr_rx_b) = tokio::sync::mpsc::channel(10);
        let status_a = make_test_status("Alice (acc-a)", true);
        let status_b = make_test_status("Bob (acc-b)", true);
        let account_a = make_test_account("acc-a", "Ualice", qr_tx_a, status_a);
        let account_b = make_test_account("acc-b", "Ubob", qr_tx_b, status_b);
        let state = WebhookState::new(bot, vec![account_a, account_b]);

        let msg = status_message_for_user(&state, "Ualice", false).await;

        assert!(msg.contains("acc-a"), "got: {msg}");
        assert!(msg.contains("Alice"), "got: {msg}");
        assert!(!msg.contains("acc-b"), "got: {msg}");
        assert!(!msg.contains("Bob"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_non_admin_without_bound_account_gets_not_found_message() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("Alice (acc-a)", true);
        let account = make_test_account("acc-a", "Ualice", qr_tx, status);
        let state = WebhookState::new(bot, vec![account]);

        let msg = status_message_for_user(&state, "Uunknown", false).await;

        assert!(msg.contains("找不到"), "got: {msg}");
        assert!(!msg.contains("Alice"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_admin_status_includes_all_accounts() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx_a, _qr_rx_a) = tokio::sync::mpsc::channel(10);
        let (qr_tx_b, _qr_rx_b) = tokio::sync::mpsc::channel(10);
        let status_a = make_test_status("Alice (acc-a)", true);
        let status_b = make_test_status("Bob (acc-b)", false);
        let account_a = make_test_account("acc-a", "Ualice", qr_tx_a, status_a);
        let account_b = make_test_account("acc-b", "Ubob", qr_tx_b, status_b);
        let state = WebhookState::new(bot, vec![account_a, account_b]);

        let msg = status_message_for_user(&state, "Uadmin123", true).await;

        assert!(msg.contains("2 個帳號"), "got: {msg}");
        assert!(msg.contains("acc-a"), "got: {msg}");
        assert!(msg.contains("acc-b"), "got: {msg}");
        assert!(msg.contains("Bob"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_sticker_message_is_status_trigger_for_user_source() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("Alice (acc-a)", true);
        let account = make_test_account("acc-a", "Ualice", qr_tx, status);
        let state = WebhookState::new(bot, vec![account]);
        let event = types::MessageEvent {
            common: make_test_event_common(types::EventSource::User {
                user_id: "Ualice".to_string(),
            }),
            message: LineMessage::Sticker(types::StickerMessage {
                id: "sticker-1".to_string(),
                package_id: "1".to_string(),
                sticker_id: "1".to_string(),
                sticker_resource_type: None,
            }),
        };

        handle_message_event(&event, &state, "Ualice")
            .await
            .unwrap();

        let msg = status_message_for_user(&state, "Ualice", false).await;
        assert!(msg.contains("acc-a"), "got: {msg}");
        assert!(msg.contains("Alice"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_non_user_message_event_is_ignored() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("Alice (acc-a)", true);
        let account = make_test_account("acc-a", "Ualice", qr_tx, status);
        let state = WebhookState::new(bot, vec![account]);
        let event = Event::Message(types::MessageEvent {
            common: make_test_event_common(types::EventSource::Group {
                group_id: "Ggroup".to_string(),
                user_id: Some("Uadmin123".to_string()),
            }),
            message: LineMessage::Text(types::TextMessage {
                id: "text-1".to_string(),
                text: "/stop".to_string(),
                emojis: vec![],
                mention: None,
                quoted_message_id: None,
            }),
        });

        handle_event(&event, &state).await.unwrap();

        assert!(
            *state.accounts[0].is_running.lock().await,
            "群組來源即使帶有管理員 user_id，也不應執行指令"
        );
    }

    #[tokio::test]
    async fn test_non_user_postback_event_is_ignored() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("Alice (acc-a)", true);
        let account = make_test_account("acc-a", "Ualice", qr_tx, status);
        let state = WebhookState::new(bot, vec![account]);
        let event = Event::Postback(types::PostbackEvent {
            common: make_test_event_common(types::EventSource::Room {
                room_id: "Rroom".to_string(),
                user_id: Some("Uadmin123".to_string()),
            }),
            postback: types::PostbackData {
                data: "/stop".to_string(),
                params: None,
            },
        });

        handle_event(&event, &state).await.unwrap();

        assert!(
            *state.accounts[0].is_running.lock().await,
            "多人聊天室來源即使帶有管理員 user_id，也不應執行 postback 指令"
        );
    }

    #[tokio::test]
    async fn test_non_admin_control_command_is_ignored() {
        let bot = Arc::new(make_test_bot_client());
        let (qr_tx, _qr_rx) = tokio::sync::mpsc::channel(10);
        let status = make_test_status("Alice (acc-a)", true);
        let account = make_test_account("acc-a", "Ualice", qr_tx, status);
        let state = WebhookState::new(bot, vec![account]);

        let _ = execute_command(BotCommand::Stop, "", &state, "Ualice").await;

        assert!(
            *state.accounts[0].is_running.lock().await,
            "非管理員不應能暫停監控"
        );
    }

    // ── constant_time_compare_inner ───────────────────────────────────────────

    #[test]
    fn test_constant_time_compare_inner_equal() {
        let a = [1u8, 2, 3, 4];
        let b = [1u8, 2, 3, 4];
        assert!(constant_time_compare_inner(&a, &b));
    }

    #[test]
    fn test_constant_time_compare_inner_different() {
        let a = [1u8, 2, 3, 4];
        let b = [1u8, 2, 3, 5];
        assert!(!constant_time_compare_inner(&a, &b));
    }

    #[test]
    fn test_constant_time_compare_inner_all_zero_equal() {
        let a = [0u8; 32];
        let b = [0u8; 32];
        assert!(constant_time_compare_inner(&a, &b));
    }

    #[test]
    fn test_constant_time_compare_inner_all_zero_vs_one() {
        let a = [0u8; 32];
        let mut b = [0u8; 32];
        b[0] = 1;
        assert!(!constant_time_compare_inner(&a, &b));
    }
}
