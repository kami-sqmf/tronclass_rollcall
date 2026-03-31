//! 認證模組。
//!
//! 提供 [`AuthClient`] 與各 provider 的登入流程，以及共用的 HTML 解析工具函數。
//!
//! ## 主要職責
//! - 建立／重建帶有 cookie jar 的 HTTP client
//! - 依帳號設定分派對應的 [`providers::AuthFlow`] 執行登入
//! - 支援手動 cookie 注入（bypass 自動登入）
//! - 驗證 session 有效性（`/api/profile`）

pub mod providers;

use std::sync::Arc;

use miette::{IntoDiagnostic, Result, WrapErr};
use reqwest::{cookie::Jar, Client, Url};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

use crate::config::AccountConfig;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("HTTP 請求失敗：{0}")]
    Http(#[from] reqwest::Error),

    #[error("無法從登錄頁面提取 hidden 欄位：{name}")]
    MissingHiddenField { name: String },

    #[error("CAS 登錄失敗：{reason}")]
    LoginFailed { reason: String },

    #[error("無效的 URL：{0}")]
    InvalidUrl(String),

    #[error("找不到 Cookie：{name}")]
    CookieNotFound { name: String },

    #[error("登錄後驗證失敗：無法獲取使用者資料")]
    ProfileVerifyFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthSession {
    pub user_name: String,
    pub cookie_string: String,
}

/// 認證客戶端，負責登錄並持有認證後的 HTTP client。
pub struct AuthClient {
    cookie_jar: Arc<Jar>,
    flow: Box<dyn providers::AuthFlow>,
    pub client: Client,
    base_url: String,
}

impl AuthClient {
    #[instrument(skip(account), fields(account_id = %account.id, username = %account.username, provider = %account.provider))]
    pub async fn new(account: &AccountConfig) -> Result<(Self, AuthSession)> {
        let cookie_jar = Arc::new(Jar::default());
        let client =
            crate::api::build_http_client(Arc::clone(&cookie_jar), account.request_timeout_secs)
                .into_diagnostic()
                .wrap_err("Failed to build HTTP client")?;
        let flow = providers::get_auth_flow(&account.provider);

        let auth_client = AuthClient {
            cookie_jar: Arc::clone(&cookie_jar),
            flow,
            client,
            base_url: account.base_url().to_string(),
        };

        let session = auth_client.authenticate(account).await?;
        Ok((auth_client, session))
    }

    async fn authenticate(&self, account: &AccountConfig) -> Result<AuthSession> {
        if account.use_manual_cookie() {
            info!("使用手動 cookie 模式，跳過自動登入流程");
            self.inject_manual_cookie(&account.manual_cookie).await
        } else {
            self.flow
                .login(&self.client, &self.cookie_jar, &self.base_url, account)
                .await
        }
    }

    async fn inject_manual_cookie(&self, cookie_str: &str) -> Result<AuthSession> {
        let url = Url::parse(&self.base_url)
            .into_diagnostic()
            .wrap_err_with(|| format!("Invalid base URL: {}", self.base_url))?;

        for part in cookie_str.split(';') {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                self.cookie_jar.add_cookie_str(trimmed, &url);
            }
        }

        use reqwest::cookie::CookieStore;
        if let Some(h) = self.cookie_jar.cookies(&url) {
            debug!("目前 Cookie 為：{}", h.to_str().unwrap_or("<non-utf8>"));
        }

        let profile = self.verify_session().await.map_err(|e| {
            miette::miette!("手動 cookie 驗證失敗：{e}\n請確認 cookie 是否仍然有效")
        })?;

        info!(user_name = %profile, "手動 cookie 驗證成功");

        Ok(AuthSession {
            user_name: profile,
            cookie_string: cookie_str.to_string(),
        })
    }

    pub async fn verify_session(&self) -> Result<String> {
        verify_session_with_client(&self.client, &self.base_url)
            .await
            .into_diagnostic()
    }

    #[instrument(skip(self, account), fields(account_id = %account.id, provider = %account.provider))]
    pub async fn re_authenticate(&mut self, account: &AccountConfig) -> Result<AuthSession> {
        info!("Session 可能已過期，嘗試重新認證...");

        self.cookie_jar = Arc::new(Jar::default());
        self.client = crate::api::build_http_client(
            Arc::clone(&self.cookie_jar),
            account.request_timeout_secs,
        )
        .into_diagnostic()
        .wrap_err("Failed to rebuild HTTP client for re-auth")?;

        if account.use_manual_cookie() {
            warn!("使用手動 cookie 模式，無法自動重新認證");
            return Err(miette::miette!(
                "手動 cookie 已過期，請更新 accounts.toml 中對應帳號的 manual_cookie"
            ));
        }

        self.flow
            .login(&self.client, &self.cookie_jar, &self.base_url, account)
            .await
    }
}

pub(crate) async fn verify_session_with_client(
    client: &Client,
    base_url: &str,
) -> std::result::Result<String, AuthError> {
    debug!("驗證 session：GET /api/profile");
    let api = crate::api::ApiClient::new(client.clone(), base_url);
    let profile = api
        .get_profile()
        .await
        .map_err(|_| AuthError::ProfileVerifyFailed)?;
    Ok(profile.name)
}

pub(crate) fn extract_hidden_fields(
    html: &str,
    field_names: &[String],
) -> std::result::Result<std::collections::BTreeMap<String, String>, AuthError> {
    let mut values = std::collections::BTreeMap::new();

    for field_name in field_names {
        let value =
            extract_input_value(html, field_name).ok_or_else(|| AuthError::MissingHiddenField {
                name: field_name.clone(),
            })?;
        values.insert(field_name.clone(), value);
    }

    Ok(values)
}

pub(crate) fn extract_input_value(html: &str, field_name: &str) -> Option<String> {
    let input_re = regex::Regex::new(r#"(?is)<input\b[^>]*>"#).ok()?;
    let escaped_field_name = regex::escape(field_name);
    let name_patterns = [
        format!(r#"(?i)\bname\s*=\s*"{escaped_field_name}""#),
        format!(r#"(?i)\bname\s*=\s*'{escaped_field_name}'"#),
    ];
    let name_regexes = name_patterns
        .iter()
        .filter_map(|pattern| regex::Regex::new(pattern).ok())
        .collect::<Vec<_>>();
    let value_re = regex::Regex::new(r#"(?i)\bvalue\s*=\s*(?:"([^"]*)"|'([^']*)')"#).ok()?;

    for mat in input_re.find_iter(html) {
        let tag = mat.as_str();
        if !name_regexes.iter().any(|re| re.is_match(tag)) {
            continue;
        }

        if let Some(caps) = value_re.captures(tag) {
            if let Some(value) = caps.get(1).or_else(|| caps.get(2)) {
                return Some(value.as_str().to_string());
            }
        }
    }

    None
}

pub(crate) fn html_has_input_named(html: &str, field_name: &str) -> bool {
    extract_input_value(html, field_name).is_some()
}

pub(crate) fn extract_cas_error(html: &str) -> Option<String> {
    let patterns = [
        r#"<span id="msg"[^>]*>([^<]+)</span>"#,
        r#"class="errors?"[^>]*>([^<]+)<"#,
        r#"<div[^>]*class="[^"]*error[^"]*"[^>]*>([^<]+)<"#,
        r#"<p class="warning">([^<]+)</p>"#,
    ];

    for pattern in &patterns {
        if let Ok(re) = regex::Regex::new(pattern) {
            if let Some(caps) = re.captures(html) {
                if let Some(msg) = caps.get(1) {
                    let text = msg.as_str().trim().to_string();
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
            }
        }
    }
    None
}

/// 從登入頁 HTML 中提取驗證碼圖片的絕對 URL。
///
/// 尋找含有 `captcha` 的 `<img>` tag，再依 `base_url` 補全相對路徑。
pub(crate) fn extract_captcha_url(html: &str, base_url: &str) -> Option<String> {
    let re = regex::Regex::new(r#"(?i)<img\b[^>]*>"#).ok()?;
    let src_re = regex::Regex::new(r#"(?i)\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)')"#).ok()?;

    for mat in re.find_iter(html) {
        let tag = mat.as_str();
        if !tag.to_lowercase().contains("captcha") {
            continue;
        }
        if let Some(caps) = src_re.captures(tag) {
            let src = caps.get(1).or_else(|| caps.get(2))?.as_str();
            if src.starts_with("http://") || src.starts_with("https://") {
                return Some(src.to_string());
            }
            // 相對路徑補全
            let base = Url::parse(base_url).ok()?;
            return Some(base.join(src).ok()?.to_string());
        }
    }
    None
}

pub(crate) fn build_cas_login_url(
    login_url: &str,
    service_url: &str,
) -> std::result::Result<String, AuthError> {
    let mut url =
        Url::parse(login_url).map_err(|_| AuthError::InvalidUrl(login_url.to_string()))?;

    if !service_url.is_empty() {
        url.query_pairs_mut().append_pair("service", service_url);
    }

    Ok(url.to_string())
}

/// jar 內按名稱找一個 cookie 的值；找不到回傳 [`AuthError::CookieNotFound`]。
pub(crate) fn require_cookie(
    jar: &Jar,
    url: &Url,
    name: &str,
) -> std::result::Result<String, AuthError> {
    use reqwest::cookie::CookieStore;
    let header = jar.cookies(url).ok_or_else(|| AuthError::CookieNotFound {
        name: name.to_string(),
    })?;
    let cookie_str = header.to_str().map_err(|_| AuthError::CookieNotFound {
        name: name.to_string(),
    })?;

    for pair in cookie_str.split(';') {
        if let Some((k, v)) = pair.trim().split_once('=') {
            if k.trim() == name {
                return Ok(v.trim().to_string());
            }
        }
    }

    Err(AuthError::CookieNotFound {
        name: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_hidden_fields() {
        let html = r#"
            <input type="hidden" name="lt" value="LT-12345-abc" />
            <input type="hidden" name="execution" value="e1s1" />
        "#;

        let fields = extract_hidden_fields(html, &["lt".to_string(), "execution".to_string()])
            .expect("fields should exist");
        assert_eq!(fields.get("lt"), Some(&"LT-12345-abc".to_string()));
        assert_eq!(fields.get("execution"), Some(&"e1s1".to_string()));
    }

    #[test]
    fn test_extract_hidden_field_not_found() {
        let html = "<html><body>no token here</body></html>";
        let err = extract_hidden_fields(html, &["lt".to_string()]).unwrap_err();
        assert!(matches!(err, AuthError::MissingHiddenField { name } if name == "lt"));
    }

    #[test]
    fn test_extract_cas_error() {
        let html = r#"<span id="msg" class="error">帳號或密碼錯誤</span>"#;
        let err = extract_cas_error(html);
        assert!(err.is_some());
        assert!(err.unwrap().contains("帳號或密碼錯誤"));
    }

    #[test]
    fn test_build_cas_login_url_appends_service_query() {
        let login_url = "https://elearn2.fju.edu.tw/cas/login";
        let service_url = "https://elearn2.fju.edu.tw/cas/login?type=3";
        let url = build_cas_login_url(login_url, service_url).expect("url should be valid");
        assert!(url.starts_with("https://elearn2.fju.edu.tw/cas/login?"));
        assert!(url.contains("service=https%3A%2F%2Felearn2.fju.edu.tw%2Fcas%2Flogin%3Ftype%3D3"));
    }

    #[test]
    fn test_build_cas_login_url_without_service_keeps_base_url() {
        let url = build_cas_login_url("https://elearn2.fju.edu.tw/cas/login", "")
            .expect("url should be valid");
        assert_eq!(url, "https://elearn2.fju.edu.tw/cas/login");
    }

    #[test]
    fn test_extract_execution_with_base64_like_value() {
        let html = r#"
            <input type="hidden" name="lt" value="LT-999-XYZ" />
            <input type="hidden"
                   name="execution"
                   value="e2s1==abc+def/ghi" />
        "#;
        let exec = extract_input_value(html, "execution");
        assert_eq!(exec, Some("e2s1==abc+def/ghi".to_string()));
    }

    #[test]
    fn test_extract_input_value_accepts_single_quotes_and_reordered_attributes() {
        let html = r#"
            <input value='token-123' type='hidden' data-x='1' name='lt'>
        "#;

        let value = extract_input_value(html, "lt");
        assert_eq!(value, Some("token-123".to_string()));
    }

    #[test]
    fn test_html_has_input_named_found() {
        let html = r#"<input type="text" name="captcha" value="">"#;
        assert!(html_has_input_named(html, "captcha"));
    }

    #[test]
    fn test_html_has_input_named_not_found() {
        let html = r#"<input type="text" name="username" value="">"#;
        assert!(!html_has_input_named(html, "captcha"));
    }

    #[test]
    fn test_extract_captcha_url_relative() {
        // 模擬 FJU 登入頁的真實 HTML 片段
        let html = r#"<img src="captcha.jpg?0.6770975559619112" onclick="this.src='captcha.jpg?'+Math.random();">"#;
        let base_url = "https://elearn2.fju.edu.tw/cas/login";
        let url = extract_captcha_url(html, base_url).expect("should find captcha url");
        assert_eq!(
            url,
            "https://elearn2.fju.edu.tw/cas/captcha.jpg?0.6770975559619112"
        );
    }

    #[test]
    fn test_extract_captcha_url_absolute() {
        let html = r#"<img src="https://elearn2.fju.edu.tw/cas/captcha.jpg?0.5" alt="captcha">"#;
        let url = extract_captcha_url(html, "https://elearn2.fju.edu.tw")
            .expect("should find captcha url");
        assert_eq!(url, "https://elearn2.fju.edu.tw/cas/captcha.jpg?0.5");
    }

    #[test]
    fn test_extract_captcha_url_not_found() {
        let html = r#"<img src="logo.png" alt="logo">"#;
        assert!(extract_captcha_url(html, "https://elearn2.fju.edu.tw").is_none());
    }
}
