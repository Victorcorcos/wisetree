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

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const OPENCODE_MODELS_TIMEOUT: Duration = Duration::from_secs(5);
/// `opencode models --verbose` dumps the full JSON for every configured
/// provider's models, so it can run longer than the bare-list call (it may
/// refresh the models.dev cache first). Give it a roomier ceiling.
const OPENCODE_VARIANTS_TIMEOUT: Duration = Duration::from_secs(15);

/// One row in the picker: a single `provider/model` pair plus the human
/// names the picker uses for the description column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpencodeModel {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub model_name: String,
    /// `true` when models.dev marks the model as reasoning-capable. Drives
    /// whether the picker offers a "Select variant" (thinking strength) step
    /// after the model is chosen.
    #[serde(default)]
    pub reasoning: bool,
    /// The exact thinking-strength variants this model accepts, as computed by
    /// the local `opencode` CLI (`opencode models --verbose`). `None` means the
    /// local CLI doesn't know this model (an unconfigured provider), so callers
    /// fall back to a generic ladder. `Some(vec![])` is authoritative: the
    /// model takes no reasoning override even though models.dev flags it
    /// reasoning-capable (e.g. Kimi, Qwen). Weakest→strongest order is
    /// preserved from opencode's own output.
    #[serde(default)]
    pub variants: Option<Vec<String>>,
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
    #[serde(default)]
    reasoning: bool,
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
                reasoning: model.reasoning,
                // models.dev only advertises the reasoning flag, never the
                // per-model variant set. `fetch_opencode_model_variants` fills
                // this in afterwards from the local CLI.
                variants: None,
            });
        }
    }
    out.sort_by_key(|m| m.pair());
    out
}

/// Ask the locally installed `opencode` binary for the models its
/// `opencode/*` provider can actually serve right now. This is more
/// authoritative than `models.dev/api.json` for the free-model picker:
/// the upstream catalogue advertises ~17 free models, but the local CLI
/// only forwards the small subset its servers currently route — calling
/// any other "free" model fails with `Model not found: ...`. Each line
/// of stdout is one `provider/model` pair.
pub async fn fetch_free_opencode_models(binary: &Path) -> Result<Vec<String>, String> {
    let mut cmd = Command::new(binary);
    cmd.arg("models")
        .arg("opencode")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = tokio::time::timeout(OPENCODE_MODELS_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "opencode models timed out".to_string())?
        .map_err(|e| format!("spawn opencode: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "opencode models exited non-zero".to_string()
        } else {
            stderr
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_opencode_models_output(&stdout))
}

/// Pulled out so unit tests can exercise the parser without spawning a
/// subprocess. Each non-empty, non-whitespace line is treated as a
/// `provider/model` pair; we additionally filter to lines that contain
/// a `/` to defend against banner / log noise sneaking into stdout.
pub fn parse_opencode_models_output(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && line.contains('/'))
        .map(|line| line.to_string())
        .collect()
}

/// Ask the locally installed `opencode` to dump every configured model's full
/// metadata (`opencode models --verbose`) and harvest the `variants` object
/// each model carries. opencode computes these per model with provider-specific
/// heuristics (Anthropic thinking budgets, OpenAI efforts, GLM `high`/`max`, …)
/// and deliberately returns an empty set for models that take no reasoning
/// override (Kimi, Qwen, MiniMax, …). Reusing opencode's own output keeps us
/// correct without re-porting that logic — and in lock-step as opencode evolves.
///
/// Returns a `provider/model` → ordered-variant-names map. Only providers the
/// local CLI is configured/authenticated for appear; callers fall back to a
/// generic ladder for anything absent.
pub async fn fetch_opencode_model_variants(
    binary: &Path,
) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
    let mut cmd = Command::new(binary);
    cmd.arg("models")
        .arg("--verbose")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = tokio::time::timeout(OPENCODE_VARIANTS_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "opencode models --verbose timed out".to_string())?
        .map_err(|e| format!("spawn opencode: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "opencode models --verbose exited non-zero".to_string()
        } else {
            stderr
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_verbose_models_output(&stdout))
}

/// Shape of one model block in `opencode models --verbose`. We only care about
/// the identity and the variant names; everything else is ignored.
#[derive(Debug, Deserialize)]
struct VerboseModel {
    id: String,
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(default)]
    variants: OrderedKeys,
}

/// The keys of a JSON object, captured in their original insertion order.
/// opencode emits variants weakest→strongest, and the picker / cycle rely on
/// that ordering, so we can't route through a `HashMap`/`BTreeMap` (which would
/// drop or reorder the keys).
#[derive(Debug, Default)]
struct OrderedKeys(Vec<String>);

impl<'de> Deserialize<'de> for OrderedKeys {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct KeysVisitor;
        impl<'de> serde::de::Visitor<'de> for KeysVisitor {
            type Value = Vec<String>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map of variant name to settings")
            }
            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut keys = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    let _: serde::de::IgnoredAny = map.next_value()?;
                    keys.push(key);
                }
                Ok(keys)
            }
        }
        deserializer.deserialize_map(KeysVisitor).map(OrderedKeys)
    }
}

/// Split `opencode models --verbose` stdout into its per-model JSON blocks and
/// collect each model's variant names. The output interleaves a bare
/// `provider/model` line before every pretty-printed JSON object, so we skip
/// non-JSON lines and accumulate brace-balanced blocks. Each block is parsed
/// with serde, so a mis-split or schema drift degrades gracefully (that model
/// is simply dropped from the map → caller falls back to the generic ladder)
/// instead of failing the whole fetch. Pulled out for unit testing without a
/// subprocess.
pub fn parse_verbose_models_output(raw: &str) -> std::collections::HashMap<String, Vec<String>> {
    let mut out = std::collections::HashMap::new();
    let mut block = String::new();
    let mut depth: usize = 0;
    let mut in_block = false;
    for line in raw.lines() {
        if !in_block {
            // The bare `provider/model` lines carry no braces — skip until the
            // opening `{` of the next JSON object.
            if !line.contains('{') {
                continue;
            }
            in_block = true;
        }
        block.push_str(line);
        block.push('\n');
        depth += line.matches('{').count();
        depth = depth.saturating_sub(line.matches('}').count());
        if depth == 0 {
            if let Ok(model) = serde_json::from_str::<VerboseModel>(&block) {
                out.insert(
                    format!("{}/{}", model.provider_id, model.id),
                    model.variants.0,
                );
            }
            block.clear();
            in_block = false;
        }
    }
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
                    "claude-sonnet-4-5": {"name": "Claude Sonnet 4.5", "reasoning": true}
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
    fn reasoning_flag_is_parsed_and_defaults_to_false() {
        let raw = r#"{
            "openai": {
                "name": "OpenAI",
                "models": {
                    "gpt-5.4": {"name": "GPT-5.4", "reasoning": true},
                    "gpt-image-1-mini": {"name": "GPT Image 1 mini"}
                }
            }
        }"#;
        let parsed = parse_models_json(raw).expect("fixture parses");
        let reasoning: std::collections::BTreeMap<String, bool> =
            parsed.iter().map(|m| (m.pair(), m.reasoning)).collect();
        assert!(reasoning["openai/gpt-5.4"]);
        assert!(!reasoning["openai/gpt-image-1-mini"]);
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

    #[test]
    fn parse_opencode_models_output_keeps_provider_model_lines() {
        let raw = "opencode/big-pickle\nopencode/deepseek-v4-flash-free\nopencode/nemotron-3-super-free\n";
        assert_eq!(
            parse_opencode_models_output(raw),
            vec![
                "opencode/big-pickle".to_string(),
                "opencode/deepseek-v4-flash-free".to_string(),
                "opencode/nemotron-3-super-free".to_string(),
            ]
        );
    }

    #[test]
    fn parse_verbose_models_output_extracts_ordered_variants_per_model() {
        // Mirrors `opencode models --verbose`: a bare `provider/model` line
        // before each pretty-printed JSON object. Covers the three cases the
        // picker cares about — no `variants` key, an empty object (Kimi-style),
        // and a populated set whose weakest→strongest order must survive.
        let raw = r#"opencode-go/kimi-k2.7-code
{
  "id": "kimi-k2.7-code",
  "providerID": "opencode-go",
  "capabilities": { "reasoning": true },
  "variants": {}
}
opencode-go/glm-5.2
{
  "id": "glm-5.2",
  "providerID": "opencode-go",
  "variants": {
    "high": { "reasoningEffort": "high" },
    "max": { "reasoningEffort": "max" }
  }
}
opencode/big-pickle
{
  "id": "big-pickle",
  "providerID": "opencode"
}
"#;
        let map = parse_verbose_models_output(raw);
        assert_eq!(map.get("opencode-go/kimi-k2.7-code"), Some(&Vec::new()));
        assert_eq!(
            map.get("opencode-go/glm-5.2"),
            Some(&vec!["high".to_string(), "max".to_string()])
        );
        // A model without a `variants` key yields an empty list, not a miss.
        assert_eq!(map.get("opencode/big-pickle"), Some(&Vec::new()));
    }

    #[test]
    fn parse_verbose_models_output_skips_unparseable_blocks() {
        // A malformed block must be dropped without poisoning the rest.
        let raw = r#"prov/bad
{
  "id": "bad",
  not valid json
}
prov/good
{
  "id": "good",
  "providerID": "prov",
  "variants": { "low": {}, "high": {} }
}
"#;
        let map = parse_verbose_models_output(raw);
        assert!(!map.contains_key("prov/bad"));
        assert_eq!(
            map.get("prov/good"),
            Some(&vec!["low".to_string(), "high".to_string()])
        );
    }

    #[test]
    fn parse_opencode_models_output_skips_noise_lines() {
        // opencode's CLI sometimes leads with an ASCII-art banner before
        // the actual list. Filter anything without a slash so banners,
        // blank lines, and progress chatter never sneak into the picker.
        let raw = "\n   opencode banner\nopencode/big-pickle\n\n";
        assert_eq!(
            parse_opencode_models_output(raw),
            vec!["opencode/big-pickle".to_string()]
        );
    }
}
