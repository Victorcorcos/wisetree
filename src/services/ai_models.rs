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
    #[derive(Deserialize)]
    struct Model {
        #[serde(alias = "slug", alias = "model")]
        id: String,
        #[serde(default)]
        supported_reasoning_levels: Vec<String>,
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
                model.supported_reasoning_levels,
            )
        })
        .collect()
}

/// Extract the documented `--effort` choices from Claude's locally installed
/// help text. Failure deliberately returns no choices: an unknown CLI must not
/// be presented as supporting a guessed effort ladder.
pub fn parse_claude_effort_levels(help: &str) -> Vec<String> {
    let Some(line) = help.lines().find(|line| line.contains("--effort")) else {
        return Vec::new();
    };
    let known = ["low", "medium", "high", "xhigh", "max", "ultracode"];
    known
        .into_iter()
        .filter(|level| {
            line.split(|c: char| !c.is_ascii_alphabetic())
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
    fn parses_only_advertised_claude_efforts() {
        assert_eq!(
            parse_claude_effort_levels("  --effort <low|medium|high|xhigh|max>"),
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert!(parse_claude_effort_levels("--model <model>").is_empty());
    }
}
