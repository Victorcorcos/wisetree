//! Fetches the public model catalogue at <https://models.dev/api.json> — the
//! same data source opencode itself reads from — and flattens it into a sorted
//! `Vec<OpencodeModel>`. Powers the "Select AI provider/model" picker in the
//! Settings screen.
//!
//! The endpoint is a JSON object keyed by provider id. Each provider has a
//! `name` and a `models` map keyed by model id. We treat missing fields
//! defensively (an absent provider name falls back to the id, an absent
//! `models` map yields zero entries) so a schema drift upstream doesn't crash
//! the picker.

use std::time::Duration;

use serde::{Deserialize, Serialize};

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One row in the picker: a single `provider/model` pair plus the human
/// names the picker uses for the description column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpencodeModel {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub model_name: String,
}

impl OpencodeModel {
    /// `provider/model` — the exact string passed to `opencode run -m`.
    pub fn pair(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

#[derive(Debug, Deserialize)]
struct ProviderEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    models: std::collections::BTreeMap<String, ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    #[serde(default)]
    name: Option<String>,
}

/// Hit `models.dev/api.json` and return the catalogue as a flat, sorted list.
/// Errors are returned as `String` (matching `services::update` style) so the
/// caller can show them verbatim in the picker's error state.
pub async fn fetch_opencode_models() -> Result<Vec<OpencodeModel>, String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(MODELS_DEV_URL)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let body: std::collections::BTreeMap<String, ProviderEntry> =
        resp.json().await.map_err(|e| e.to_string())?;
    Ok(flatten_providers(body))
}

fn flatten_providers(
    providers: std::collections::BTreeMap<String, ProviderEntry>,
) -> Vec<OpencodeModel> {
    let mut out = Vec::new();
    for (provider_id, provider) in providers {
        let provider_name = provider.name.unwrap_or_else(|| provider_id.clone());
        for (model_id, model) in provider.models {
            let model_name = model.name.unwrap_or_else(|| model_id.clone());
            out.push(OpencodeModel {
                provider_id: provider_id.clone(),
                provider_name: provider_name.clone(),
                model_id,
                model_name,
            });
        }
    }
    out.sort_by(|a, b| a.pair().cmp(&b.pair()));
    out
}

/// Test-only helper: run the same flattening logic against a raw JSON string.
/// Kept `pub` so the integration test crate can reuse it without re-deriving
/// the schema.
pub fn parse_models_json(raw: &str) -> Result<Vec<OpencodeModel>, String> {
    let providers: std::collections::BTreeMap<String, ProviderEntry> =
        serde_json::from_str(raw).map_err(|e| e.to_string())?;
    Ok(flatten_providers(providers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_and_sorts_provider_model_pairs() {
        let raw = r#"{
            "openai": {
                "name": "OpenAI",
                "models": {
                    "gpt-4o": {"name": "GPT-4o"},
                    "gpt-4o-mini": {"name": "GPT-4o mini"}
                }
            },
            "anthropic": {
                "name": "Anthropic",
                "models": {
                    "claude-sonnet-4-5": {"name": "Claude Sonnet 4.5"}
                }
            }
        }"#;
        let parsed = parse_models_json(raw).expect("fixture parses");
        let pairs: Vec<String> = parsed.iter().map(OpencodeModel::pair).collect();
        assert_eq!(
            pairs,
            vec![
                "anthropic/claude-sonnet-4-5".to_string(),
                "openai/gpt-4o".to_string(),
                "openai/gpt-4o-mini".to_string(),
            ]
        );
        assert_eq!(parsed[0].provider_name, "Anthropic");
        assert_eq!(parsed[0].model_name, "Claude Sonnet 4.5");
    }

    #[test]
    fn provider_without_name_falls_back_to_provider_id() {
        let raw = r#"{
            "weirdprovider": {
                "models": {
                    "m1": {}
                }
            }
        }"#;
        let parsed = parse_models_json(raw).expect("fixture parses");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].provider_name, "weirdprovider");
        assert_eq!(parsed[0].model_name, "m1");
    }

    #[test]
    fn provider_without_models_yields_nothing() {
        let raw = r#"{
            "anthropic": {"name": "Anthropic"}
        }"#;
        let parsed = parse_models_json(raw).expect("fixture parses");
        assert!(parsed.is_empty());
    }

    #[test]
    fn empty_object_is_an_empty_list() {
        let parsed = parse_models_json("{}").expect("fixture parses");
        assert!(parsed.is_empty());
    }

    #[test]
    fn malformed_json_returns_err() {
        let parsed = parse_models_json("not json");
        assert!(parsed.is_err());
    }
}
