//! LINE webhook HTTP entry point and LINE-to-core event conversion.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use miette::{IntoDiagnostic, Result, WrapErr};
use serde_json::json;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};

use crate::adapters::requests::{self, AdapterRequest, RequestContent, RequestState};
use crate::adapters::scanner::{QrScannerRegistry, ScannerSubmission};

use super::client::LineBotClient;
use super::types::{Event, LineMessage, WebhookPayload};

const SIGNATURE_HEADER: &str = "x-line-signature";
const MAX_BODY_SIZE: usize = 5 * 1024 * 1024;

#[derive(Clone)]
pub struct LineWebhookState {
    pub bot: Arc<LineBotClient>,
    pub requests: RequestState,
    pub scanner: Option<Arc<QrScannerRegistry>>,
}

impl LineWebhookState {
    pub fn new(
        bot: Arc<LineBotClient>,
        requests: RequestState,
        scanner: Option<Arc<QrScannerRegistry>>,
    ) -> Self {
        Self {
            bot,
            requests,
            scanner,
        }
    }
}

pub fn build_router(state: LineWebhookState, webhook_path: &str) -> Router {
    Router::new()
        .route(webhook_path, post(webhook_handler))
        .route("/scanner", get(scanner_page_handler))
        .route("/scanner/submit", post(scanner_submit_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn start_webhook_server(
    state: LineWebhookState,
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

async fn webhook_handler(
    State(state): State<LineWebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > MAX_BODY_SIZE {
        warn!(size = body.len(), "Webhook body 過大，拒絕處理");
        return (StatusCode::PAYLOAD_TOO_LARGE, "Request body too large").into_response();
    }

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

    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            error!(error = %e, "Webhook payload 反序列化失敗");
            return (StatusCode::OK, "").into_response();
        }
    };

    debug!(
        destination = %payload.destination,
        event_count = payload.events.len(),
        "收到 Webhook Payload"
    );

    for event in &payload.events {
        match adapter_request_from_line(event) {
            Some(request) => {
                if let Err(e) = requests::handle_request(request, &state.requests).await {
                    error!(error = %e, "處理 adapter request 失敗");
                }
            }
            None => debug!("LINE 事件無法轉換或不需處理，忽略"),
        }
    }

    (StatusCode::OK, "").into_response()
}

async fn scanner_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("scanner.html")),
    )
        .into_response()
}

async fn scanner_submit_handler(
    State(state): State<LineWebhookState>,
    Json(submission): Json<ScannerSubmission>,
) -> Response {
    let Some(scanner) = state.scanner.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "scanner is not enabled" })),
        )
            .into_response();
    };

    match scanner.submit(submission).await {
        Ok(result) => (StatusCode::OK, Json(json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub fn adapter_request_from_line(event: &Event) -> Option<AdapterRequest> {
    match event {
        Event::Message(msg_event) => {
            let user_id = msg_event.common.source.user_id().unwrap_or("").to_string();
            let reply_token = msg_event.common.reply_token.clone();
            let is_direct_user = msg_event.common.source.is_user();
            let content = match &msg_event.message {
                LineMessage::Text(text) => RequestContent::Text(text.text.clone()),
                LineMessage::Image(_) | LineMessage::Video(_) | LineMessage::Audio(_) => {
                    RequestContent::Media
                }
                LineMessage::Location(loc) => RequestContent::Location {
                    latitude: loc.latitude,
                    longitude: loc.longitude,
                },
                LineMessage::Sticker(_) => RequestContent::Sticker,
                _ => RequestContent::Unknown,
            };

            Some(AdapterRequest::new(
                user_id,
                reply_token,
                is_direct_user,
                content,
            ))
        }
        Event::Follow(follow_event) => Some(AdapterRequest::new(
            follow_event
                .common
                .source
                .user_id()
                .unwrap_or("")
                .to_string(),
            follow_event.common.reply_token.clone(),
            follow_event.common.source.is_user(),
            RequestContent::Follow,
        )),
        Event::Unfollow(unfollow_event) => Some(AdapterRequest::new(
            unfollow_event
                .common
                .source
                .user_id()
                .unwrap_or("")
                .to_string(),
            unfollow_event.common.reply_token.clone(),
            unfollow_event.common.source.is_user(),
            RequestContent::Unfollow,
        )),
        Event::Join(join_event) => Some(AdapterRequest::new(
            join_event.common.source.user_id().unwrap_or("").to_string(),
            join_event.common.reply_token.clone(),
            join_event.common.source.is_user(),
            RequestContent::Join,
        )),
        Event::Leave(leave_event) => Some(AdapterRequest::new(
            leave_event
                .common
                .source
                .user_id()
                .unwrap_or("")
                .to_string(),
            leave_event.common.reply_token.clone(),
            leave_event.common.source.is_user(),
            RequestContent::Leave,
        )),
        Event::Postback(pb_event) => Some(AdapterRequest::new(
            pb_event.common.source.user_id().unwrap_or("").to_string(),
            pb_event.common.reply_token.clone(),
            pb_event.common.source.is_user(),
            RequestContent::Text(pb_event.postback.data.clone()),
        )),
        Event::Unknown => Some(AdapterRequest::new(
            "",
            None,
            false,
            RequestContent::Unknown,
        )),
        _ => Some(AdapterRequest::new(
            "",
            None,
            false,
            RequestContent::Unknown,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Method, Request};
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    use crate::adapters::line::types;
    use crate::adapters::scanner::QrScannerRegistry;
    use crate::config::LineBotConfig;

    fn test_bot() -> Arc<LineBotClient> {
        Arc::new(
            LineBotClient::new(&LineBotConfig {
                enabled: true,
                channel_secret: "secret".to_string(),
                channel_access_token: "token".to_string(),
                webhook_port: 8080,
                webhook_path: "/webhook".to_string(),
                public_base_url: "https://scanner.example.test".to_string(),
                admin_user_id: "Uadmin123".to_string(),
            })
            .unwrap(),
        )
    }

    fn test_state(scanner: Option<Arc<QrScannerRegistry>>) -> LineWebhookState {
        let bot = test_bot();
        let messenger = Arc::clone(&bot) as Arc<dyn crate::adapters::events::AdapterMessenger>;
        let requests = RequestState::new(messenger, vec![]);
        LineWebhookState::new(bot, requests, scanner)
    }

    fn make_event_common(source: types::EventSource) -> types::EventCommon {
        types::EventCommon {
            webhook_event_id: "test-event".to_string(),
            reply_token: None,
            timestamp: 0,
            source,
            delivery_context: types::DeliveryContext::default(),
        }
    }

    #[test]
    fn test_non_user_message_event_maps_as_not_direct() {
        let event = Event::Message(types::MessageEvent {
            common: make_event_common(types::EventSource::Group {
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

        let request = adapter_request_from_line(&event).unwrap();
        assert!(!request.is_direct_user);
        assert_eq!(request.user_id, "Uadmin123");
    }

    #[tokio::test]
    async fn scanner_route_serves_html() {
        let app = build_router(
            test_state(Some(Arc::new(QrScannerRegistry::new(
                "https://scanner.example.test",
            )))),
            "/webhook",
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/scanner")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("Tronclass QR Scanner"));
        assert!(html.contains("jsQR"));
    }

    #[tokio::test]
    async fn scanner_submit_route_delivers_qr_data() {
        let scanner = Arc::new(QrScannerRegistry::new("https://scanner.example.test"));
        let (tx, mut rx) = mpsc::channel(1);
        let link = scanner
            .register_pending("fju", 42, "acc1", tx, std::time::Duration::from_secs(60))
            .await
            .unwrap();
        let app = build_router(test_state(Some(scanner)), "/webhook");
        let body = serde_json::json!({
            "provider": "fju",
            "rollcall_id": 42,
            "account_id": "acc1",
            "token": link.token,
            "qr_data": "0~100!3~secret!4~42"
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/scanner/submit")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(rx.recv().await.unwrap(), "0~100!3~secret!4~42");
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["delivered_count"], 1);
    }

    #[tokio::test]
    async fn scanner_submit_route_returns_error_for_invalid_submit() {
        let app = build_router(
            test_state(Some(Arc::new(QrScannerRegistry::new(
                "https://scanner.example.test",
            )))),
            "/webhook",
        );
        let body = serde_json::json!({
            "provider": "fju",
            "rollcall_id": 42,
            "account_id": "acc1",
            "token": "missing",
            "qr_data": "0~100!3~secret!4~42"
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/scanner/submit")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("等待中"));
    }
}
