//! 帳號設定模組

use miette::Result;
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, ProviderConfig};

// ─── 解析後的帳號設定 ─────────────────────────────────────────────────────────

/// 解析完成、可直接執行的帳號設定
#[derive(Debug, Clone, Serialize)]
pub struct AccountConfig {
    pub id: String,
    pub provider: String,
    pub username: String,
    pub password: String,
    pub enabled: bool,
    pub line_user_id: String,
    pub discord_user_id: String,
    pub provider_config: ProviderConfig,
    pub request_timeout_secs: u64,
}

impl AccountConfig {
    pub fn base_url(&self) -> &str {
        &self.provider_config.base_url
    }

    pub fn display_name(&self) -> &str {
        &self.id
    }
}

// ─── 原始帳號設定 ─────────────────────────────────────────────────────────────

/// 原始帳號設定（來自資料庫）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawAccountConfig {
    pub id: String,
    pub provider: String,
    pub username: String,
    pub password: String,

    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default)]
    pub line_user_id: String,

    #[serde(default)]
    pub discord_user_id: String,
}

fn default_true() -> bool {
    true
}

// ─── 帳號清單 ─────────────────────────────────────────────────────────────────

/// 帳號清單容器，用於解析成可執行的 `AccountConfig`
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccountsFile {
    #[serde(default)]
    pub accounts: Vec<RawAccountConfig>,
}

impl AccountsFile {
    pub fn resolve(&self, app: &AppConfig) -> Result<Vec<AccountConfig>> {
        if self.accounts.is_empty() {
            return Err(miette::miette!(
                "accounts database must contain at least one account"
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
            if account.username.is_empty() {
                return Err(miette::miette!(
                    "account `{}`: username must not be empty",
                    account.id
                ));
            }
            if account.password.is_empty() {
                return Err(miette::miette!(
                    "account `{}`: password must not be empty",
                    account.id
                ));
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

            let request_timeout_secs = provider.api.request_timeout_secs;
            resolved.push(AccountConfig {
                id: account.id.clone(),
                provider: account.provider.clone(),
                username: account.username.clone(),
                password: account.password.clone(),
                enabled: account.enabled,
                line_user_id: account.line_user_id.clone(),
                discord_user_id: account.discord_user_id.clone(),
                provider_config: provider,
                request_timeout_secs,
            });
        }

        if resolved.is_empty() {
            return Err(miette::miette!(
                "no enabled accounts found in accounts database"
            ));
        }

        Ok(resolved)
    }
}
