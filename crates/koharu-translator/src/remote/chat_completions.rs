// The OpenAI Chat Completions contract, shared by every gateway Koharu reaches
// through an OpenAI-compatible endpoint.
//
// https://opencode.ai/docs/console/harnesses

use anyhow::Context;
use koharu_secrets::{ExposeSecret as _, SecretString};
use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use url::Url;

use super::send_json;
use crate::{
    GenerationConfig, Model, Provider, Result, TranslationRequest, backend::encode_image,
    display_name, prompt,
};

/// One OpenAI-compatible endpoint plus the headers it needs on every request.
pub(super) struct Target {
    provider: Provider,
    base_url: String,
    /// Extra header some gateways require, such as OpenCode's request routing.
    session: Option<(&'static str, &'static str)>,
    /// Hosted gateways cannot be used without a stored key; a local server can.
    credential_required: bool,
}

impl Target {
    pub(super) fn resolve(
        provider: Provider,
        base_url: Option<&Url>,
        default_base_url: &str,
        session: Option<(&'static str, &'static str)>,
        credential_required: bool,
    ) -> Self {
        let base_url = base_url.map_or(default_base_url, Url::as_str);
        Self {
            provider,
            base_url: base_url.trim_end_matches('/').to_owned(),
            session,
            credential_required,
        }
    }

    fn name(&self) -> &'static str {
        self.provider.into()
    }

    fn endpoint(&self, suffix: &str) -> String {
        format!("{}/{}", self.base_url, suffix.trim_start_matches('/'))
    }

    fn credential(&self) -> anyhow::Result<Option<SecretString>> {
        koharu_secrets::get(self.name())
    }

    fn authorize(&self, request: RequestBuilder, key: Option<&SecretString>) -> RequestBuilder {
        let request = match key {
            Some(key) => request.bearer_auth(key.expose_secret()),
            None => request,
        };
        match self.session {
            Some((header, value)) => request.header(header, value),
            None => request,
        }
    }
}

pub(super) async fn translate(
    client: &Client,
    target: &Target,
    model: &str,
    generation: &GenerationConfig,
    request: &TranslationRequest,
) -> Result<Vec<String>> {
    let key = target.credential()?;
    if key.is_none() && target.credential_required {
        return Err(anyhow::anyhow!("{} API key is not configured", target.name()).into());
    }
    let (system, user) = prompt::prompts(request)?;
    let user_content = match request.image.as_deref() {
        Some(image) => MessageContent::Parts(vec![
            ContentPart::Text { text: user },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: encode_image(image)?.data_url(),
                },
            },
        ]),
        None => MessageContent::Text(user),
    };
    let body = ChatRequest {
        model,
        messages: [
            Message {
                role: "system",
                content: MessageContent::Text(system),
            },
            Message {
                role: "user",
                content: user_content,
            },
        ],
        temperature: generation.temperature,
        top_p: generation.top_p,
        max_tokens: generation.max_tokens,
        frequency_penalty: generation.frequency_penalty,
        presence_penalty: generation.presence_penalty,
        reasoning_effort: generation
            .reasoning
            .map(|enabled| if enabled { "medium" } else { "none" }),
        response_format: ResponseFormat {
            kind: "json_schema",
            json_schema: JsonSchema {
                name: "manga_translation",
                strict: true,
                schema: prompt::output_schema(request.segments.len()),
            },
        },
    };
    let http = target
        .authorize(
            client.post(target.endpoint("chat/completions")),
            key.as_ref(),
        )
        .json(&body);
    let response: ChatResponse = send_json(target.name(), http).await?;
    let text = response
        .choices
        .into_iter()
        .next()
        .with_context(|| format!("{} returned no choices", target.name()))?
        .message
        .content
        .with_context(|| format!("{} returned no message content", target.name()))?;
    Ok(prompt::translations(
        target.name(),
        &text,
        &request.segments,
    )?)
}

/// Read the catalog with the stored key, for gateways that need it.
pub(super) async fn catalog(client: &Client, target: &Target) -> Result<Vec<CatalogEntry>> {
    let key = target.credential()?;
    fetch(client, target, key.as_ref()).await
}

/// Read a catalog that is public. Some gateways reject a request that carries
/// an invalid key while serving the same catalog without one, so a stale
/// credential must not turn an available catalog into an error.
pub(super) async fn public_catalog(client: &Client, target: &Target) -> Result<Vec<CatalogEntry>> {
    fetch(client, target, None).await
}

async fn fetch(
    client: &Client,
    target: &Target,
    key: Option<&SecretString>,
) -> Result<Vec<CatalogEntry>> {
    let http = target.authorize(client.get(target.endpoint("models")), key);
    let response: ModelsResponse = send_json(target.name(), http).await?;
    Ok(response.data.into_iter().map(CatalogEntry::from).collect())
}

/// One catalog row before a provider decides which rows it can actually use.
pub(super) struct CatalogEntry {
    id: String,
    /// A gateway that serves the row for free refuses it outside its own app.
    free_tier: bool,
}

impl CatalogEntry {
    pub(super) const fn is_free_tier(&self) -> bool {
        self.free_tier
    }

    pub(super) fn into_model(self, provider: Provider) -> Model {
        Model {
            provider,
            name: display_name(&self.id),
            model: Some(self.id),
            quantizations: Vec::new(),
            vision: true,
            reasoning: true,
        }
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [Message; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
    response_format: ResponseFormat,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
    json_schema: JsonSchema,
}

#[derive(Serialize)]
struct JsonSchema {
    name: &'static str,
    strict: bool,
    schema: serde_json::Value,
}

#[derive(Serialize)]
struct Message {
    role: &'static str,
    content: MessageContent,
}

#[derive(Serialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Serialize)]
struct ImageUrl {
    url: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ListedModel>,
}

#[derive(Deserialize)]
struct ListedModel {
    id: String,
    #[serde(default)]
    cost: Option<Cost>,
}

impl From<ListedModel> for CatalogEntry {
    fn from(model: ListedModel) -> Self {
        let free_tier = model
            .cost
            .as_ref()
            .and_then(Cost::output_price)
            .is_some_and(|price| price <= 0.0);
        Self {
            id: model.id,
            free_tier,
        }
    }
}

#[derive(Deserialize)]
struct Cost {
    #[serde(default)]
    text: Option<CostText>,
}

impl Cost {
    fn output_price(&self) -> Option<f64> {
        self.text.as_ref()?.output.as_ref()?.price
    }
}

#[derive(Deserialize)]
struct CostText {
    #[serde(default)]
    output: Option<Price>,
}

#[derive(Deserialize)]
struct Price {
    #[serde(default)]
    price: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(provider: Provider, base_url: &str) -> Target {
        Target::resolve(
            provider,
            Some(&Url::parse(base_url).unwrap()),
            "unused",
            None,
            false,
        )
    }

    #[test]
    fn endpoint_preserves_base_path() {
        let target = target(Provider::OpenAiCompatible, "http://localhost:1234/v1");
        assert_eq!(target.endpoint("models"), "http://localhost:1234/v1/models");
    }

    #[test]
    fn endpoint_keeps_the_gateway_session_path() {
        let target = target(
            Provider::OpenCode,
            "https://opencode.ai/inference/go/openai/v1/",
        );
        assert_eq!(
            target.endpoint("chat/completions"),
            "https://opencode.ai/inference/go/openai/v1/chat/completions"
        );
    }

    #[test]
    fn serializes_compatible_request_contract() {
        let body = ChatRequest {
            model: "model",
            messages: [
                Message {
                    role: "system",
                    content: MessageContent::Text("system".to_owned()),
                },
                Message {
                    role: "user",
                    content: MessageContent::Text("user".to_owned()),
                },
            ],
            temperature: None,
            top_p: None,
            max_tokens: Some(1024),
            frequency_penalty: None,
            presence_penalty: None,
            reasoning_effort: Some("none"),
            response_format: ResponseFormat {
                kind: "json_schema",
                json_schema: JsonSchema {
                    name: "manga_translation",
                    strict: true,
                    schema: prompt::output_schema(2),
                },
            },
        };
        let value = serde_json::to_value(body).unwrap();

        assert_eq!(value["max_tokens"], 1024);
        assert_eq!(value["reasoning_effort"], "none");
        assert_eq!(value["response_format"]["type"], "json_schema");
        assert_eq!(value["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            value["response_format"]["json_schema"]["schema"]["properties"]["translations"]["maxItems"],
            2
        );
    }

    #[test]
    fn reads_a_model_catalog_entry() {
        let response: ModelsResponse = serde_json::from_str(
            r#"{"object":"list","data":[{"id":"claude-opus-4-5","object":"model","owned_by":"opencode","name":"Claude Opus 4.5"}]}"#,
        )
        .unwrap();

        assert_eq!(response.data[0].id, "claude-opus-4-5");
    }

    #[test]
    fn marks_a_zero_priced_entry_as_free_tier() {
        let response: ModelsResponse = serde_json::from_str(
            r#"{"data":[
                {"id":"deepseek-v4.1-flash-free","cost":{"text":{"input":{"price":0},"output":{"price":0}}}},
                {"id":"deepseek-v4.1-flash","cost":{"text":{"input":{"price":0.3},"output":{"price":1.2}}}},
                {"id":"no-pricing"}
            ]}"#,
        )
        .unwrap();
        let entries = response
            .data
            .into_iter()
            .map(CatalogEntry::from)
            .collect::<Vec<_>>();

        assert!(entries[0].is_free_tier());
        assert!(!entries[1].is_free_tier());
        assert!(
            !entries[2].is_free_tier(),
            "an unknown price is not evidence of a free tier"
        );
    }

    #[test]
    fn serializes_text_before_an_attached_image() {
        let content = MessageContent::Parts(vec![
            ContentPart::Text {
                text: "translate".to_owned(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/jpeg;base64,image".to_owned(),
                },
            },
        ]);
        let value = serde_json::to_value(content).unwrap();

        assert_eq!(
            value[0],
            serde_json::json!({ "type": "text", "text": "translate" })
        );
        assert_eq!(value[1]["type"], "image_url");
        assert_eq!(value[1]["image_url"]["url"], "data:image/jpeg;base64,image");
    }
}
