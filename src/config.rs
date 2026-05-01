//! 設定管理模組

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{NaiveTime, Weekday};
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
    /// Providers (學校/Tronclass 系統) 設定
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Adapters 設定
    #[serde(default)]
    pub adapters: AdapterConfig,
    /// 時區設定
    #[serde(default)]
    pub time: TimeConfig,
    /// 日誌設定
    pub logging: LoggingConfig,
    /// 監控設定
    pub monitor: MonitorConfig,
}

// ─── 子設定結構 ───────────────────────────────────────────────────────────────

// ======== Provider ========
/// Provider 設定結構
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: ProviderKind,
    /// Tronclass API (學校系統) Base URL
    #[serde(default)]
    pub base_url: String,
    /// API 設定
    pub api: ApiConfig,
    /// 雷達簽到設定
    pub radar: RadarConfig,
    /// 數字爆破設定
    pub brute_force: BruteForceConfig,
    /// QR Code 設定
    pub qrcode: QrCodeConfig,
    /// 課堂時段與輪詢時間設定
    #[serde(default)]
    pub schedule: PollingScheduleConfig,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::default(),
            base_url: default_base_url(),
            api: ApiConfig {
                base_url: default_base_url(),
                poll_interval_secs: default_poll_interval(),
                request_timeout_secs: default_request_timeout(),
            },
            radar: RadarConfig {
                default_coords: default_coords(),
                accuracy: default_accuracy(),
                altitude: 0,
            },
            brute_force: BruteForceConfig {
                concurrency: default_concurrency(),
                request_delay_ms: 0,
                transient_failure_threshold: default_transient_failure_threshold(),
                transient_failure_ratio: default_transient_failure_ratio(),
                cooldown_secs: default_cooldown_secs(),
                max_cooldowns: default_max_cooldowns(),
                min_concurrency: default_min_concurrency(),
            },
            qrcode: QrCodeConfig {
                scanner_base_url: default_scanner_base_url(),
                scan_timeout_secs: default_scan_timeout(),
            },
            schedule: PollingScheduleConfig::default(),
        }
    }
}

impl ProviderConfig {
    fn apply_kind_defaults(&mut self) {
        if self.schedule.periods.is_empty() && matches!(self.kind, ProviderKind::FJU) {
            self.schedule.periods = default_fju_schedule_periods();
        }
        if self.schedule.rest_weekdays.is_empty() && matches!(self.kind, ProviderKind::FJU) {
            self.schedule.rest_weekdays = vec![ScheduleWeekday::Sun];
        }
    }
}

/// Provider 種類
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Tronclass 原生系統
    Tronclass,
    /// 輔仁大學系統（接受 "fju" 或 "f_j_u"）
    #[serde(alias = "fju")]
    FJU,
    /// 淡江大學系統（接受 "tku" 或 "t_k_u"）
    #[serde(alias = "tku")]
    TKU,
}

impl Default for ProviderKind {
    fn default() -> Self {
        Self::Tronclass
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
    /// 預設雷達點名座標陣列
    #[serde(default = "default_coords")]
    pub default_coords: Vec<[f64; 2]>,
    /// 精確度
    #[serde(default = "default_accuracy")]
    pub accuracy: u32,
    /// 海拔高度
    #[serde(default)]
    pub altitude: i32,
}

/// 數字爆破設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BruteForceConfig {
    /// 最大並行數
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// 請求延遲（毫秒）
    #[serde(default)]
    pub request_delay_ms: u64,
    /// 自上次冷卻後可容忍的異常失敗次數
    #[serde(default = "default_transient_failure_threshold")]
    pub transient_failure_threshold: usize,
    /// 單批異常失敗比例門檻（0.0 < ratio <= 1.0）
    #[serde(default = "default_transient_failure_ratio")]
    pub transient_failure_ratio: f32,
    /// 異常失敗觸發後的冷卻時間（秒）
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
    /// 最大冷卻次數；0 表示達門檻時立即失敗
    #[serde(default = "default_max_cooldowns")]
    pub max_cooldowns: usize,
    /// 動態降速後的最低並行數
    #[serde(default = "default_min_concurrency")]
    pub min_concurrency: usize,
}

/// QR Code 簽到設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QrCodeConfig {
    #[serde(default = "default_scanner_base_url")]
    pub scanner_base_url: String,

    #[serde(default = "default_scan_timeout")]
    pub scan_timeout_secs: u64,
}

/// Provider 輪詢時段設定
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PollingScheduleConfig {
    /// 允許輪詢的課堂時段
    #[serde(default)]
    pub periods: Vec<String>,
    /// 休息日；這些星期不會進行自動輪詢
    #[serde(default)]
    pub rest_weekdays: Vec<ScheduleWeekday>,
}

impl PollingScheduleConfig {
    pub fn is_configured(&self) -> bool {
        !self.periods.is_empty()
    }

    pub fn is_rest_day(&self, weekday: Weekday) -> bool {
        self.rest_weekdays.iter().any(|day| day.matches(weekday))
    }
}

/// 星期設定（用於課表休息日）
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleWeekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl ScheduleWeekday {
    pub fn matches(self, weekday: Weekday) -> bool {
        matches!(
            (self, weekday),
            (ScheduleWeekday::Mon, Weekday::Mon)
                | (ScheduleWeekday::Tue, Weekday::Tue)
                | (ScheduleWeekday::Wed, Weekday::Wed)
                | (ScheduleWeekday::Thu, Weekday::Thu)
                | (ScheduleWeekday::Fri, Weekday::Fri)
                | (ScheduleWeekday::Sat, Weekday::Sat)
                | (ScheduleWeekday::Sun, Weekday::Sun)
        )
    }
}

// ======== Adapter ========
/// Adapter 設定結構
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AdapterConfig {
    #[serde(default)]
    pub line_bot: LineBotConfig,
    #[serde(default)]
    pub discord: DiscordBotConfig,
}

/// Line Bot 設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LineBotConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default)]
    pub channel_secret: String,

    #[serde(default)]
    pub channel_access_token: String,

    #[serde(default = "default_webhook_port")]
    pub webhook_port: u16,

    #[serde(default = "default_webhook_path")]
    pub webhook_path: String,

    #[serde(default)]
    pub public_base_url: String,

    #[serde(default)]
    pub admin_user_id: String,
}

impl Default for LineBotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel_secret: String::new(),
            channel_access_token: String::new(),
            webhook_port: default_webhook_port(),
            webhook_path: default_webhook_path(),
            public_base_url: String::new(),
            admin_user_id: String::new(),
        }
    }
}

/// Discord Bot 設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiscordBotConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default)]
    pub bot_token: String,

    #[serde(default)]
    pub admin_user_id: String,

    #[serde(default)]
    pub admin_channel_id: String,

    #[serde(default)]
    pub public_base_url: String,

    #[serde(default = "default_register_commands")]
    pub register_commands: bool,

    #[serde(default)]
    pub guild_ids: Vec<String>,
}

impl Default for DiscordBotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            admin_user_id: String::new(),
            admin_channel_id: String::new(),
            public_base_url: String::new(),
            register_commands: default_register_commands(),
            guild_ids: vec![],
        }
    }
}

// ======== Time ========
/// 時區設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TimeConfig {
    /// 執行時區；可設為 `local`、`UTC` 或 IANA 時區名稱（例如 `Asia/Taipei`）
    #[serde(default = "default_timezone")]
    pub timezone: String,
}

impl Default for TimeConfig {
    fn default() -> Self {
        Self {
            timezone: default_timezone(),
        }
    }
}

// ======== Logging ========
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

// ======== Monitor ========
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
fn default_base_url() -> String {
    "https://www.tronclass.com".to_string()
}
fn default_poll_interval() -> u64 {
    10
}
fn default_request_timeout() -> u64 {
    30
}
fn default_coords() -> Vec<[f64; 2]> {
    vec![[24.3, 118.0], [24.6, 118.2]]
}
fn default_accuracy() -> u32 {
    35
}
fn default_concurrency() -> usize {
    200
}
fn default_transient_failure_threshold() -> usize {
    50
}
fn default_transient_failure_ratio() -> f32 {
    0.20
}
fn default_cooldown_secs() -> u64 {
    10
}
fn default_max_cooldowns() -> usize {
    3
}
fn default_min_concurrency() -> usize {
    10
}
fn default_scanner_base_url() -> String {
    "https://elearn2.fju.edu.tw/scanner-jumper".to_string()
}
fn default_scan_timeout() -> u64 {
    60
}
fn default_fju_schedule_periods() -> Vec<String> {
    vec!["07:10~17:30".to_string()]
}
fn default_webhook_port() -> u16 {
    8080
}
fn default_webhook_path() -> String {
    "/webhook".to_string()
}
fn default_register_commands() -> bool {
    true
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_timezone() -> String {
    "local".to_string()
}
fn default_log_file_path() -> String {
    "tronclass_rollcall.log".to_string()
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

// ─── AppConfig 實作 ───────────────────────────────────────────────────────────────────
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
                config::Environment::with_prefix("TRONCLASS_ROLLCALL")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to load config from `{}`", path.display()))?;

        let mut app = cfg
            .try_deserialize::<AppConfig>()
            .into_diagnostic()
            .wrap_err("Failed to deserialize config")?;

        for provider in app.providers.values_mut() {
            provider.apply_kind_defaults();
        }

        Ok(app)
    }

    pub fn load_default() -> Result<Self> {
        Self::load("config.toml")
    }

    pub fn validate(&self) -> Result<()> {
        if self.providers.is_empty() {
            return Err(miette::miette!("至少需要一個 provider"));
        }

        for (name, p) in &self.providers {
            if p.api.base_url.is_empty() {
                return Err(miette::miette!("providers.{name}.api.base_url 不可為空"));
            }
            if p.api.poll_interval_secs == 0 {
                return Err(miette::miette!(
                    "providers.{name}.api.poll_interval_secs 必須大於 0"
                ));
            }
            if p.api.request_timeout_secs == 0 {
                return Err(miette::miette!(
                    "providers.{name}.api.request_timeout_secs 必須大於 0"
                ));
            }
            validate_provider_schedule(name, &p.schedule)?;
            if p.brute_force.concurrency == 0 {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.concurrency 必須大於 0"
                ));
            }
            if p.brute_force.transient_failure_threshold == 0 {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.transient_failure_threshold 必須大於 0"
                ));
            }
            if !(0.0..=1.0).contains(&p.brute_force.transient_failure_ratio)
                || p.brute_force.transient_failure_ratio == 0.0
            {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.transient_failure_ratio 必須大於 0 且小於等於 1"
                ));
            }
            if p.brute_force.max_cooldowns > 0 && p.brute_force.cooldown_secs == 0 {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.cooldown_secs 在啟用冷卻時必須大於 0"
                ));
            }
            if p.brute_force.min_concurrency == 0 {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.min_concurrency 必須大於 0"
                ));
            }
            if p.brute_force.min_concurrency > p.brute_force.concurrency {
                return Err(miette::miette!(
                    "providers.{name}.brute_force.min_concurrency 不可大於 concurrency"
                ));
            }
            for (i, coords) in p.radar.default_coords.iter().enumerate() {
                let [lat, lon] = coords;
                if !(-90.0..=90.0).contains(lat) {
                    return Err(miette::miette!(
                        "providers.{name}.radar.default_coords[{i}] 緯度 {lat} 超出範圍（-90 ~ 90）"
                    ));
                }
                if !(-180.0..=180.0).contains(lon) {
                    return Err(miette::miette!(
                        "providers.{name}.radar.default_coords[{i}] 經度 {lon} 超出範圍（-180 ~ 180）"
                    ));
                }
            }
        }

        if self.adapters.line_bot.enabled {
            if self.adapters.line_bot.channel_secret.is_empty() {
                return Err(miette::miette!(
                    "adapters.line_bot.channel_secret 不可為空（line_bot.enabled = true）"
                ));
            }
            if self.adapters.line_bot.channel_access_token.is_empty() {
                return Err(miette::miette!(
                    "adapters.line_bot.channel_access_token 不可為空（line_bot.enabled = true）"
                ));
            }
            if self.adapters.line_bot.admin_user_id.is_empty() {
                return Err(miette::miette!(
                    "adapters.line_bot.admin_user_id 不可為空（line_bot.enabled = true）"
                ));
            }
        }

        if !self.adapters.line_bot.public_base_url.trim().is_empty() {
            let parsed =
                url::Url::parse(self.adapters.line_bot.public_base_url.trim()).map_err(|_| {
                    miette::miette!("adapters.line_bot.public_base_url 必須是有效的 http(s) URL")
                })?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return Err(miette::miette!(
                    "adapters.line_bot.public_base_url 必須是有效的 http(s) URL"
                ));
            }
        }

        if self.adapters.discord.enabled {
            if self.adapters.discord.bot_token.trim().is_empty() {
                return Err(miette::miette!(
                    "adapters.discord.bot_token 不可為空（discord.enabled = true）"
                ));
            }
            if self.adapters.discord.admin_user_id.trim().is_empty() {
                return Err(miette::miette!(
                    "adapters.discord.admin_user_id 不可為空（discord.enabled = true）"
                ));
            }
            if self.adapters.discord.admin_channel_id.trim().is_empty() {
                return Err(miette::miette!(
                    "adapters.discord.admin_channel_id 不可為空（discord.enabled = true）"
                ));
            }
            validate_discord_id(
                "adapters.discord.admin_user_id",
                &self.adapters.discord.admin_user_id,
            )?;
            validate_discord_id(
                "adapters.discord.admin_channel_id",
                &self.adapters.discord.admin_channel_id,
            )?;
            for (i, guild_id) in self.adapters.discord.guild_ids.iter().enumerate() {
                validate_discord_id(&format!("adapters.discord.guild_ids[{i}]"), guild_id)?;
            }
        }
        if !self.adapters.discord.public_base_url.trim().is_empty() {
            validate_http_url(
                "adapters.discord.public_base_url",
                self.adapters.discord.public_base_url.trim(),
            )?;
        }

        if self.monitor.retry_interval_secs == 0 {
            return Err(miette::miette!("monitor.retry_interval_secs 必須大於 0"));
        }
        if self.monitor.max_failures_before_reauth == 0 {
            return Err(miette::miette!(
                "monitor.max_failures_before_reauth 必須大於 0"
            ));
        }
        if self.time.timezone.trim().is_empty() {
            return Err(miette::miette!("time.timezone 不可為空"));
        }

        Ok(())
    }
}

impl std::fmt::Display for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AppConfig {{ providers: {}, line_bot.enabled: {}, discord.enabled: {} }}",
            self.providers.len(),
            self.adapters.line_bot.enabled,
            self.adapters.discord.enabled,
        )
    }
}

fn validate_discord_id(field: &str, value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Err(miette::miette!("{field} 必須是 Discord snowflake 數字 ID"));
    }

    Ok(())
}

fn validate_http_url(field: &str, value: &str) -> Result<()> {
    let parsed =
        url::Url::parse(value).map_err(|_| miette::miette!("{field} 必須是有效的 http(s) URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(miette::miette!("{field} 必須是有效的 http(s) URL"));
    }

    Ok(())
}

pub(crate) fn parse_hhmm(value: &str) -> Result<NaiveTime> {
    let (hour_str, minute_str) = value
        .split_once(':')
        .ok_or_else(|| miette::miette!("時間格式必須為 HH:MM，收到 `{value}`"))?;
    let hour: u32 = hour_str
        .parse()
        .into_diagnostic()
        .wrap_err_with(|| format!("無法解析小時：`{value}`"))?;
    let minute: u32 = minute_str
        .parse()
        .into_diagnostic()
        .wrap_err_with(|| format!("無法解析分鐘：`{value}`"))?;

    NaiveTime::from_hms_opt(hour, minute, 0)
        .ok_or_else(|| miette::miette!("時間 `{value}` 超出有效範圍"))
}

pub(crate) fn parse_schedule_period(period: &str) -> Result<(NaiveTime, NaiveTime)> {
    let (start, end) = period
        .split_once('~')
        .ok_or_else(|| miette::miette!("時段格式必須為 HH:MM~HH:MM，收到 `{period}`"))?;
    let start = parse_hhmm(start.trim())?;
    let end = parse_hhmm(end.trim())?;

    if start >= end {
        return Err(miette::miette!(
            "時段 `{period}` 的開始時間必須早於結束時間"
        ));
    }

    Ok((start, end))
}

fn validate_provider_schedule(name: &str, schedule: &PollingScheduleConfig) -> Result<()> {
    let mut parsed = Vec::with_capacity(schedule.periods.len());
    let mut periods = std::collections::BTreeSet::new();
    let mut rest_weekdays = std::collections::BTreeSet::new();

    for weekday in &schedule.rest_weekdays {
        if !rest_weekdays.insert(*weekday) {
            return Err(miette::miette!(
                "providers.{name}.schedule.rest_weekdays 中有重複的星期設定"
            ));
        }
    }

    for (i, period) in schedule.periods.iter().enumerate() {
        if period.trim().is_empty() {
            return Err(miette::miette!(
                "providers.{name}.schedule.periods[{i}] 不可為空"
            ));
        }

        if !periods.insert(period.clone()) {
            return Err(miette::miette!(
                "providers.{name}.schedule.periods 中的 `{period}` 重複"
            ));
        }

        let (start, end) = parse_schedule_period(period)
            .wrap_err_with(|| format!("providers.{name}.schedule.periods[{i}] 格式錯誤"))?;

        parsed.push((start, end, period.clone()));
    }

    parsed.sort_by_key(|(start, _, _)| *start);
    for pair in parsed.windows(2) {
        let (_, prev_end, prev_label) = &pair[0];
        let (next_start, _, next_label) = &pair[1];
        if next_start < prev_end {
            return Err(miette::miette!(
                "providers.{name}.schedule.periods 中的 `{prev_label}` 與 `{next_label}` 時段重疊"
            ));
        }
    }

    Ok(())
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_toml(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    // Minimal valid TOML: logging + monitor are required; providers is optional.
    fn minimal_toml() -> &'static str {
        r#"
[logging]
[monitor]

[providers.default]
[providers.default.api]
[providers.default.radar]
[providers.default.brute_force]
[providers.default.qrcode]
"#
    }

    // ── Load ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_load_minimal_config() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.providers.contains_key("default"));
        assert_eq!(cfg.providers.len(), 1);
    }

    #[test]
    fn test_load_nonexistent_file_returns_error() {
        assert!(AppConfig::load("/nonexistent/config.toml").is_err());
    }

    #[test]
    fn test_load_invalid_toml_returns_error() {
        let f = write_toml("not valid toml !!! [[[");
        assert!(AppConfig::load(f.path()).is_err());
    }

    // ── Defaults ─────────────────────────────────────────────────────────────

    #[test]
    fn test_adapters_section_is_optional() {
        // Config without [adapters] loads fine; line_bot defaults to disabled.
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(!cfg.adapters.line_bot.enabled);
    }

    #[test]
    fn test_logging_defaults_when_section_empty() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.logging.level, "info");
        assert!(!cfg.logging.log_to_file);
        assert_eq!(cfg.logging.log_file_path, "tronclass_rollcall.log");
    }

    #[test]
    fn test_time_defaults_when_section_missing() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.time.timezone, "local");
    }

    #[test]
    fn test_monitor_defaults_when_section_empty() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.monitor.startup_delay_secs, 3);
        assert_eq!(cfg.monitor.retry_interval_secs, 30);
        assert_eq!(cfg.monitor.max_failures_before_reauth, 5);
    }

    #[test]
    fn test_provider_config_defaults() {
        let p = ProviderConfig::default();
        assert_eq!(p.kind, ProviderKind::Tronclass);
        assert_eq!(p.base_url, "https://www.tronclass.com");
        assert_eq!(p.api.base_url, "https://www.tronclass.com");
        assert_eq!(p.api.poll_interval_secs, 10);
        assert_eq!(p.api.request_timeout_secs, 30);
        assert_eq!(p.radar.accuracy, 35);
        assert_eq!(p.radar.altitude, 0);
        assert!(!p.radar.default_coords.is_empty());
        assert_eq!(p.brute_force.concurrency, 200);
        assert_eq!(p.brute_force.request_delay_ms, 0);
        assert_eq!(p.brute_force.transient_failure_threshold, 50);
        assert!((p.brute_force.transient_failure_ratio - 0.20).abs() < f32::EPSILON);
        assert_eq!(p.brute_force.cooldown_secs, 10);
        assert_eq!(p.brute_force.max_cooldowns, 3);
        assert_eq!(p.brute_force.min_concurrency, 10);
        assert_eq!(p.qrcode.scan_timeout_secs, 60);
        assert!(!p.qrcode.scanner_base_url.is_empty());
        assert!(p.schedule.periods.is_empty());
        assert!(p.schedule.rest_weekdays.is_empty());
    }

    #[test]
    fn test_provider_kind_default_is_tronclass() {
        assert_eq!(ProviderKind::default(), ProviderKind::Tronclass);
    }

    #[test]
    fn test_line_bot_config_defaults() {
        let lb = LineBotConfig::default();
        assert!(!lb.enabled);
        assert!(lb.channel_secret.is_empty());
        assert!(lb.channel_access_token.is_empty());
        assert!(lb.admin_user_id.is_empty());
        assert_eq!(lb.webhook_port, 8080);
        assert_eq!(lb.webhook_path, "/webhook");
        assert!(lb.public_base_url.is_empty());
    }

    #[test]
    fn test_discord_bot_config_defaults() {
        let discord = DiscordBotConfig::default();
        assert!(!discord.enabled);
        assert!(discord.bot_token.is_empty());
        assert!(discord.admin_user_id.is_empty());
        assert!(discord.admin_channel_id.is_empty());
        assert!(discord.public_base_url.is_empty());
        assert!(discord.register_commands);
        assert!(discord.guild_ids.is_empty());
    }

    // ── Line Bot loading ──────────────────────────────────────────────────────

    #[test]
    fn test_load_with_line_bot_enabled() {
        let toml = format!(
            "{}\n\
             [adapters.line_bot]\n\
             enabled = true\n\
             channel_secret = \"mysecret\"\n\
             channel_access_token = \"mytoken\"\n\
             public_base_url = \"https://example.ngrok-free.app\"\n\
             admin_user_id = \"U12345\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.adapters.line_bot.enabled);
        assert_eq!(cfg.adapters.line_bot.channel_secret, "mysecret");
        assert_eq!(cfg.adapters.line_bot.channel_access_token, "mytoken");
        assert_eq!(
            cfg.adapters.line_bot.public_base_url,
            "https://example.ngrok-free.app"
        );
        assert_eq!(cfg.adapters.line_bot.admin_user_id, "U12345");
        // port and path should keep their defaults
        assert_eq!(cfg.adapters.line_bot.webhook_port, 8080);
        assert_eq!(cfg.adapters.line_bot.webhook_path, "/webhook");
    }

    #[test]
    fn test_load_with_custom_webhook_settings() {
        let toml = format!(
            "{}\n\
             [adapters.line_bot]\n\
             enabled = false\n\
             webhook_port = 9090\n\
             webhook_path = \"/line\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.adapters.line_bot.webhook_port, 9090);
        assert_eq!(cfg.adapters.line_bot.webhook_path, "/line");
    }

    #[test]
    fn test_load_with_discord_enabled() {
        let toml = format!(
            "{}\n\
             [adapters.discord]\n\
             enabled = true\n\
             bot_token = \"token\"\n\
             admin_user_id = \"123\"\n\
             admin_channel_id = \"456\"\n\
             public_base_url = \"https://discord-scanner.example.test\"\n\
             register_commands = false\n\
             guild_ids = [\"789\"]\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.adapters.discord.enabled);
        assert_eq!(cfg.adapters.discord.bot_token, "token");
        assert_eq!(cfg.adapters.discord.admin_user_id, "123");
        assert_eq!(cfg.adapters.discord.admin_channel_id, "456");
        assert_eq!(
            cfg.adapters.discord.public_base_url,
            "https://discord-scanner.example.test"
        );
        assert!(!cfg.adapters.discord.register_commands);
        assert_eq!(cfg.adapters.discord.guild_ids, vec!["789".to_string()]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_empty_timezone() {
        let toml = format!(
            "{}\n\
             [time]\n\
             timezone = \"\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.validate().is_err());
    }

    // ── Provider loading ──────────────────────────────────────────────────────

    #[test]
    fn test_load_provider_with_custom_base_url() {
        let toml = r#"
[logging]
[monitor]

[providers.fju]
base_url = "https://elearn2.fju.edu.tw"
[providers.fju.api]
poll_interval_secs = 5
[providers.fju.radar]
accuracy = 50
[providers.fju.brute_force]
concurrency = 100
[providers.fju.qrcode]
scan_timeout_secs = 30
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let p = &cfg.providers["fju"];
        assert_eq!(p.base_url, "https://elearn2.fju.edu.tw");
        assert_eq!(p.api.poll_interval_secs, 5);
        assert_eq!(p.radar.accuracy, 50);
        assert_eq!(p.brute_force.concurrency, 100);
        assert_eq!(p.qrcode.scan_timeout_secs, 30);
    }

    #[test]
    fn test_fju_kind_applies_default_schedule_and_sunday_rest() {
        let toml = r#"
[logging]
[monitor]

[providers.fju]
kind = "fju"
[providers.fju.api]
[providers.fju.radar]
[providers.fju.brute_force]
[providers.fju.qrcode]
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let p = &cfg.providers["fju"];
        assert_eq!(p.schedule.periods, vec!["07:10~17:30".to_string()]);
        assert!(p.schedule.rest_weekdays.contains(&ScheduleWeekday::Sun));
    }

    #[test]
    fn test_load_multiple_providers() {
        let toml = r#"
[logging]
[monitor]

[providers.school_a]
[providers.school_a.api]
[providers.school_a.radar]
[providers.school_a.brute_force]
[providers.school_a.qrcode]

[providers.school_b]
[providers.school_b.api]
[providers.school_b.radar]
[providers.school_b.brute_force]
[providers.school_b.qrcode]
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert_eq!(cfg.providers.len(), 2);
        assert!(cfg.providers.contains_key("school_a"));
        assert!(cfg.providers.contains_key("school_b"));
    }

    // ── Validation ────────────────────────────────────────────────────────────

    #[test]
    fn test_validate_fails_when_no_providers() {
        let f = write_toml("[logging]\n[monitor]\n");
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.providers.is_empty());
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("provider"));
    }

    #[test]
    fn test_validate_passes_for_minimal_valid_config() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_fails_when_concurrency_is_zero() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
concurrency = 0
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("concurrency"), "got: {err}");
    }

    #[test]
    fn test_validate_fails_for_invalid_transient_failure_ratio() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
transient_failure_ratio = 0.0
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("transient_failure_ratio"),
            "got: {err}"
        );
    }

    #[test]
    fn test_validate_fails_for_transient_failure_ratio_above_one() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
transient_failure_ratio = 1.1
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("transient_failure_ratio"),
            "got: {err}"
        );
    }

    #[test]
    fn test_validate_fails_for_zero_transient_failure_threshold() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
transient_failure_threshold = 0
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("transient_failure_threshold"),
            "got: {err}"
        );
    }

    #[test]
    fn test_validate_fails_for_zero_cooldown_when_cooldowns_enabled() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
cooldown_secs = 0
max_cooldowns = 1
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("cooldown_secs"), "got: {err}");
    }

    #[test]
    fn test_validate_allows_zero_max_cooldowns() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
max_cooldowns = 0
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_fails_for_invalid_min_concurrency() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
[providers.p.brute_force]
concurrency = 10
min_concurrency = 20
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("min_concurrency"), "got: {err}");
    }

    #[test]
    fn test_validate_fails_for_invalid_latitude() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
base_url = "https://example.com"
[providers.p.api]
base_url = "https://example.com"
[providers.p.radar]
default_coords = [[999.0, 121.0]]
[providers.p.brute_force]
[providers.p.qrcode]
scanner_base_url = "https://example.com/scanner"
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("緯度"), "got: {err}");
    }

    #[test]
    fn test_validate_fails_for_invalid_schedule_overlap() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
[providers.p.api]
[providers.p.radar]
[providers.p.brute_force]
[providers.p.qrcode]
[providers.p.schedule]
periods = ["08:10~09:00", "08:50~09:30"]
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("重疊"), "got: {err}");
    }

    #[test]
    fn test_validate_fails_for_duplicate_rest_weekdays() {
        let toml = r#"
[logging]
[monitor]
[providers.p]
[providers.p.api]
[providers.p.radar]
[providers.p.brute_force]
[providers.p.qrcode]
[providers.p.schedule]
rest_weekdays = ["sun", "sun"]
"#;
        let f = write_toml(toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("rest_weekdays"), "got: {err}");
    }

    #[test]
    fn test_validate_fails_when_line_bot_enabled_without_credentials() {
        let toml = format!("{}\n[adapters.line_bot]\nenabled = true\n", minimal_toml());
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("channel_secret"), "got: {err}");
    }

    #[test]
    fn test_validate_passes_when_line_bot_enabled_with_credentials() {
        let toml = format!(
            "{}\n[adapters.line_bot]\nenabled = true\nchannel_secret = \"s\"\nchannel_access_token = \"t\"\nadmin_user_id = \"u\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_fails_when_discord_enabled_without_credentials() {
        let toml = format!("{}\n[adapters.discord]\nenabled = true\n", minimal_toml());
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("bot_token"), "got: {err}");
    }

    #[test]
    fn test_validate_rejects_invalid_discord_id() {
        let toml = format!(
            "{}\n[adapters.discord]\nenabled = true\nbot_token = \"t\"\nadmin_user_id = \"not-id\"\nadmin_channel_id = \"456\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("snowflake"), "got: {err}");
    }

    #[test]
    fn test_validate_rejects_invalid_public_base_url() {
        let toml = format!(
            "{}\n[adapters.line_bot]\nenabled = true\nchannel_secret = \"s\"\nchannel_access_token = \"t\"\nadmin_user_id = \"u\"\npublic_base_url = \"not a url\"\n",
            minimal_toml()
        );
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("public_base_url"), "got: {err}");
    }

    // ── Serialization ─────────────────────────────────────────────────────────

    #[test]
    fn test_provider_kind_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProviderKind::Tronclass).unwrap(),
            r#""tronclass""#
        );
        // FJU → "f_j_u" per serde snake_case rules
        assert_eq!(
            serde_json::to_string(&ProviderKind::FJU).unwrap(),
            r#""f_j_u""#
        );
        assert_eq!(
            serde_json::to_string(&ProviderKind::TKU).unwrap(),
            r#""t_k_u""#
        );
    }

    #[test]
    fn test_provider_kind_round_trips() {
        for kind in [
            ProviderKind::Tronclass,
            ProviderKind::FJU,
            ProviderKind::TKU,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: ProviderKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn test_provider_kind_accepts_short_aliases() {
        let fju: ProviderKind = serde_json::from_str(r#""fju""#).unwrap();
        assert_eq!(fju, ProviderKind::FJU);

        let tku: ProviderKind = serde_json::from_str(r#""tku""#).unwrap();
        assert_eq!(tku, ProviderKind::TKU);
    }

    // ── Display ───────────────────────────────────────────────────────────────

    #[test]
    fn test_display_shows_provider_count_and_line_bot_status() {
        let f = write_toml(minimal_toml());
        let cfg = AppConfig::load(f.path()).unwrap();
        let s = format!("{cfg}");
        assert!(s.contains("providers: 1"), "got: {s}");
        assert!(s.contains("line_bot.enabled: false"), "got: {s}");
        assert!(s.contains("discord.enabled: false"), "got: {s}");
    }

    #[test]
    fn test_display_with_line_bot_enabled() {
        let toml = format!("{}\n[adapters.line_bot]\nenabled = true\n", minimal_toml());
        let f = write_toml(&toml);
        let cfg = AppConfig::load(f.path()).unwrap();
        let s = format!("{cfg}");
        assert!(s.contains("line_bot.enabled: true"), "got: {s}");
    }
}
