//! Tronclass API 客戶端連線模組
//!
//! API 將通過此模組進行對外的發送，並發包或接收下級的請求：
//! - `GET  /api/profile`                                      → 取得使用者資料

pub mod profile;
pub mod rollcall;

use std::sync::Arc;
use std::time::Duration;

use miette::{IntoDiagnostic, Result, WrapErr};
use reqwest::{
    cookie::Jar,
    header::{self, HeaderMap, HeaderValue},
    Client, StatusCode,
};
use thiserror::Error;

// ─── 錯誤類型 ─────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP 請求失敗：{0}")]
    Http(#[from] reqwest::Error),

    #[error("API 返回錯誤狀態 {status}：{body}")]
    ApiStatus { status: u16, body: String },

    #[error("Session 已過期（HTTP 401/403），需要重新登錄")]
    Unauthorized,

    #[error(transparent)]
    Rollcall(#[from] rollcall::RollcallError),

    #[error("JSON 解析失敗：{0}")]
    Json(String),
}

// ─── API 客戶端 ───────────────────────────────────────────────────────────────
/// Tronclass API 客戶端
pub struct ApiClient {
    /// API 基礎 URL
    pub(crate) base_url: String,
    /// HTTP client（從 Auth 取得已登入的 Session）
    pub(crate) client: Client,
}

impl ApiClient {
    /// 從已認證的 client 建立 API 客戶端
    pub fn new(client: Client, base_url: impl Into<String>) -> Self {
        Self {
            client,
            base_url: base_url.into(),
        }
    }

    /// 通用回應處理：反序列化 JSON，遇到 401/403 轉換為 Unauthorized 錯誤
    pub(crate) async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T> {
        let status = resp.status();

        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(miette::miette!(ApiError::Unauthorized));
            }
            _ if !status.is_success() => {
                let body = resp.text().await.unwrap_or_default();
                return Err(miette::miette!(ApiError::ApiStatus {
                    status: status.as_u16(),
                    body,
                }));
            }
            _ => {}
        }

        let body_bytes = resp
            .bytes()
            .await
            .into_diagnostic()
            .wrap_err("Failed to read response body")?;

        serde_json::from_slice::<T>(&body_bytes).map_err(|e| {
            let body_str = String::from_utf8_lossy(&body_bytes);
            miette::miette!(ApiError::Json(format!(
                "{e}: body was `{}`",
                &body_str[..body_str.len().min(200)]
            )))
        })
    }
}

// ─── HTTP Client ─────────────────────────────────────────────────────────
/// 建立帶有預設 Headers 和 Cookie Jar 的 HTTP client
/// 由認證模組呼叫，建立後交給 `ApiClient` 使用。
pub fn build_http_client(cookie_jar: Arc<Jar>, timeout_secs: u64) -> reqwest::Result<Client> {
    let mut default_headers = HeaderMap::new();

    default_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) \
             AppleWebKit/605.1.15 (KHTML, like Gecko) \
             Mobile/15E148 Tronclass/5.0",
        ),
    );
    default_headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    default_headers.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static("zh-TW,zh;q=0.9,en;q=0.8"),
    );

    Client::builder()
        .cookie_provider(cookie_jar)
        .default_headers(default_headers)
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::limited(10))
        .use_rustls_tls()
        .build()
}

// ─── 輔助函式 ─────────────────────────────────────────────────────────────────

/// 判斷錯誤字串是否為認證錯誤（需要重新登錄）
pub(crate) fn is_auth_error(err: &str) -> bool {
    err.contains("Unauthorized")
        || err.contains("401")
        || err.contains("403")
        || err.contains("Session")
        || err.contains("session")
        || err.contains("登錄")
        || err.contains("認證")
        || err.contains("致命錯誤")
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ApiError 訊息格式 ─────────────────────────────────────────────────────

    #[test]
    fn test_api_error_unauthorized_message() {
        let err = ApiError::Unauthorized;
        assert!(err.to_string().contains("401") || err.to_string().contains("重新登錄"));
    }

    #[test]
    fn test_api_error_api_status_message() {
        let err = ApiError::ApiStatus {
            status: 500,
            body: "Internal Server Error".to_string(),
        };
        assert!(err.to_string().contains("500"));
        assert!(err.to_string().contains("Internal Server Error"));
    }

    #[test]
    fn test_api_error_json_message() {
        let err = ApiError::Json("unexpected token".to_string());
        assert!(err.to_string().contains("unexpected token"));
    }

    // ── build_http_client ────────────────────────────────────────────────────

    #[test]
    fn test_build_http_client_succeeds() {
        let jar = Arc::new(Jar::default());
        assert!(build_http_client(jar, 30).is_ok());
    }
}
