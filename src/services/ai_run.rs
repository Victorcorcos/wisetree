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
    /// Image files delivered to the harness. Image bytes never enter the
    /// prompt; see [`attachment_delivery`] for how each CLI receives them.
    pub attachments: Vec<PathBuf>,
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
        // Harnesses without a native attachment flag receive the paths as a
        // readable trailer instead, so the prompt is resolved before the
        // per-CLI syntaxes below embed it.
        let delivery = attachment_delivery(harness, request.mode);
        let prompt = match delivery {
            AttachmentDelivery::Flag(_) => request.prompt.clone(),
            AttachmentDelivery::PromptPaths { mention } => {
                append_attachment_paths(&request.prompt, &request.attachments, mention)
            }
        };
        let mut args = match (harness, request.mode) {
            (AiHarness::OpenCode, AiRunMode::Interactive) => {
                // opencode's interactive TUI exposes no `--variant` flag, so the
                // configured reasoning effort is seeded into its `model.json`
                // (keyed by this same `provider/model`) before launch — see
                // `seed_opencode_tui_variant`.
                seed_opencode_tui_variant(&model, effort);
                let mut args = vec!["--prompt".into(), prompt.clone(), "-m".into(), model];
                if request.permission == AiPermission::Plan {
                    args.extend(["--agent".into(), "plan".into()]);
                }
                args.push(request.cwd.to_string_lossy().to_string());
                args
            }
            (AiHarness::OpenCode, AiRunMode::Captured) => {
                let mut args = vec!["run".into(), prompt.clone(), "-m".into(), model];
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
                // Wisetree embeds Codex inside its own terminal emulator and
                // owns the transcript scrollback. Codex defaults to its
                // alternate screen (`tui.alternate_screen = "auto"`), whose
                // alternate-scroll mode changes how wheel input is delivered.
                // Force the supported inline mode so wheel events always move
                // Wisetree's vt100 scrollback and can never reach Codex as raw
                // SGR mouse reports (whose leading ESC interrupts the turn).
                let mut args = vec![
                    "--no-alt-screen".into(),
                    "--dangerously-bypass-approvals-and-sandbox".into(),
                    "--model".into(),
                    model,
                    prompt.clone(),
                ];
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
                    "--dangerously-bypass-approvals-and-sandbox".into(),
                    "--model".into(),
                    model,
                    prompt.clone(),
                ];
                if !effort.is_empty() {
                    args.extend([
                        "--config".into(),
                        format!("model_reasoning_effort={effort}"),
                    ]);
                }
                args
            }
            (AiHarness::ClaudeCode, AiRunMode::Interactive) => {
                let mut args = vec![
                    "--dangerously-skip-permissions".into(),
                    "--model".into(),
                    model,
                    prompt.clone(),
                ];
                if !effort.is_empty() {
                    args.extend(["--effort".into(), effort.into()]);
                }
                args
            }
            (AiHarness::ClaudeCode, AiRunMode::Captured) => {
                let mut args = vec![
                    "-p".into(),
                    "--dangerously-skip-permissions".into(),
                    prompt.clone(),
                    "--model".into(),
                    model,
                    "--output-format".into(),
                    "text".into(),
                ];
                if !effort.is_empty() {
                    args.extend(["--effort".into(), effort.into()]);
                }
                args
            }
        };
        if let AttachmentDelivery::Flag(flag) = delivery {
            for attachment in &request.attachments {
                args.extend([flag.to_string(), attachment.to_string_lossy().to_string()]);
            }
        }
        // Keep the prompt as the sole textual payload argument. The individual CLI
        // syntaxes above intentionally place it directly after their prompt flag.
        debug_assert!(args.iter().any(|arg| arg == &prompt));
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

/// How one harness accepts image attachments in one run mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachmentDelivery {
    /// The CLI has a native attachment flag, repeated once per image.
    Flag(&'static str),
    /// The CLI has no attachment flag here, so the absolute paths ride along
    /// in the prompt, written in that harness's own file-reference syntax.
    PromptPaths { mention: &'static str },
}

/// Attachment support is per-command, not per-harness, and both `claude` and
/// `opencode` accept unknown flags silently — an unsupported flag would drop
/// the images with no error at all. Every entry below is verified against the
/// CLI's own `--help`.
fn attachment_delivery(harness: AiHarness, mode: AiRunMode) -> AttachmentDelivery {
    match (harness, mode) {
        // `opencode run` has `-f, --file`; the root TUI command it launches in
        // interactive mode does not. There, `--prompt` pre-fills the composer,
        // so the paths are written as `@` mentions — opencode's documented way
        // to reference a file, since a bare path in prompt text is not
        // attached.
        (AiHarness::OpenCode, AiRunMode::Captured) => AttachmentDelivery::Flag("--file"),
        (AiHarness::OpenCode, AiRunMode::Interactive) => {
            AttachmentDelivery::PromptPaths { mention: "@" }
        }
        // `-i, --image` exists on both the root `codex` command and `codex exec`.
        (AiHarness::Codex, _) => AttachmentDelivery::Flag("--image"),
        // Claude Code has no image flag in any mode; its documented path is to
        // name the file in the prompt and let it read the image itself.
        (AiHarness::ClaudeCode, _) => AttachmentDelivery::PromptPaths { mention: "" },
    }
}

/// Append the attachment paths as a readable trailer. The prompt stays a
/// single literal argument and never carries image bytes.
fn append_attachment_paths(prompt: &str, attachments: &[PathBuf], mention: &str) -> String {
    if attachments.is_empty() {
        return prompt.to_string();
    }
    let mut out = String::from(prompt.trim_end());
    out.push_str(
        "\n\n- Image attachments for this request. Read each one before you begin and treat \
         it as part of the description above:\n\n",
    );
    for (index, attachment) in attachments.iter().enumerate() {
        out.push_str(&format!(
            "{}. {mention}{}\n",
            index + 1,
            attachment.display()
        ));
    }
    out
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

/// opencode's sentinel value for "no reasoning override" in `model.json`. The
/// TUI persists this (not the empty string) when a model is cycled back to no
/// variant, and treats it as "no override" because it's never a real variant
/// name (which are reasoning efforts like `high`/`max`).
const OPENCODE_NO_VARIANT: &str = "default";

/// Persist the configured reasoning effort for `model` into opencode's
/// `model.json` so the **TUI** opens at that thinking strength.
///
/// opencode's interactive TUI (`opencode [project]`) exposes no `--variant`
/// flag — through at least 1.17.x it resolves a model's reasoning effort
/// *solely* from its persisted state file (the "saved preference" the user
/// otherwise cycles with ctrl+t), keyed by `provider/model`. Only `opencode
/// run` takes `--variant` (see the Captured arm of [`AiRunner::command`]). So
/// to launch a TUI flow at the user's configured strength we seed that exact
/// entry here first.
///
/// Best-effort: any IO/JSON error is swallowed so a read-only or absent state
/// dir never blocks the AI flow — it just launches without the seeded effort,
/// exactly as before this seeding existed.
fn seed_opencode_tui_variant(model: &str, thinking: &str) {
    seed_opencode_tui_variant_at(
        &crate::constants::opencode_model_state_file(),
        model,
        thinking,
    );
}

/// [`seed_opencode_tui_variant`] against an explicit state-file path, so tests
/// can target a tempdir instead of the developer's real `$XDG_STATE_HOME`.
fn seed_opencode_tui_variant_at(path: &Path, model: &str, thinking: &str) {
    let model = model.trim();
    if model.is_empty() {
        return;
    }
    let current = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let next = merged_variant_state(current, model, thinking);
    let _ = write_json_atomic(path, &next);
}

/// Pure merge: take the current `model.json` value (or `None`) and return it
/// with `variant[model]` set to the resolved effort, preserving every other
/// field (`recent`, `favorite`, other models' variants). An empty `thinking`
/// (the persisted "Default") writes [`OPENCODE_NO_VARIANT`], which clears any
/// stale effort a prior session left for this model. Factored out so the merge
/// is unit-testable without touching the filesystem.
fn merged_variant_state(
    current: Option<serde_json::Value>,
    model: &str,
    thinking: &str,
) -> serde_json::Value {
    let effort = {
        let t = thinking.trim();
        if t.is_empty() {
            OPENCODE_NO_VARIANT
        } else {
            t
        }
    };

    let mut root = current
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    // Safe: `root` is guaranteed to be an object by the filter/fallback above.
    let obj = root.as_object_mut().expect("root is a json object");
    let variant = obj
        .entry("variant")
        .or_insert_with(|| serde_json::json!({}));
    if !variant.is_object() {
        *variant = serde_json::json!({});
    }
    variant
        .as_object_mut()
        .expect("variant is a json object")
        .insert(
            model.to_string(),
            serde_json::Value::String(effort.to_string()),
        );
    root
}

/// Atomically write `value` as JSON to `path` — write a sibling temp file then
/// rename over the target — so a concurrent opencode reader never observes a
/// half-written file (mirrors opencode's own `writeJsonAtomic`). Parent dirs
/// are created as needed.
fn write_json_atomic(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // pid-suffixed sibling so two wisetree processes can't collide on the temp.
    let tmp = path.with_file_name(format!(
        ".{}.wisetree.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "model.json".to_string()),
        std::process::id()
    ));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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
            attachments: Vec::new(),
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

    /// Every (harness, mode) pair either emits a flag its CLI really has, or
    /// falls back to prompt paths. Nothing may be attached with a flag the
    /// command would silently ignore.
    #[test]
    fn image_attachments_reach_every_harness_by_a_supported_route() {
        let cases = [
            (AiHarness::OpenCode, AiRunMode::Captured, Some("--file")),
            (AiHarness::OpenCode, AiRunMode::Interactive, None),
            (AiHarness::Codex, AiRunMode::Captured, Some("--image")),
            (AiHarness::Codex, AiRunMode::Interactive, Some("--image")),
            (AiHarness::ClaudeCode, AiRunMode::Captured, None),
            (AiHarness::ClaudeCode, AiRunMode::Interactive, None),
        ];
        for (harness, mode, flag) in cases {
            let runner = AiRunner::default().with_binary(harness, PathBuf::from("true"));
            let mut req = request(harness, mode);
            req.attachments = vec![PathBuf::from("/tmp/screenshot.png")];
            let command = runner.command(&req).unwrap();
            let prompt_arg = command
                .args
                .iter()
                .find(|arg| arg.starts_with(&req.prompt))
                .expect("the prompt is always one argument");

            match flag {
                Some(flag) => {
                    assert!(
                        command
                            .args
                            .windows(2)
                            .any(|args| args == [flag, "/tmp/screenshot.png"]),
                        "{harness:?}/{mode:?} should attach with {flag}"
                    );
                    // A native flag carries the image, so the prompt is untouched.
                    assert_eq!(prompt_arg, &req.prompt);
                }
                None => {
                    assert!(
                        !command
                            .args
                            .iter()
                            .any(|arg| arg == "--image" || arg == "--file"),
                        "{harness:?}/{mode:?} has no attachment flag and must not invent one"
                    );
                    // opencode needs its `@` mention syntax; a bare path in
                    // prompt text is not attached. Claude Code reads a plain
                    // path with its own file tools.
                    let expected = if harness == AiHarness::OpenCode {
                        "@/tmp/screenshot.png"
                    } else {
                        "/tmp/screenshot.png"
                    };
                    assert!(
                        prompt_arg.contains(expected),
                        "{harness:?}/{mode:?} must reference the image as {expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_prompt_without_attachments_is_never_rewritten() {
        for harness in [AiHarness::OpenCode, AiHarness::Codex, AiHarness::ClaudeCode] {
            for mode in [AiRunMode::Interactive, AiRunMode::Captured] {
                let runner = AiRunner::default().with_binary(harness, PathBuf::from("true"));
                let req = request(harness, mode);
                let command = runner.command(&req).unwrap();
                assert!(command.args.iter().any(|arg| arg == &req.prompt));
            }
        }
    }

    #[test]
    fn image_paths_with_shell_syntax_remain_single_literal_arguments() {
        let attachment = PathBuf::from("/tmp/image; touch should-not-run.png");
        let path = attachment.to_string_lossy().to_string();
        for harness in [AiHarness::OpenCode, AiHarness::Codex, AiHarness::ClaudeCode] {
            for mode in [AiRunMode::Interactive, AiRunMode::Captured] {
                let runner = AiRunner::default().with_binary(harness, PathBuf::from("true"));
                let mut req = request(harness, mode);
                req.attachments = vec![attachment.clone()];
                let command = runner.command(&req).unwrap();
                // Either the flag's value or the prompt trailer holds the path
                // verbatim, and in both cases it is one argv entry — the `;`
                // can never be seen by a shell.
                assert!(command
                    .args
                    .iter()
                    .any(|arg| arg == &path || arg.contains(&path)));
            }
        }
    }

    #[test]
    fn translates_canonical_codex_models() {
        let runner = AiRunner::default().with_binary(AiHarness::Codex, PathBuf::from("true"));
        let command = runner
            .command(&request(AiHarness::Codex, AiRunMode::Captured))
            .unwrap();
        assert!(command
            .args
            .windows(2)
            .any(|args| args == ["--model", "gpt-5"]));
    }

    #[test]
    fn codex_always_bypasses_approvals_and_sandbox() {
        let runner = AiRunner::default().with_binary(AiHarness::Codex, PathBuf::from("true"));
        for mode in [AiRunMode::Interactive, AiRunMode::Captured] {
            for permission in [AiPermission::Plan, AiPermission::Implement] {
                let mut req = request(AiHarness::Codex, mode);
                req.permission = permission;
                let command = runner.command(&req).unwrap();
                assert!(command
                    .args
                    .iter()
                    .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
                assert!(!command.args.iter().any(|arg| arg == "--sandbox"));
                assert!(!command.args.iter().any(|arg| arg == "--ask-for-approval"));
            }
        }
    }

    #[test]
    fn claude_always_skips_permissions() {
        let runner = AiRunner::default().with_binary(AiHarness::ClaudeCode, PathBuf::from("true"));
        for mode in [AiRunMode::Interactive, AiRunMode::Captured] {
            for permission in [AiPermission::Plan, AiPermission::Implement] {
                let mut req = request(AiHarness::ClaudeCode, mode);
                req.permission = permission;
                let command = runner.command(&req).unwrap();
                assert!(command
                    .args
                    .iter()
                    .any(|arg| arg == "--dangerously-skip-permissions"));
                assert!(!command.args.iter().any(|arg| arg == "--permission-mode"));
            }
        }
    }

    #[test]
    fn codex_does_not_use_removed_full_auto_flag() {
        let runner = AiRunner::default().with_binary(AiHarness::Codex, PathBuf::from("true"));
        for mode in [AiRunMode::Interactive, AiRunMode::Captured] {
            let command = runner.command(&request(AiHarness::Codex, mode)).unwrap();
            assert!(
                !command.args.iter().any(|arg| arg == "--full-auto"),
                "codex {mode:?} must not pass the removed --full-auto flag"
            );
        }
    }

    #[test]
    fn interactive_codex_forces_inline_mode_for_embedded_scrollback() {
        let runner = AiRunner::default().with_binary(AiHarness::Codex, PathBuf::from("true"));

        let interactive = request(AiHarness::Codex, AiRunMode::Interactive);
        let command = runner.command(&interactive).unwrap();
        assert!(
            command.args.iter().any(|arg| arg == "--no-alt-screen"),
            "embedded Codex must stay inline so Wisetree owns wheel scrolling"
        );

        let captured = request(AiHarness::Codex, AiRunMode::Captured);
        let command = runner.command(&captured).unwrap();
        assert!(
            !command.args.iter().any(|arg| arg == "--no-alt-screen"),
            "codex exec has no TUI and must not receive TUI-only flags"
        );
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

    #[test]
    fn merged_variant_state_seeds_variant_into_empty_state() {
        // No prior model.json → a fresh object carrying just the seeded effort.
        let next = merged_variant_state(None, "openai/gpt-5.4", "high");
        assert_eq!(next["variant"]["openai/gpt-5.4"], serde_json::json!("high"));
    }

    #[test]
    fn merged_variant_state_preserves_other_fields_and_overwrites_same_model() {
        // A realistic model.json: recent/favorite and other models' variants
        // must survive untouched; only the target model's effort changes.
        let current = serde_json::json!({
            "recent": [{ "providerID": "openai", "modelID": "gpt-5.4" }],
            "favorite": [],
            "variant": {
                "openai/gpt-5.5": "low",
                "openai/gpt-5.4": "medium"
            }
        });
        let next = merged_variant_state(Some(current), "openai/gpt-5.4", "high");
        assert_eq!(
            next["recent"],
            serde_json::json!([{ "providerID": "openai", "modelID": "gpt-5.4" }])
        );
        assert_eq!(next["favorite"], serde_json::json!([]));
        // Untouched sibling variant.
        assert_eq!(next["variant"]["openai/gpt-5.5"], serde_json::json!("low"));
        // Target model overwritten medium → high.
        assert_eq!(next["variant"]["openai/gpt-5.4"], serde_json::json!("high"));
    }

    #[test]
    fn merged_variant_state_writes_default_sentinel_for_blank_thinking() {
        // Empty thinking (the persisted "Default") must clear any stale effort
        // by writing opencode's "default" sentinel, not the empty string.
        let current = serde_json::json!({ "variant": { "openai/gpt-5.4": "max" } });
        for blank in ["", "   "] {
            let next = merged_variant_state(Some(current.clone()), "openai/gpt-5.4", blank);
            assert_eq!(
                next["variant"]["openai/gpt-5.4"],
                serde_json::json!("default")
            );
        }
    }

    #[test]
    fn merged_variant_state_recovers_from_a_non_object_variant_field() {
        // A corrupt/unexpected `variant` (here an array) is replaced with a
        // fresh object rather than panicking or dropping the seed.
        let current = serde_json::json!({ "variant": [1, 2, 3] });
        let next = merged_variant_state(Some(current), "openai/gpt-5.4", "high");
        assert_eq!(next["variant"]["openai/gpt-5.4"], serde_json::json!("high"));
    }

    #[test]
    fn seed_opencode_tui_variant_at_round_trips_through_disk() {
        // End-to-end against a tempdir: the seeded effort lands in model.json
        // and a subsequent seed of a different model is merged in, not clobbered.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode").join("model.json");

        seed_opencode_tui_variant_at(&path, "openai/gpt-5.4", "high");
        let first: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read seeded file"))
                .expect("valid json");
        assert_eq!(
            first["variant"]["openai/gpt-5.4"],
            serde_json::json!("high")
        );

        seed_opencode_tui_variant_at(&path, "opencode/glm-5.2", "max");
        let second: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read seeded file"))
                .expect("valid json");
        // Both entries coexist after the second seed.
        assert_eq!(
            second["variant"]["openai/gpt-5.4"],
            serde_json::json!("high")
        );
        assert_eq!(
            second["variant"]["opencode/glm-5.2"],
            serde_json::json!("max")
        );
    }

    #[test]
    fn seed_opencode_tui_variant_at_is_a_noop_for_blank_model() {
        // A blank model id must never create or touch the state file.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode").join("model.json");
        seed_opencode_tui_variant_at(&path, "   ", "high");
        assert!(!path.exists());
    }
}
