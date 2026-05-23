//! Integration tests for `services::opencode_models` parser. The live
//! `fetch_opencode_models` is not exercised here (it would hit the network);
//! instead we feed `parse_models_json` a static fixture mirroring the
//! `models.dev/api.json` shape and lock down the flattening contract so a
//! future schema drift fails CI rather than the picker.

use wisetree::services::opencode_models::{parse_models_json, OpencodeModel};

/// Mirrors a trimmed slice of the actual `https://models.dev/api.json`
/// response — two providers, three models, plus one provider with no models
/// at all (to make sure that branch produces zero rows rather than panics).
const FIXTURE: &str = r#"{
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
    },
    "ghost-provider": {
        "name": "Ghost",
        "models": {}
    }
}"#;

#[test]
fn fixture_flattens_into_sorted_pair_list() {
    let parsed = parse_models_json(FIXTURE).expect("fixture parses");
    let pairs: Vec<String> = parsed.iter().map(OpencodeModel::pair).collect();
    assert_eq!(
        pairs,
        vec![
            "anthropic/claude-sonnet-4-5".to_string(),
            "openai/gpt-4o".to_string(),
            "openai/gpt-4o-mini".to_string(),
        ]
    );
}

#[test]
fn fixture_preserves_provider_and_model_display_names() {
    let parsed = parse_models_json(FIXTURE).expect("fixture parses");
    let anthropic = parsed
        .iter()
        .find(|m| m.provider_id == "anthropic")
        .expect("anthropic present");
    assert_eq!(anthropic.provider_name, "Anthropic");
    assert_eq!(anthropic.model_name, "Claude Sonnet 4.5");

    let mini = parsed
        .iter()
        .find(|m| m.model_id == "gpt-4o-mini")
        .expect("gpt-4o-mini present");
    assert_eq!(mini.provider_name, "OpenAI");
    assert_eq!(mini.model_name, "GPT-4o mini");
}

#[test]
fn provider_with_empty_models_map_yields_no_entries() {
    let parsed = parse_models_json(FIXTURE).expect("fixture parses");
    assert!(
        parsed.iter().all(|m| m.provider_id != "ghost-provider"),
        "ghost-provider should contribute zero rows when its models map is empty"
    );
}

#[test]
fn malformed_json_surfaces_as_err() {
    let result = parse_models_json("not json at all");
    assert!(result.is_err(), "garbage input must not panic or return Ok");
}
