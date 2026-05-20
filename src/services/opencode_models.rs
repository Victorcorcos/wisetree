//! Discovery of free models exposed by the opencode CLI.
//!
//! Runs `opencode models opencode --verbose` once per process and keeps
//! every entry whose `cost.input` and `cost.output` are both zero — that's
//! the same criterion opencode itself uses for the "Free" badge in its
//! own UI. Falls back to a hardcoded list of currently-known free models
//! when the CLI is missing, the command fails, or the verbose output
//! doesn't include any free entry, so the Settings → Dashboard → useAi
//! cycle always has at least one valid model to land on.

use std::process::Command;
use std::sync::{OnceLock, RwLock};

/// Provider id the dashboard merge resolver targets. Every "free" model we
/// surface in the cycle is namespaced under this provider.
pub const OPENCODE_PROVIDER: &str = "opencode";

/// A single opencode model surfaced in the Settings → Dashboard → useAi
/// cycle. `id` is what the opencode CLI accepts on `--model` (matches the
/// `provider/id` header in `opencode models --verbose`); `label` is the
/// human-friendly string the UI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpencodeModel {
    pub id: String,
    pub label: String,
}

/// Hardcoded fallback list of free opencode models. Mirrors the verbose
/// `opencode models opencode` output as of May 2026, used when the CLI is
/// unavailable so the cycle still has something usable. Keep ordered by
/// id — that's the order the UI cycles through.
pub const FALLBACK_FREE_MODELS: &[(&str, &str)] = &[
    ("opencode/big-pickle", "Big Pickle — Free"),
    ("opencode/deepseek-v4-flash-free", "DeepSeek V4 Flash Free"),
    ("opencode/nemotron-3-super-free", "Nemotron 3 Super Free"),
    ("opencode/qwen3.6-plus-free", "Qwen3.6 Plus Free"),
];

/// Snapshot of free opencode models for this process.
///
/// Returns the hardcoded fallback list synchronously so startup never blocks
/// on the opencode CLI (which takes ~1s to respond on a cold boot). On the
/// first call we also kick off a background thread that runs
/// `opencode models opencode --verbose` and, on success, replaces the
/// returned slice for subsequent callers. The settings cycle picks up the
/// live list the next time it renders.
pub fn free_models() -> &'static [OpencodeModel] {
    static LIVE: RwLock<Option<&'static [OpencodeModel]>> = RwLock::new(None);
    static FALLBACK: OnceLock<Vec<OpencodeModel>> = OnceLock::new();
    static DISCOVERY: OnceLock<()> = OnceLock::new();

    DISCOVERY.get_or_init(|| {
        std::thread::spawn(|| {
            if let Some(live) = discover_from_cli() {
                let leaked: &'static [OpencodeModel] = Box::leak(live.into_boxed_slice());
                if let Ok(mut guard) = LIVE.write() {
                    *guard = Some(leaked);
                }
            }
        });
    });

    if let Ok(guard) = LIVE.read() {
        if let Some(live) = *guard {
            return live;
        }
    }

    FALLBACK.get_or_init(fallback_models).as_slice()
}

/// Owned copy of the fallback list. Public so tests and callers that need
/// a deterministic baseline (without spawning the CLI) can use it.
pub fn fallback_models() -> Vec<OpencodeModel> {
    FALLBACK_FREE_MODELS
        .iter()
        .map(|(id, label)| OpencodeModel {
            id: (*id).to_string(),
            label: (*label).to_string(),
        })
        .collect()
}

fn discover_from_cli() -> Option<Vec<OpencodeModel>> {
    let output = Command::new("opencode")
        .args(["models", OPENCODE_PROVIDER, "--verbose"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let models = parse_verbose_output(&stdout);
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

/// Parse opencode's `models --verbose` output. Each model entry is a
/// `<provider>/<id>` header line followed by a pretty-printed JSON object
/// (one block per model, blocks separated only by the next header line).
/// We keep entries with `cost.input == 0 && cost.output == 0` — that's
/// opencode's own "free" criterion.
pub fn parse_verbose_output(stdout: &str) -> Vec<OpencodeModel> {
    let mut models = Vec::new();
    let mut current_id: Option<String> = None;
    let mut json_buf = String::new();
    let mut depth: i32 = 0;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if depth == 0 {
            if trimmed.starts_with('{') {
                json_buf.clear();
                json_buf.push_str(line);
                json_buf.push('\n');
                depth = count_braces(line);
                if depth == 0 {
                    if let Some(id) = current_id.take() {
                        if let Some(model) = parse_model_block(&id, &json_buf) {
                            models.push(model);
                        }
                    }
                }
            } else if !trimmed.is_empty() {
                current_id = Some(trimmed.to_string());
            }
            continue;
        }

        json_buf.push_str(line);
        json_buf.push('\n');
        depth += count_braces(line);
        if depth == 0 {
            if let Some(id) = current_id.take() {
                if let Some(model) = parse_model_block(&id, &json_buf) {
                    models.push(model);
                }
            }
            json_buf.clear();
        }
    }

    models
}

fn count_braces(line: &str) -> i32 {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for c in line.chars() {
        if esc {
            esc = false;
            continue;
        }
        if in_str {
            match c {
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    depth
}

fn parse_model_block(id: &str, json: &str) -> Option<OpencodeModel> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let cost = value.get("cost")?;
    let input_cost = cost.get("input").and_then(|v| v.as_f64()).unwrap_or(1.0);
    let output_cost = cost.get("output").and_then(|v| v.as_f64()).unwrap_or(1.0);
    if input_cost != 0.0 || output_cost != 0.0 {
        return None;
    }
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(id);
    Some(OpencodeModel {
        id: id.to_string(),
        label: label_for(name),
    })
}

/// Build the label shown in the cycle. opencode's `name` usually already
/// includes "Free" (e.g. `Qwen3.6 Plus Free`); for entries that don't
/// (`Big Pickle`), append a clear suffix so the toast and the settings
/// rect both call out the no-cost backend.
fn label_for(name: &str) -> String {
    if name.to_lowercase().contains("free") {
        name.to_string()
    } else {
        format!("{name} — Free")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "opencode/big-pickle
{
  \"id\": \"big-pickle\",
  \"name\": \"Big Pickle\",
  \"cost\": { \"input\": 0, \"output\": 0 }
}
opencode/paid-model
{
  \"id\": \"paid-model\",
  \"name\": \"Paid Model\",
  \"cost\": { \"input\": 1.5, \"output\": 3.0 }
}
opencode/qwen3.6-plus-free
{
  \"id\": \"qwen3.6-plus-free\",
  \"name\": \"Qwen3.6 Plus Free\",
  \"cost\": { \"input\": 0, \"output\": 0 }
}
";

    #[test]
    fn parse_verbose_output_keeps_only_free_models() {
        let models = parse_verbose_output(SAMPLE);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["opencode/big-pickle", "opencode/qwen3.6-plus-free"],
            "expected only free models in order, got {ids:?}"
        );
    }

    #[test]
    fn parse_verbose_output_appends_free_suffix_when_name_lacks_it() {
        let models = parse_verbose_output(SAMPLE);
        let pickle = models
            .iter()
            .find(|m| m.id == "opencode/big-pickle")
            .expect("big-pickle present");
        assert_eq!(pickle.label, "Big Pickle — Free");

        let qwen = models
            .iter()
            .find(|m| m.id == "opencode/qwen3.6-plus-free")
            .expect("qwen present");
        // Already says Free → leave the label intact.
        assert_eq!(qwen.label, "Qwen3.6 Plus Free");
    }

    #[test]
    fn parse_verbose_output_ignores_garbage_blocks() {
        let stdout = "opencode/unparseable\n{ not json\n}\nopencode/big-pickle\n{ \"cost\": { \"input\": 0, \"output\": 0 }, \"name\": \"Big Pickle\" }\n";
        let models = parse_verbose_output(stdout);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "opencode/big-pickle");
    }

    #[test]
    fn fallback_list_is_non_empty() {
        let fb = fallback_models();
        assert!(!fb.is_empty());
        assert!(fb.iter().all(|m| m.id.starts_with("opencode/")));
    }
}
