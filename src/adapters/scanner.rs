//! Local QR scanner state and submission routing.
//!
//! The scanner page submits one QR payload back to this process. This registry
//! fans that payload out to every account currently waiting for the same
//! provider + rollcall_id.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};
use url::Url;
use uuid::Uuid;

use crate::adapters::requests::QrCodeSender;

const DEFAULT_SCANNER_PATH: &str = "/scanner";
const MAX_QR_DATA_LEN: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PendingQrKey {
    provider: String,
    rollcall_id: u64,
}

impl PendingQrKey {
    fn new(provider: impl Into<String>, rollcall_id: u64) -> Self {
        Self {
            provider: provider.into(),
            rollcall_id,
        }
    }
}

#[derive(Clone)]
struct PendingAccount {
    tx: QrCodeSender,
}

struct PendingQrEntry {
    token: String,
    expires_at: Instant,
    used: bool,
    submitted_qr_data: Option<String>,
    accounts: HashMap<String, PendingAccount>,
}

/// Shared registry for scanner links and pending QR waits.
#[derive(Clone)]
pub struct QrScannerRegistry {
    public_base_url: String,
    scanner_path: String,
    inner: Arc<Mutex<HashMap<PendingQrKey, PendingQrEntry>>>,
}

#[derive(Clone)]
pub struct ScannerHttpState {
    scanner: Arc<QrScannerRegistry>,
}

impl ScannerHttpState {
    pub fn new(scanner: Arc<QrScannerRegistry>) -> Self {
        Self { scanner }
    }
}

impl QrScannerRegistry {
    pub fn new(public_base_url: impl Into<String>) -> Self {
        Self {
            public_base_url: public_base_url.into(),
            scanner_path: DEFAULT_SCANNER_PATH.to_string(),
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn public_base_url(&self) -> &str {
        &self.public_base_url
    }

    /// Register one account as waiting for a shared QR scan.
    pub async fn register_pending(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        tx: QrCodeSender,
        timeout: Duration,
    ) -> Result<ScannerLink, ScannerError> {
        let now = Instant::now();
        let expires_at = now + timeout;
        let key = PendingQrKey::new(provider, rollcall_id);
        let (token, account_count, submitted_qr_data) = {
            let mut inner = self.inner.lock().await;
            cleanup_expired_locked(&mut inner, now);

            let entry = inner.entry(key).or_insert_with(|| PendingQrEntry {
                token: Uuid::new_v4().simple().to_string(),
                expires_at,
                used: false,
                submitted_qr_data: None,
                accounts: HashMap::new(),
            });

            if entry.used && entry.submitted_qr_data.is_none() {
                entry.token = Uuid::new_v4().simple().to_string();
                entry.used = false;
                entry.accounts.clear();
            }
            entry.expires_at = entry.expires_at.max(expires_at);
            entry
                .accounts
                .insert(account_id.to_string(), PendingAccount { tx: tx.clone() });

            (
                entry.token.clone(),
                entry.accounts.len(),
                entry.submitted_qr_data.clone(),
            )
        };

        if let Some(qr_data) = submitted_qr_data.as_ref() {
            if let Err(e) = tx.send(qr_data.clone()).await {
                warn!(account_id = %account_id, error = %e, "已提交 QR data 補送失敗");
            }
        }

        let scan_url = self.build_scan_url(provider, rollcall_id, account_id, &token)?;
        debug!(
            provider = %provider,
            rollcall_id = rollcall_id,
            account_id = %account_id,
            account_count = account_count,
            already_submitted = submitted_qr_data.is_some(),
            "已註冊共享 QR scanner 等待項目"
        );

        Ok(ScannerLink {
            scan_url,
            token,
            already_submitted: submitted_qr_data.is_some(),
        })
    }

    /// Remove one account from a pending QR group.
    pub async fn unregister_pending(&self, provider: &str, rollcall_id: u64, account_id: &str) {
        let key = PendingQrKey::new(provider, rollcall_id);
        let mut inner = self.inner.lock().await;
        if let Some(entry) = inner.get_mut(&key) {
            entry.accounts.remove(account_id);
            if entry.accounts.is_empty() && !entry.used {
                inner.remove(&key);
            }
        }
    }

    /// Submit scanned QR data and fan it out to every currently waiting account.
    pub async fn submit(
        &self,
        submission: ScannerSubmission,
    ) -> Result<ScannerSubmitResult, ScannerError> {
        let provider = submission.provider.trim();
        let account_id = submission.account_id.trim();
        let token = submission.token.trim();
        let qr_data = submission.qr_data.trim();

        if provider.is_empty() || account_id.is_empty() || token.is_empty() || qr_data.is_empty() {
            return Err(ScannerError::InvalidSubmission("missing required field"));
        }
        if qr_data.len() > MAX_QR_DATA_LEN {
            return Err(ScannerError::QrDataTooLarge {
                max: MAX_QR_DATA_LEN,
            });
        }

        let key = PendingQrKey::new(provider, submission.rollcall_id);
        let account_senders = {
            let now = Instant::now();
            let mut inner = self.inner.lock().await;
            let entry = inner.get_mut(&key).ok_or(ScannerError::PendingNotFound)?;

            if now > entry.expires_at {
                inner.remove(&key);
                return Err(ScannerError::Expired);
            }
            if entry.used {
                return Err(ScannerError::TokenAlreadyUsed);
            }
            if entry.token != token {
                return Err(ScannerError::InvalidToken);
            }
            if !entry.accounts.contains_key(account_id) {
                return Err(ScannerError::AccountNotPending);
            }

            entry.used = true;
            entry.submitted_qr_data = Some(qr_data.to_string());
            entry
                .accounts
                .iter()
                .map(|(account_id, account)| (account_id.clone(), account.tx.clone()))
                .collect::<Vec<_>>()
        };

        let mut delivered_accounts = Vec::new();
        let mut failed_accounts = Vec::new();
        for (account_id, tx) in account_senders {
            match tx.send(qr_data.to_string()).await {
                Ok(()) => delivered_accounts.push(account_id),
                Err(e) => {
                    warn!(account_id = %account_id, error = %e, "QR scanner callback 傳送失敗");
                    failed_accounts.push(account_id);
                }
            }
        }

        if delivered_accounts.is_empty() {
            return Err(ScannerError::NoDeliveredAccounts);
        }

        Ok(ScannerSubmitResult {
            delivered_count: delivered_accounts.len(),
            delivered_accounts,
            failed_accounts,
        })
    }

    fn build_scan_url(
        &self,
        provider: &str,
        rollcall_id: u64,
        account_id: &str,
        token: &str,
    ) -> Result<String, ScannerError> {
        let mut base =
            Url::parse(&self.public_base_url).map_err(|_| ScannerError::InvalidPublicBaseUrl)?;
        base.set_path(self.scanner_path.trim_start_matches('/'));
        base.query_pairs_mut()
            .clear()
            .append_pair("provider", provider)
            .append_pair("rollcall_id", &rollcall_id.to_string())
            .append_pair("account_id", account_id)
            .append_pair("token", token);
        Ok(base.to_string())
    }
}

pub fn build_scanner_router(scanner: Arc<QrScannerRegistry>) -> Router {
    Router::new()
        .route("/scanner", get(scanner_page_handler))
        .route("/scanner/submit", post(scanner_submit_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(ScannerHttpState::new(scanner))
}

pub async fn start_scanner_server(scanner: Arc<QrScannerRegistry>, port: u16) -> Result<()> {
    let app = build_scanner_router(scanner);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));

    info!(port = port, "QR scanner 伺服器啟動：http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to bind scanner server to port {port}"))?;

    axum::serve(listener, app)
        .await
        .into_diagnostic()
        .wrap_err("Scanner server error")
}

async fn scanner_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("line/scanner.html")),
    )
        .into_response()
}

async fn scanner_submit_handler(
    State(state): State<ScannerHttpState>,
    Json(submission): Json<ScannerSubmission>,
) -> Response {
    match state.scanner.submit(submission).await {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

fn cleanup_expired_locked(inner: &mut HashMap<PendingQrKey, PendingQrEntry>, now: Instant) {
    inner.retain(|_, entry| entry.expires_at >= now);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannerLink {
    pub scan_url: String,
    pub token: String,
    pub already_submitted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScannerSubmission {
    pub provider: String,
    pub rollcall_id: u64,
    pub account_id: String,
    pub token: String,
    pub qr_data: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScannerSubmitResult {
    pub delivered_count: usize,
    pub delivered_accounts: Vec<String>,
    pub failed_accounts: Vec<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ScannerError {
    #[error("scanner public_base_url 不是有效 URL")]
    InvalidPublicBaseUrl,
    #[error("scanner submission 欄位不完整：{0}")]
    InvalidSubmission(&'static str),
    #[error("找不到等待中的 QR Code 簽到")]
    PendingNotFound,
    #[error("QR Code scanner token 已過期")]
    Expired,
    #[error("QR Code scanner token 已使用")]
    TokenAlreadyUsed,
    #[error("QR Code scanner token 不正確")]
    InvalidToken,
    #[error("此帳號沒有等待中的 QR Code 簽到")]
    AccountNotPending,
    #[error("QR Code 資料過大，最大 {max} bytes")]
    QrDataTooLarge { max: usize },
    #[error("沒有任何帳號收到 QR Code 資料")]
    NoDeliveredAccounts,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn registry() -> QrScannerRegistry {
        QrScannerRegistry::new("https://scanner.example.test/base")
    }

    #[tokio::test]
    async fn register_pending_builds_scanner_url() {
        let registry = registry();
        let (tx, _rx) = mpsc::channel(1);

        let link = registry
            .register_pending("fju", 42, "acc1", tx, Duration::from_secs(60))
            .await
            .unwrap();

        assert!(link
            .scan_url
            .starts_with("https://scanner.example.test/scanner?"));
        assert!(link.scan_url.contains("provider=fju"));
        assert!(link.scan_url.contains("rollcall_id=42"));
        assert!(link.scan_url.contains("account_id=acc1"));
        assert!(link.scan_url.contains("token="));
        assert_eq!(link.token.len(), 32);
    }

    #[tokio::test]
    async fn submit_fans_out_to_all_accounts_for_same_rollcall() {
        let registry = registry();
        let (tx_a, mut rx_a) = mpsc::channel(1);
        let (tx_b, mut rx_b) = mpsc::channel(1);

        let link = registry
            .register_pending("fju", 42, "acc-a", tx_a, Duration::from_secs(60))
            .await
            .unwrap();
        registry
            .register_pending("fju", 42, "acc-b", tx_b, Duration::from_secs(60))
            .await
            .unwrap();

        let result = registry
            .submit(ScannerSubmission {
                provider: "fju".to_string(),
                rollcall_id: 42,
                account_id: "acc-a".to_string(),
                token: link.token,
                qr_data: "0~100!3~secret!4~42".to_string(),
            })
            .await
            .unwrap();

        assert_eq!(result.delivered_count, 2);
        assert_eq!(rx_a.recv().await.unwrap(), "0~100!3~secret!4~42");
        assert_eq!(rx_b.recv().await.unwrap(), "0~100!3~secret!4~42");
    }

    #[tokio::test]
    async fn token_reuse_is_rejected() {
        let registry = registry();
        let (tx, mut rx) = mpsc::channel(2);
        let link = registry
            .register_pending("fju", 42, "acc1", tx, Duration::from_secs(60))
            .await
            .unwrap();

        let submission = ScannerSubmission {
            provider: "fju".to_string(),
            rollcall_id: 42,
            account_id: "acc1".to_string(),
            token: link.token,
            qr_data: "0~100!3~secret!4~42".to_string(),
        };

        registry.submit(submission.clone()).await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), "0~100!3~secret!4~42");
        assert_eq!(
            registry.submit(submission).await.unwrap_err(),
            ScannerError::TokenAlreadyUsed
        );
    }

    #[tokio::test]
    async fn later_account_registration_receives_already_submitted_qr_data() {
        let registry = registry();
        let (tx_a, mut rx_a) = mpsc::channel(1);
        let link = registry
            .register_pending("fju", 42, "acc-a", tx_a, Duration::from_secs(60))
            .await
            .unwrap();

        registry
            .submit(ScannerSubmission {
                provider: "fju".to_string(),
                rollcall_id: 42,
                account_id: "acc-a".to_string(),
                token: link.token,
                qr_data: "0~100!3~secret!4~42".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(rx_a.recv().await.unwrap(), "0~100!3~secret!4~42");

        let (tx_b, mut rx_b) = mpsc::channel(1);
        let link_b = registry
            .register_pending("fju", 42, "acc-b", tx_b, Duration::from_secs(60))
            .await
            .unwrap();

        assert!(link_b.already_submitted);
        assert_eq!(rx_b.recv().await.unwrap(), "0~100!3~secret!4~42");
    }

    #[tokio::test]
    async fn rejects_wrong_rollcall_provider_token_and_expired_token() {
        let registry = registry();
        let (tx, _rx) = mpsc::channel(1);
        let link = registry
            .register_pending("fju", 42, "acc1", tx, Duration::from_millis(20))
            .await
            .unwrap();

        let base = ScannerSubmission {
            provider: "fju".to_string(),
            rollcall_id: 42,
            account_id: "acc1".to_string(),
            token: link.token.clone(),
            qr_data: "0~100!3~secret!4~42".to_string(),
        };

        let mut wrong_rollcall = base.clone();
        wrong_rollcall.rollcall_id = 43;
        assert_eq!(
            registry.submit(wrong_rollcall).await.unwrap_err(),
            ScannerError::PendingNotFound
        );

        let mut wrong_provider = base.clone();
        wrong_provider.provider = "other".to_string();
        assert_eq!(
            registry.submit(wrong_provider).await.unwrap_err(),
            ScannerError::PendingNotFound
        );

        let mut wrong_token = base.clone();
        wrong_token.token = "wrong".to_string();
        assert_eq!(
            registry.submit(wrong_token).await.unwrap_err(),
            ScannerError::InvalidToken
        );

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            registry.submit(base).await.unwrap_err(),
            ScannerError::Expired
        );
    }
}
