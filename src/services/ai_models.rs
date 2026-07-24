//! Harness-specific model thinking capabilities.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;

use serde::Deserialize;
use tokio::process::Command;

/// Codex's bundled catalogue uses model slugs while settings persist canonical
/// `openai/<slug>` pairs. Parsing is kept separate from process execution so it
/// can be exercised without an installed CLI.
pub fn parse_codex_bundled_models(raw: &str) -> HashMap<String, Vec<String>> {
    // Real `codex debug models --bundled` output nests each level as
    // `{"effort": "low", "description": "..."}`; accept a bare string too in
    // case a future/older CLI simplifies the shape.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ReasoningLevel {
        Name(String),
        Detailed { effort: String },
    }
    impl ReasoningLevel {
        fn into_name(self) -> String {
            match self {
                Self::Name(name) => name,
                Self::Detailed { effort } => effort,
            }
        }
    }

    #[derive(Deserialize)]
    struct Model {
        #[serde(alias = "slug", alias = "model")]
        id: String,
        #[serde(default)]
        supported_reasoning_levels: Vec<ReasoningLevel>,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Catalogue {
        List(Vec<Model>),
        Wrapped { models: Vec<Model> },
    }

    let Ok(catalogue) = serde_json::from_str::<Catalogue>(raw) else {
        return HashMap::new();
    };
    let models = match catalogue {
        Catalogue::List(models) | Catalogue::Wrapped { models } => models,
    };
    models
        .into_iter()
        .map(|model| {
            (
                format!("openai/{}", model.id),
                model
                    .supported_reasoning_levels
                    .into_iter()
                    .map(ReasoningLevel::into_name)
                    .collect(),
            )
        })
        .collect()
}

/// Extract the documented `--effort` choices from Claude's locally installed
/// help text. Failure deliberately returns no choices: an unknown CLI must not
/// be presented as supporting a guessed effort ladder.
///
/// `--help` wraps the level list onto the continuation line(s) below the flag
/// (e.g. `--effort <level>` then, indented, `(low, medium, high, xhigh, max)`),
/// so the scan continues past the flag's own line until the next `--` flag.
pub fn parse_claude_effort_levels(help: &str) -> Vec<String> {
    let lines: Vec<&str> = help.lines().collect();
    let Some(start) = lines.iter().position(|line| line.contains("--effort")) else {
        return Vec::new();
    };
    let block = lines[start..]
        .iter()
        .take_while(|line| {
            line.trim_start().starts_with("--effort") || { !line.trim_start().starts_with("--") }
        })
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let known = ["low", "medium", "high", "xhigh", "max", "ultracode"];
    known
        .into_iter()
        .filter(|level| {
            block
                .split(|c: char| !c.is_ascii_alphabetic())
                .any(|word| word == *level)
        })
        .map(str::to_string)
        .collect()
}

pub async fn fetch_codex_reasoning_levels(
    binary: &Path,
) -> Result<HashMap<String, Vec<String>>, String> {
    let output = Command::new(binary)
        .args(["debug", "models", "--bundled"])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| format!("spawn codex: {error}"))?;
    if !output.status.success() {
        return Err("codex debug models --bundled exited non-zero".to_string());
    }
    Ok(parse_codex_bundled_models(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

pub async fn fetch_claude_effort_levels(binary: &Path) -> Result<Vec<String>, String> {
    let output = Command::new(binary)
        .arg("--help")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| format!("spawn claude: {error}"))?;
    if !output.status.success() {
        return Err("claude --help exited non-zero".to_string());
    }
    Ok(parse_claude_effort_levels(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_reasoning_levels() {
        let levels = parse_codex_bundled_models(
            r#"{"models":[{"slug":"gpt-5.6-sol","supported_reasoning_levels":["low","high","ultra"]}]}"#,
        );
        assert_eq!(levels["openai/gpt-5.6-sol"], ["low", "high", "ultra"]);
    }

    #[test]
    fn parses_codex_reasoning_levels_from_real_bundled_shape() {
        // Real `codex debug models --bundled` nests each level as an object
        // with an "effort" field, not a bare string.
        let levels = parse_codex_bundled_models(
            r#"{"models":[{"slug":"gpt-5.6-sol","supported_reasoning_levels":[
                {"effort":"low","description":"fast"},
                {"effort":"high","description":"deeper"}
            ]}]}"#,
        );
        assert_eq!(levels["openai/gpt-5.6-sol"], ["low", "high"]);
    }

    #[test]
    fn parses_only_advertised_claude_efforts() {
        assert_eq!(
            parse_claude_effort_levels("  --effort <low|medium|high|xhigh|max>"),
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert!(parse_claude_effort_levels("--model <model>").is_empty());
    }

    #[test]
    fn parses_claude_efforts_wrapped_onto_the_continuation_line() {
        // Real `claude --help` wraps the level list onto the line below the
        // flag itself, indented under the description column.
        let help = "  --effort <level>                      Effort level for the current session\n\
                     \x20                                       (low, medium, high, xhigh, max)\n\
                     \x20 --exclude-dynamic-system-prompt-sections\n";
        assert_eq!(
            parse_claude_effort_levels(help),
            ["low", "medium", "high", "xhigh", "max"]
        );
    }
}
