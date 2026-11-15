// OpenCode Console serves its paid and Go models from two OpenAI-compatible
// gateways. Only a Console API key works here: free-tier models are refused for
// every harness other than OpenCode, and a Console login stays OAuth.
//
// https://opencode.ai/docs/console/harnesses
// https://opencode.ai/docs/console/go

use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Client;
use url::Url;

use super::chat_completions::{self, Target};
use crate::{GenerationConfig, Model, Provider, Result, TranslationRequest};

const DEFAULT_BASE_URL: &str = "https://opencode.ai/inference/openai/v1";

/// Console routes a conversation and reuses its prompt cache per session id,
/// and the Go gateway rejects requests that carry none.
static SESSION: LazyLock<String> = LazyLock::new(|| {
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("ses_koharu_{started:x}")
});

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(default)]
pub struct OpenCodeConfig {
    pub base_url: Option<Url>,
}

impl Default for OpenCodeConfig {
    fn default() -> Self {
        Self {
            base_url: Some(Url::parse(DEFAULT_BASE_URL).expect("default OpenCode URL is valid")),
        }
    }
}

fn target(config: &OpenCodeConfig) -> Target {
    Target::resolve(
        Provider::OpenCode,
        config.base_url.as_ref(),
        DEFAULT_BASE_URL,
        Some(("x-opencode-session", SESSION.as_str())),
        true,
    )
}

pub(super) async fn translate(
    client: &Client,
    config: &OpenCodeConfig,
    model: &str,
    generation: &GenerationConfig,
    request: &TranslationRequest,
) -> Result<Vec<String>> {
    chat_completions::translate(client, &target(config), model, generation, request).await
}

pub(super) async fn models(client: &Client, config: &OpenCodeConfig) -> Result<Vec<Model>> {
    let target = target(config);
    // Console serves its catalog to any caller but rejects a request that
    // carries an invalid key, so the listing never depends on the stored one.
    let catalog = chat_completions::public_catalog(client, &target).await?;
    // Console serves free-tier models only inside OpenCode itself; every other
    // harness gets HTTP 403, so they stay out of the picker.
    Ok(catalog
        .into_iter()
        .filter(|entry| !entry.is_free_tier())
        .map(|entry| entry.into_model(Provider::OpenCode))
        .collect())
}
