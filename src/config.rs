//! 設定管理模組
//!
//! `config.toml` 僅存放全域設定與 CAS provider。
//! 多帳號資料由獨立的 `accounts.toml` 管理，載入後會解析成可直接執行的 `AccountConfig`。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use miette::{IntoDiagnostic, Result, WrapErr};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};

/// 全域設定單例
static CONFIG: OnceCell<AppConfig> = OnceCell::new();

/// 取得全域設定（必須先呼叫 `init_config`）
pub fn get_config() -> &'static AppConfig {
    CONFIG
        .get()
        .expect("Config not initialized. Call init_config() first.")
}

/// 從指定路徑初始化全域設定
pub fn init_config(path: impl AsRef<Path>) -> Result<&'static AppConfig> {
    let cfg = AppConfig::load(path)?;
    CONFIG
        .set(cfg)
        .map_err(|_| miette::miette!("Config already initialized"))?;
    Ok(CONFIG.get().unwrap())
}

// ─── 根設定 ───────────────────────────────────────────────────────────────────

/// 應用程式全域設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    /// 可用 CAS provider 清單
    #[serde(default = "default_providers")]
    pub providers: BTreeMap<String, ProviderConfig>,

    /// API 設定
    pub api: ApiConfig,

    /// 雷達簽到設定
    pub radar: RadarConfig,

    /// 數字爆破設定
    pub brute_force: BruteForceConfig,

    /// Line Bot 設定
    pub line_bot: LineBotConfig,

    /// QR Code 設定
    pub qrcode: QrCodeConfig,

    /// 日誌設定
    pub logging: LoggingConfig,

    /// 監控設定
    pub monitor: MonitorConfig,
}

/// 解析完成、可直接執行的帳號設定
#[derive(Debug, Clone, Serialize)]
pub struct AccountConfig {
    pub id: String,
    pub provider: String,
    pub username: String,
    pub password: String,
    pub captcha: String,
    pub manual_cookie: String,
    pub enabled: bool,
    pub line_user_id: String,
    pub provider_config: ProviderConfig,
    pub request_timeout_secs: u64,
}

impl AccountConfig {
    pub fn base_url(&self) -> &str {
        &self.provider_config.base_url
    }

    pub fn use_manual_cookie(&self) -> bool {
        !self.manual_cookie.is_empty()
    }

    pub fn display_name(&self) -> &str {
        &self.id
    }
}

/// `accounts.toml` 檔案格式
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccountsFile {
    #[serde(default)]
    pub accounts: Vec<RawAccountConfig>,
}

/// 原始帳號設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawAccountConfig {
    pub id: String,
    pub provider: String,
    pub username: String,
    pub password: String,

    #[serde(default)]
    pub captcha: String,

    #[serde(default)]
    pub manual_cookie: String,

    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default)]
    pub line_user_id: String,
}

// ─── 子設定結構 ───────────────────────────────────────────────────────────────

/// Provider 固定種類
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Fju,
    Tku,
    Asia,
    Scu,
    Custom,
}

impl Default for ProviderKind {
    fn default() -> Self {
        Self::Fju
    }
}

/// 學校 Provider 設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: ProviderKind,

    /// Tronclass 或學校系統 API base URL；若為空則退回全域 api.base_url
    #[serde(default)]
    pub base_url: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::Fju,
            base_url: default_base_url(),
        }
    }
}

/// API 設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiConfig {
    /// Tronclass 基礎 URL
    #[serde(default = "default_base_url")]
    pub base_url: String,

    /// 輪詢間隔（秒）
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// 請求逾時（秒）
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,
}

/// 雷達簽到設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RadarConfig {
    #[serde(default = "default_coords")]
    pub default_coords: Vec<[f64; 2]>,

    #[serde(default = "default_accuracy")]
    pub accuracy: u32,

    #[serde(default)]
    pub altitude: i32,
}

/// 數字爆破設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BruteForceConfig {
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,

    #[serde(default)]
    pub request_delay_ms: u64,
}

/// Line Bot 設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LineBotConfig {
    #[serde(default)]
    pub enabled: bool,

    pub channel_secret: String,
    pub channel_access_token: String,

    #[serde(default = "default_webhook_port")]
    pub webhook_port: u16,

    #[serde(default = "default_webhook_path")]
    pub webhook_path: String,

    pub admin_user_id: String,
}

/// QR Code 簽到設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QrCodeConfig {
    #[serde(default = "default_scanner_base_url")]
    pub scanner_base_url: String,

    #[serde(default = "default_scan_timeout")]
    pub scan_timeout_secs: u64,
}

/// 日誌設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,

    #[serde(default)]
    pub log_to_file: bool,

    #[serde(default = "default_log_file_path")]
    pub log_file_path: String,
}

/// 監控設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MonitorConfig {
    #[serde(default = "default_startup_delay")]
    pub startup_delay_secs: u64,

    #[serde(default = "default_retry_interval")]
    pub retry_interval_secs: u64,

    #[serde(default = "default_max_failures")]
    pub max_failures_before_reauth: u32,
}

// ─── 預設值 ───────────────────────────────────────────────────────────────────

fn default_providers() -> BTreeMap<String, ProviderConfig> {
    crate::auth::providers::builtin_providers()
}

fn default_base_url() -> String {
    "https://elearn2.fju.edu.tw".to_string()
}

fn default_poll_interval() -> u64 {
    10
}

fn default_request_timeout() -> u64 {
    30
}

fn default_accuracy() -> u32 {
    35
}

fn default_concurrency() -> usize {
    200
}

fn default_true() -> bool {
    true
}

fn default_webhook_port() -> u16 {
    8080
}

fn default_webhook_path() -> String {
    "/webhook".to_string()
}

fn default_scanner_base_url() -> String {
    "https://elearn2.fju.edu.tw/scanner-jumper".to_string()
}

fn default_scan_timeout() -> u64 {
    60
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_file_path() -> String {
    "fju_ghost.log".to_string()
}

fn default_startup_delay() -> u64 {
    3
}

fn default_retry_interval() -> u64 {
    30
}

fn default_max_failures() -> u32 {
    5
}

fn default_coords() -> Vec<[f64; 2]> {
    vec![[24.3, 118.0], [24.6, 118.2]]
}

// ─── 載入與驗證 ───────────────────────────────────────────────────────────────

impl AppConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        let cfg = config::Config::builder()
            .add_source(
                config::File::from(path)
                    .required(true)
                    .format(config::FileFormat::Toml),
            )
            .add_source(
                config::Environment::with_prefix("FJU_GHOST")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to load config from `{}`", path.display()))?;

        cfg.try_deserialize::<AppConfig>()
            .into_diagnostic()
            .wrap_err("Failed to deserialize config")
    }

    pub fn load_default() -> Result<Self> {
        Self::load("config.toml")
    }

    pub fn validate(&self) -> Result<()> {
        if self.providers.is_empty() {
            return Err(miette::miette!("at least one provider is required"));
        }

        for (provider_name, provider) in &self.providers {
            validate_provider(provider_name, provider)?;
        }

        if self.line_bot.enabled {
            if self.line_bot.channel_secret.is_empty()
                || self.line_bot.channel_secret == "your_line_channel_secret"
            {
                return Err(miette::miette!(
                    "line_bot.channel_secret is required when line_bot.enabled = true"
                ));
            }
            if self.line_bot.channel_access_token.is_empty()
                || self.line_bot.channel_access_token == "your_line_channel_access_token"
            {
                return Err(miette::miette!(
                    "line_bot.channel_access_token is required when line_bot.enabled = true"
                ));
            }
            if self.line_bot.admin_user_id.is_empty() {
                return Err(miette::miette!(
                    "line_bot.admin_user_id is required when line_bot.enabled = true"
                ));
            }
        }

        if self.brute_force.concurrency == 0 {
            return Err(miette::miette!(
                "brute_force.concurrency must be greater than 0"
            ));
        }

        for coords in &self.radar.default_coords {
            let [lat, lon] = coords;
            if !(-90.0..=90.0).contains(lat) {
                return Err(miette::miette!(
                    "Invalid latitude {}: must be between -90 and 90",
                    lat
                ));
            }
            if !(-180.0..=180.0).contains(lon) {
                return Err(miette::miette!(
                    "Invalid longitude {}: must be between -180 and 180",
                    lon
                ));
            }
        }

        Ok(())
    }
}

impl AccountsFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to read accounts from `{}`", path.display()))?;

        toml::from_str::<Self>(&raw)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to parse accounts from `{}`", path.display()))
    }

    pub fn resolve(&self, app: &AppConfig) -> Result<Vec<AccountConfig>> {
        if self.accounts.is_empty() {
            return Err(miette::miette!(
                "accounts.toml must contain at least one account"
            ));
        }

        let mut resolved = Vec::with_capacity(self.accounts.len());

        for account in &self.accounts {
            if account.id.is_empty() {
                return Err(miette::miette!("account.id must not be empty"));
            }
            if !account.enabled {
                continue;
            }
            if account.manual_cookie.is_empty() {
                if account.username.is_empty() {
                    return Err(miette::miette!(
                        "account `{}` requires username when manual_cookie is empty",
                        account.id
                    ));
                }
                if account.password.is_empty() {
                    return Err(miette::miette!(
                        "account `{}` requires password when manual_cookie is empty",
                        account.id
                    ));
                }
            }

            let provider = app
                .providers
                .get(&account.provider)
                .cloned()
                .ok_or_else(|| {
                    miette::miette!(
                        "account `{}` references unknown provider `{}`",
                        account.id,
                        account.provider
                    )
                })?;

            resolved.push(AccountConfig {
                id: account.id.clone(),
                provider: account.provider.clone(),
                username: account.username.clone(),
                password: account.password.clone(),
                captcha: account.captcha.clone(),
                manual_cookie: account.manual_cookie.clone(),
                enabled: account.enabled,
                line_user_id: account.line_user_id.clone(),
                provider_config: ProviderConfig {
                    base_url: if provider.base_url.is_empty() {
                        app.api.base_url.clone()
                    } else {
                        provider.base_url.clone()
                    },
                    ..provider
                },
                request_timeout_secs: app.api.request_timeout_secs,
            });
        }

        if resolved.is_empty() {
            return Err(miette::miette!(
                "no enabled accounts found in accounts.toml"
            ));
        }

        Ok(resolved)
    }
}

fn validate_provider(_provider_name: &str, _provider: &ProviderConfig) -> Result<()> {
    Ok(())
}

impl std::fmt::Display for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AppConfig {{ providers: {}, api.base_url: {:?}, line_bot.enabled: {}, brute_force.concurrency: {} }}",
            self.providers.len(),
            self.api.base_url,
            self.line_bot.enabled,
            self.brute_force.concurrency,
        )
    }
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_config() -> String {
        r#"
[api]
base_url = "https://elearn2.fju.edu.tw"

[line_bot]
enabled = false
channel_secret = ""
channel_access_token = ""
admin_user_id = ""

[radar]
[brute_force]
[qrcode]
[logging]
[monitor]

[providers.fju]
kind = "fju"
base_url = "https://elearn2.fju.edu.tw"
"#
        .to_string()
    }

    #[test]
    fn test_load_minimal_config() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(make_config().as_bytes()).unwrap();
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.providers.contains_key("fju"));
        assert_eq!(cfg.api.base_url, "https://elearn2.fju.edu.tw");
    }

    #[test]
    fn test_load_accounts_and_resolve_provider() {
        let mut config_file = NamedTempFile::new().unwrap();
        config_file.write_all(make_config().as_bytes()).unwrap();
        let cfg = AppConfig::load(config_file.path()).unwrap();

        let mut accounts_file = NamedTempFile::new().unwrap();
        accounts_file
            .write_all(
                br#"
[[accounts]]
id = "user-a"
provider = "fju"
username = "alice"
password = "secret"
"#,
            )
            .unwrap();

        let accounts = AccountsFile::load(accounts_file.path())
            .unwrap()
            .resolve(&cfg)
            .unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, "user-a");
        assert_eq!(accounts[0].base_url(), "https://elearn2.fju.edu.tw");
    }

    #[test]
    fn test_account_manual_cookie_short_circuits_credentials() {
        let mut config_file = NamedTempFile::new().unwrap();
        config_file.write_all(make_config().as_bytes()).unwrap();
        let cfg = AppConfig::load(config_file.path()).unwrap();

        let mut accounts_file = NamedTempFile::new().unwrap();
        accounts_file
            .write_all(
                br#"
[[accounts]]
id = "user-a"
provider = "fju"
username = ""
password = ""
manual_cookie = "SESSION=abc"
"#,
            )
            .unwrap();

        let accounts = AccountsFile::load(accounts_file.path())
            .unwrap()
            .resolve(&cfg)
            .unwrap();
        assert!(accounts[0].use_manual_cookie());
    }
}
