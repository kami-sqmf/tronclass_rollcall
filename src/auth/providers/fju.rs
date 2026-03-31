use std::sync::Arc;

use async_trait::async_trait;
use miette::{IntoDiagnostic, Result, WrapErr};
use reqwest::{cookie::Jar, header, Client, Url};
use tracing::{debug, info, warn};

use crate::config::{AccountConfig, ProviderConfig};

use super::super::{
    build_cas_login_url, extract_captcha_url, extract_cas_error, extract_hidden_fields,
    html_has_input_named, require_cookie, verify_session_with_client, AuthError, AuthSession,
};
use super::AuthFlow;

const LOGIN_PATH: &str = "/cas/login";
const SERVICE_PATH: &str = "/login?next=/user/index";
const MAX_CAPTCHA_RETRIES: u32 = 5;
const MAX_LOGIN_RETRIES: u32 = 3;

pub fn provider_config() -> ProviderConfig {
    ProviderConfig::default()
}

pub fn create_flow() -> Box<dyn AuthFlow> {
    Box::new(FjuAuthFlow)
}

/// 下載並 OCR 單次驗證碼，每次加隨機 query 讓伺服器重新產生圖片。
async fn fetch_and_ocr_captcha(client: &Client, captcha_base_url: &str) -> Result<String> {
    let url = format!("{}?{}", captcha_base_url, rand::random::<u32>());

    let bytes = client
        .get(&url)
        .send()
        .await
        .into_diagnostic()
        .wrap_err("FJU: Failed to download captcha image")?
        .bytes()
        .await
        .into_diagnostic()
        .wrap_err("FJU: Failed to read captcha image bytes")?;

    let ocr = ddddocr::ddddocr_classification()
        .map_err(|e| miette::miette!("FJU: 初始化 ddddocr 失敗：{e}"))?;

    ocr.classification(bytes.as_ref())
        .map_err(|e| miette::miette!("FJU: 驗證碼識別失敗：{e}"))
}

/// 嘗試辨識驗證碼；若結果非 4 位則重刷，最多重試 [`MAX_CAPTCHA_RETRIES`] 次。
async fn solve_captcha(client: &Client, html: &str, base_url: &str) -> Result<String> {
    let captcha_url = extract_captcha_url(html, base_url)
        .ok_or_else(|| miette::miette!("FJU: 無法從登入頁找到驗證碼圖片"))?;

    // 取 base（去掉 query），後續每次重刷都換新 query
    let captcha_base = captcha_url.split('?').next().unwrap_or(&captcha_url).to_string();

    debug!(url = %captcha_base, "FJU: 下載驗證碼圖片");

    for attempt in 0..MAX_CAPTCHA_RETRIES {
        let result = fetch_and_ocr_captcha(client, &captcha_base).await?;
        if result.len() == 4 {
            debug!(captcha = %result, attempt, "FJU: 驗證碼識別成功");
            return Ok(result);
        }
        warn!(captcha = %result, attempt, "FJU: 驗證碼非4位（{}位），重刷", result.len());
    }

    Err(miette::miette!(
        "FJU: 嘗試 {} 次後仍無法識別4位驗證碼",
        MAX_CAPTCHA_RETRIES
    ))
}

struct FjuAuthFlow;

#[async_trait]
impl AuthFlow for FjuAuthFlow {
    async fn login(
        &self,
        client: &Client,
        cookie_jar: &Arc<Jar>,
        base_url: &str,
        account: &AccountConfig,
    ) -> Result<AuthSession> {
        let cas_login_url = format!("{}{}", base_url, LOGIN_PATH);
        let service_url = format!("{}{}", base_url, SERVICE_PATH);
        let login_url = build_cas_login_url(&cas_login_url, &service_url).into_diagnostic()?;
        let login_url_parsed = Url::parse(&cas_login_url).expect("valid URL");

        let mut last_err = String::new();

        for attempt in 0..MAX_LOGIN_RETRIES {
            if attempt > 0 {
                warn!(attempt, "FJU: 重新嘗試登入");
            }

            // ── GET 登入頁面（每次重試都重新拿，確保 hidden fields 有效）──
            debug!(url = %login_url, "FJU: GET 登錄頁面");
            let resp = client
                .get(&login_url)
                .send()
                .await
                .into_diagnostic()
                .wrap_err("FJU: Failed to GET login page")?;

            if !resp.status().is_success() {
                return Err(miette::miette!(
                    "FJU: 登錄頁面返回非 2xx 狀態：{}",
                    resp.status()
                ));
            }

            let html = resp
                .text()
                .await
                .into_diagnostic()
                .wrap_err("FJU: Failed to read login page")?;

            let hidden =
                extract_hidden_fields(&html, &["lt".to_string(), "execution".to_string()])
                    .into_diagnostic()
                    .wrap_err("FJU: 無法提取 hidden 欄位")?;

            debug!(fields = ?hidden.keys().collect::<Vec<_>>(), "FJU: 提取到 hidden fields");

            // ── 辨識驗證碼（內部已處理非4位的重刷重試）──
            let captcha = solve_captcha(client, &html, &cas_login_url).await?;

            // ── POST 登入表單 ──
            let mut form = vec![
                ("username".to_string(), account.username.clone()),
                ("password".to_string(), account.password.clone()),
                ("captcha".to_string(), captcha),
            ];
            form.extend(hidden.iter().map(|(k, v)| (k.clone(), v.clone())));
            form.push(("_eventId".to_string(), "submit".to_string()));
            form.push(("submit".to_string(), "LOGIN".to_string()));

            let post_resp = client
                .post(&login_url)
                .form(&form)
                .header(header::REFERER, &login_url)
                .send()
                .await
                .into_diagnostic()
                .wrap_err("FJU: Failed to POST login form")?;

            let final_url = post_resp.url().clone();
            let final_body = post_resp
                .text()
                .await
                .into_diagnostic()
                .wrap_err("FJU: Failed to read response")?;

            let still_on_login = final_url.host_str() == login_url_parsed.host_str()
                && final_url.path() == login_url_parsed.path()
                && (html_has_input_named(&final_body, "username")
                    || html_has_input_named(&final_body, "captcha")
                    || html_has_input_named(&final_body, "lt"));

            if still_on_login {
                last_err = extract_cas_error(&final_body)
                    .unwrap_or_else(|| "帳號、密碼或驗證碼錯誤".to_string());
                warn!(attempt, error = %last_err, final_url = %final_url, "FJU: 登入失敗，仍停留在登入頁");
                continue;
            }

            // ── 登入成功 ──
            let success = [
                "Log In Successful",
                "successfully logged into the Central Authentication Service",
            ]
            .iter()
            .any(|m| final_body.contains(m));

            if success {
                info!(final_url = %final_url, "FJU: 登入成功，停留在成功頁");
            } else {
                info!(final_url = %final_url, "FJU: 登入成功，已離開登入頁");
            }

            let base_url_parsed = Url::parse(base_url).expect("valid base URL");
            let session_cookie = require_cookie(cookie_jar, &base_url_parsed, "session")
                .into_diagnostic()
                .wrap_err("FJU: 登入後未找到 session cookie")?;
            debug!(session = %session_cookie, "FJU: session cookie 取得成功");

            let user_name = verify_session_with_client(client, base_url)
                .await
                .into_diagnostic()
                .wrap_err("FJU: session 驗證失敗")?;

            return Ok(AuthSession {
                user_name,
                cookie_string: format!("session={}", session_cookie),
            });
        }

        Err(AuthError::LoginFailed {
            reason: format!("已重試 {} 次，最後錯誤：{}", MAX_LOGIN_RETRIES, last_err),
        })
        .into_diagnostic()
    }
}
