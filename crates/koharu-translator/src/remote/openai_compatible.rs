// https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create

use reqwest::Client;
use url::Url;

use super::chat_completions::{self, Target};
use crate::{GenerationConfig, Model, Provider, Result, TranslationRequest};

const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(default)]
pub struct OpenAiCompatibleConfig {
    pub base_url: Option<Url>,
}

impl Default for OpenAiCompatibleConfig {
    fn default() -> Self {
        Self {
            base_url: Some(
                Url::parse(DEFAULT_BASE_URL).expect("default OpenAI-compatible URL is valid"),
            ),
        }
    }
}

fn target(config: &OpenAiCompatibleConfig) -> Target {
    Target::resolve(
        Provider::OpenAiCompatible,
        config.base_url.as_ref(),
        DEFAULT_BASE_URL,
        None,
        false,
    )
}

pub(super) async fn translate(
    client: &Client,
    config: &OpenAiCompatibleConfig,
    model: &str,
    generation: &GenerationConfig,
    request: &TranslationRequest,
) -> Result<Vec<String>> {
    chat_completions::translate(client, &target(config), model, generation, request).await
}

pub(super) async fn models(client: &Client, config: &OpenAiCompatibleConfig) -> Result<Vec<Model>> {
    let target = target(config);
    Ok(chat_completions::catalog(client, &target)
        .await?
        .into_iter()
        .map(|entry| entry.into_model(Provider::OpenAiCompatible))
        .collect())
}
