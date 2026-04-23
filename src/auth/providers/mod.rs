use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{cookie::Jar, Client};

use crate::account::AccountConfig;

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

pub(crate) fn get_auth_flow(provider_name: &str) -> Box<dyn AuthFlow> {
    match provider_name {
        "fju" => fju::create_flow(),
        other => panic!("unknown provider: {other}"),
    }
}
