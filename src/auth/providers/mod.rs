use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{cookie::Jar, Client};

use crate::config::{AccountConfig, ProviderConfig};

use super::AuthSession;

mod fju;

// ─── AuthFlow trait ───────────────────────────────────────────────────────────

#[async_trait]
pub(crate) trait AuthFlow: Send + Sync {
    async fn login(
        &self,
        client: &Client,
        cookie_jar: &Arc<Jar>,
        base_url: &str,
        account: &AccountConfig,
    ) -> miette::Result<AuthSession>;
}

// ─── Provider registry ───────────────────────────────────────────────────────

pub fn builtin_providers() -> BTreeMap<String, ProviderConfig> {
    BTreeMap::from([(String::from("fju"), fju::provider_config())])
}

pub(crate) fn get_auth_flow(provider_name: &str) -> Box<dyn AuthFlow> {
    match provider_name {
        "fju" => fju::create_flow(),
        other => panic!("unknown provider: {other}"),
    }
}
