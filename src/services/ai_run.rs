//! Provider-neutral AI command construction and captured execution.
//!
//! Prompts are always passed with `Command::arg`, never a shell, so content is
//! preserved verbatim regardless of quotes, substitutions, backticks, or newlines.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::config::schema::{AiHarness, AiModelConfig};
use crate::errors::{AiErrorCode, Result, WisetreeError};

const CLAUDE_STREAMING_MIN_VERSION: (u64, u64, u64) = (2, 1, 214);
pub const DEFAULT_ACTIVITY_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRunMode {
    Interactive,
    Captured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiPermission {
    Plan,
    Implement,
}

#[derive(Debug, Clone)]
pub struct AiRunRequest {
    pub slot: String,
    pub config: AiModelConfig,
    pub prompt: String,
    pub cwd: PathBuf,
    pub mode: AiRunMode,
    pub permission: AiPermission,
    pub timeout: Duration,
    pub activity_limit: usize,
    /// Optional OpenCode session title used to correlate its own token
    /// telemetry. Other harnesses must report usage themselves.
    pub session_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiCommand {
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiCapturedRun {
    pub activity: Vec<String>,
    pub transcript: String,
}

#[derive(Debug, Clone)]
pub struct AiRunner {
    opencode_binary: PathBuf,
    codex_binary: PathBuf,
    claude_binary: PathBuf,
}

impl Default for AiRunner {
    fn default() -> Self {
        Self {
            opencode_binary: PathBuf::from("opencode"),
            codex_binary: PathBuf::from("codex"),
            claude_binary: PathBuf::from("claude"),
        }
    }
}

impl AiRunner {
    pub fn with_binary(mut self, harness: AiHarness, binary: PathBuf) -> Self {
        match harness {
            AiHarness::OpenCode => self.opencode_binary = binary,
            AiHarness::Codex => self.codex_binary = binary,
            AiHarness::ClaudeCode => self.claude_binary = binary,
        }
        self
    }

    pub fn command(&self, request: &AiRunRequest) -> Result<AiCommand> {
        let harness = request.config.harness;
        let model = canonical_model(harness, &request.config.model).ok_or_else(|| {
            self.error(
                request,
                AiErrorCode::UnavailableModel,
                "the configured model is unavailable for this harness",
            )
        })?;
        let binary = self.binary(harness).to_path_buf();
        if !binary_available(&binary) {
            return Err(self.error(
                request,
                AiErrorCode::MissingBinary,
                "CLI binary is not available on PATH",
            ));
        }
        let effort = request.config.thinking.trim();
        let mut args = match (harness, request.mode) {
            (AiHarness::OpenCode, AiRunMode::Interactive) => {
                let mut args = vec![
                    "--prompt".into(),
                    request.prompt.clone(),
                    "-m".into(),
                    model,
                ];
                if request.permission == AiPermission::Plan {
                    args.extend(["--agent".into(), "plan".into()]);
                }
                args.push(request.cwd.to_string_lossy().to_string());
                args
            }
            (AiHarness::OpenCode, AiRunMode::Captured) => {
                let mut args = vec!["run".into(), request.prompt.clone(), "-m".into(), model];
                if request.permission == AiPermission::Plan {
                    args.extend(["--agent".into(), "plan".into()]);
                }
                if !effort.is_empty() {
                    args.extend(["--variant".into(), effort.into()]);
                }
                if let Some(title) = &request.session_title {
                    args.extend(["--title".into(), title.clone()]);
                }
                args
            }
            (AiHarness::Codex, AiRunMode::Interactive) => {
                let mut args = vec!["--model".into(), model, request.prompt.clone()];
                if request.permission == AiPermission::Plan {
                    args.extend(["--sandbox".into(), "read-only".into()]);
                } else {
                    args.push("--full-auto".into());
                }
                if !effort.is_empty() {
                    args.extend([
                        "--config".into(),
                        format!("model_reasoning_effort={effort}"),
                    ]);
                }
                args
            }
            (AiHarness::Codex, AiRunMode::Captured) => {
                let mut args = vec![
                    "exec".into(),
                    "--model".into(),
                    model,
                    request.prompt.clone(),
                ];
                if request.permission == AiPermission::Plan {
                    args.extend(["--sandbox".into(), "read-only".into()]);
                } else {
                    args.push("--full-auto".into());
                }
                if !effort.is_empty() {
                    args.extend([
                        "--config".into(),
                        format!("model_reasoning_effort={effort}"),
                    ]);
                }
                args
            }
            (AiHarness::ClaudeCode, AiRunMode::Interactive) => {
                let mut args = vec!["--model".into(), model, request.prompt.clone()];
                args.extend(permission_args(request.permission));
                if !effort.is_empty() {
                    args.extend(["--effort".into(), effort.into()]);
                }
                args
            }
            (AiHarness::ClaudeCode, AiRunMode::Captured) => {
                let mut args = vec![
                    "-p".into(),
                    request.prompt.clone(),
                    "--model".into(),
                    model,
                    "--output-format".into(),
                    "text".into(),
                ];
                args.extend(permission_args(request.permission));
                if !effort.is_empty() {
                    args.extend(["--effort".into(), effort.into()]);
                }
                args
            }
        };
        // Keep the prompt as the sole payload argument. The individual CLI
        // syntaxes above intentionally place it directly after their prompt flag.
        debug_assert!(args.iter().any(|arg| arg == &request.prompt));
        Ok(AiCommand {
            binary,
            args: std::mem::take(&mut args),
            cwd: request.cwd.clone(),
        })
    }

    pub async fn preflight(&self, request: &AiRunRequest) -> Result<AiCommand> {
        let command = self.command(request)?;
        if request.config.harness == AiHarness::ClaudeCode && request.mode == AiRunMode::Captured {
            let version = cli_version(&command.binary).await.map_err(|_| {
                self.error(
                    request,
                    AiErrorCode::UnsupportedVersion,
                    "could not determine Claude Code version required for captured execution",
                )
            })?;
            if version < CLAUDE_STREAMING_MIN_VERSION {
                return Err(self.error(
                    request,
                    AiErrorCode::UnsupportedVersion,
                    "Claude Code 2.1.214 or newer is required for captured execution",
                ));
            }
        }
        Ok(command)
    }

    pub async fn run_captured(
        &self,
        request: &AiRunRequest,
        activity_tx: Option<mpsc::UnboundedSender<String>>,
        mut cancel: oneshot::Receiver<()>,
    ) -> Result<AiCapturedRun> {
        let command = self.preflight(request).await?;
        let mut child = Command::new(&command.binary)
            .args(&command.args)
            .current_dir(&command.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                self.error(
                    request,
                    AiErrorCode::MissingBinary,
                    format!("could not start CLI: {error}"),
                )
            })?;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let (line_tx, mut line_rx) = mpsc::unbounded_channel();
        tokio::spawn(read_lines(stdout, true, line_tx.clone()));
        tokio::spawn(read_lines(stderr, false, line_tx.clone()));
        drop(line_tx);
        let limit = request.activity_limit.max(1);
        let mut activity = VecDeque::with_capacity(limit);
        let mut transcript = String::new();
        let mut stderr = String::new();
        let completion = async {
            while let Some((is_stdout, line)) = line_rx.recv().await {
                if activity.len() == limit {
                    activity.pop_front();
                }
                activity.push_back(line.clone());
                if let Some(tx) = &activity_tx {
                    let _ = tx.send(line.clone());
                }
                if is_stdout {
                    transcript.push_str(&line);
                    transcript.push('\n');
                } else {
                    stderr.push_str(&line);
                    stderr.push('\n');
                }
            }
            child
                .wait()
                .await
                .map_err(|error| self.error(request, AiErrorCode::Failed, error.to_string()))
        };
        let status = tokio::select! {
            status = tokio::time::timeout(request.timeout, completion) => status.map_err(|_| self.error(request, AiErrorCode::TimedOut, "captured run timed out"))??,
            _ = &mut cancel => { let _ = child.kill().await; return Err(self.error(request, AiErrorCode::Cancelled, "captured run was cancelled")); }
        };
        if !status.success() {
            return Err(self.error(
                request,
                classify_cli_failure(&stderr),
                if stderr.trim().is_empty() {
                    "captured run exited non-zero".to_string()
                } else {
                    stderr.trim().to_string()
                },
            ));
        }
        let transcript = transcript.trim().to_string();
        if transcript.is_empty() {
            return Err(self.error(
                request,
                AiErrorCode::MissingOutput,
                "captured run completed without final output",
            ));
        }
        Ok(AiCapturedRun {
            activity: activity.into_iter().collect(),
            transcript,
        })
    }

    fn binary(&self, harness: AiHarness) -> &Path {
        match harness {
            AiHarness::OpenCode => &self.opencode_binary,
            AiHarness::Codex => &self.codex_binary,
            AiHarness::ClaudeCode => &self.claude_binary,
        }
    }
    fn error(
        &self,
        request: &AiRunRequest,
        code: AiErrorCode,
        message: impl Into<String>,
    ) -> WisetreeError {
        WisetreeError::ai(
            &request.slot,
            request.config.harness.wire_name(),
            code,
            message,
        )
    }
}

fn permission_args(permission: AiPermission) -> Vec<String> {
    match permission {
        AiPermission::Plan => vec!["--permission-mode".into(), "plan".into()],
        AiPermission::Implement => vec!["--permission-mode".into(), "acceptEdits".into()],
    }
}

fn canonical_model(harness: AiHarness, model: &str) -> Option<String> {
    let (provider, model) = model.trim().split_once('/').unwrap_or(("", model.trim()));
    if model.is_empty() {
        return None;
    }
    match harness {
        AiHarness::OpenCode => Some(if provider.is_empty() {
            model.into()
        } else {
            format!("{provider}/{model}")
        }),
        AiHarness::Codex if provider == "openai" || provider.is_empty() => Some(model.into()),
        AiHarness::ClaudeCode if provider == "anthropic" || provider.is_empty() => {
            Some(model.into())
        }
        _ => None,
    }
}

fn binary_available(binary: &Path) -> bool {
    std::process::Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn read_lines<R>(stream: R, is_stdout: bool, tx: mpsc::UnboundedSender<(bool, String)>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let _ = tx.send((is_stdout, line));
    }
}

fn classify_cli_failure(stderr: &str) -> AiErrorCode {
    let stderr = stderr.to_ascii_lowercase();
    if [
        "not logged",
        "not authenticated",
        "login required",
        "authentication",
    ]
    .iter()
    .any(|needle| stderr.contains(needle))
    {
        AiErrorCode::MissingAuthentication
    } else if ["model", "not found", "unavailable", "does not exist"]
        .iter()
        .any(|needle| stderr.contains(needle))
    {
        AiErrorCode::UnavailableModel
    } else if stderr.contains("effort")
        || stderr.contains("reasoning")
        || stderr.contains("variant")
    {
        AiErrorCode::UnsupportedEffort
    } else if stderr.contains("unknown option")
        || stderr.contains("unknown flag")
        || stderr.contains("unrecognized option")
    {
        AiErrorCode::UnsupportedFlag
    } else {
        AiErrorCode::Failed
    }
}

async fn cli_version(binary: &Path) -> std::result::Result<(u64, u64, u64), ()> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .await
        .map_err(|_| ())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.split_whitespace().find_map(parse_version).ok_or(())?;
    Ok(version)
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = text.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
    });
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn request(harness: AiHarness, mode: AiRunMode) -> AiRunRequest {
        AiRunRequest {
            slot: "dashboard.ai.fix.plan".into(),
            config: AiModelConfig {
                model: match harness {
                    AiHarness::OpenCode => "openai/gpt-5".into(),
                    AiHarness::Codex => "openai/gpt-5".into(),
                    AiHarness::ClaudeCode => "anthropic/claude-sonnet".into(),
                },
                thinking: "high".into(),
                harness,
            },
            prompt: "say \"$HOME\" `now`\nnext".into(),
            cwd: PathBuf::from("/tmp/worktree"),
            mode,
            permission: AiPermission::Plan,
            timeout: Duration::from_secs(1),
            activity_limit: 2,
            session_title: None,
        }
    }

    #[test]
    fn each_harness_builds_a_single_literal_prompt_argument() {
        for harness in [AiHarness::OpenCode, AiHarness::Codex, AiHarness::ClaudeCode] {
            let runner = AiRunner::default().with_binary(harness, PathBuf::from("true"));
            let req = request(harness, AiRunMode::Captured);
            let command = runner.command(&req).unwrap();
            assert_eq!(
                command
                    .args
                    .iter()
                    .filter(|arg| *arg == &req.prompt)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn translates_canonical_models_and_permission_policy() {
        let runner = AiRunner::default().with_binary(AiHarness::Codex, PathBuf::from("true"));
        let command = runner
            .command(&request(AiHarness::Codex, AiRunMode::Captured))
            .unwrap();
        assert!(command
            .args
            .windows(2)
            .any(|args| args == ["--model", "gpt-5"]));
        assert!(command
            .args
            .windows(2)
            .any(|args| args == ["--sandbox", "read-only"]));
    }

    #[test]
    fn session_titles_are_forwarded_only_to_opencode() {
        let mut opencode = request(AiHarness::OpenCode, AiRunMode::Captured);
        opencode.session_title = Some("review-123".into());
        let command = AiRunner::default()
            .with_binary(AiHarness::OpenCode, PathBuf::from("true"))
            .command(&opencode)
            .unwrap();
        assert!(command
            .args
            .windows(2)
            .any(|args| args == ["--title", "review-123"]));

        let mut codex = request(AiHarness::Codex, AiRunMode::Captured);
        codex.session_title = Some("review-123".into());
        let command = AiRunner::default()
            .with_binary(AiHarness::Codex, PathBuf::from("true"))
            .command(&codex)
            .unwrap();
        assert!(!command.args.iter().any(|arg| arg == "--title"));
    }

    #[test]
    fn rejects_models_for_the_wrong_harness_with_slot_context() {
        let runner = AiRunner::default();
        let mut req = request(AiHarness::ClaudeCode, AiRunMode::Interactive);
        req.config.model = "openai/gpt-5".into();
        let Err(WisetreeError::Ai {
            slot,
            harness,
            code,
            ..
        }) = runner.command(&req)
        else {
            panic!("expected AI error")
        };
        assert_eq!(slot, "dashboard.ai.fix.plan");
        assert_eq!(harness, "claudeCode");
        assert_eq!(code, AiErrorCode::UnavailableModel);
    }

    #[test]
    fn parses_semantic_versions() {
        assert_eq!(
            parse_version("claude 2.1.214 (Claude Code)"),
            Some((2, 1, 214))
        );
    }

    #[test]
    fn classifies_actionable_cli_failures() {
        assert_eq!(
            classify_cli_failure("Please login required"),
            AiErrorCode::MissingAuthentication
        );
        assert_eq!(
            classify_cli_failure("unknown flag --effort"),
            AiErrorCode::UnsupportedEffort
        );
        assert_eq!(
            classify_cli_failure("unknown option --json"),
            AiErrorCode::UnsupportedFlag
        );
    }
}
