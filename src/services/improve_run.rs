//! Durable state for one Improve walkthrough.
//!
//! The journal lives in the worktree-specific Git directory, not in the
//! checkout, so it survives Wisetree restarts without making the worktree
//! dirty. Discovery is frozen once; subsequent runs resume the first item
//! without a terminal outcome.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::errors::{Result, WisetreeError};
use crate::git::execute_git_command;
use crate::services::dashboard::{BugkillSnapshot, ReviewFile, ReviewFinding, ReviewSkippedFile};

const SCHEMA_VERSION: u32 = 1;
const STATE_GIT_PATH: &str = "wisetree/improve/run.json";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImproveRun {
    pub schema_version: u32,
    pub id: String,
    pub branch: String,
    pub base_ref: String,
    pub initial_head_sha: String,
    pub current_head_sha: String,
    pub full_scan: bool,
    pub files: Vec<ReviewFile>,
    pub skipped_files: Vec<ImproveSkippedFile>,
    pub items: Vec<ImproveRunItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImproveRunItem {
    pub id: String,
    pub finding: ReviewFinding,
    pub state: ImproveItemState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImproveCheckpointIdentity {
    pub run_id: String,
    pub finding_id: String,
    pub attempt_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ImproveItemState {
    Pending,
    Applying {
        attempt_id: String,
        baseline_head_sha: String,
        snapshot: BugkillSnapshot,
    },
    Applied {
        commit_sha: String,
    },
    Addressed,
    Skipped,
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImproveSkippedFile {
    pub path: String,
    pub reason: String,
}

impl ImproveRun {
    pub fn new(
        branch: String,
        base_ref: String,
        initial_head_sha: String,
        full_scan: bool,
        files: Vec<ReviewFile>,
        skipped_files: &[ReviewSkippedFile],
        findings: Vec<ReviewFinding>,
    ) -> Self {
        let started = now_millis();
        let id = digest(&[
            &initial_head_sha,
            &base_ref,
            if full_scan { "full" } else { "diff" },
            &started.to_string(),
            &std::process::id().to_string(),
            &TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed).to_string(),
        ]);
        let items = findings
            .into_iter()
            .enumerate()
            .map(|(index, finding)| ImproveRunItem {
                id: digest(&[
                    &id,
                    &index.to_string(),
                    &finding.file,
                    &finding
                        .start_line
                        .map_or_else(|| "-".to_string(), |n| n.to_string()),
                    &finding
                        .line
                        .map_or_else(|| "-".to_string(), |n| n.to_string()),
                    &finding.title,
                ]),
                finding,
                state: ImproveItemState::Pending,
            })
            .collect();
        Self {
            schema_version: SCHEMA_VERSION,
            id,
            branch,
            base_ref,
            current_head_sha: initial_head_sha.clone(),
            initial_head_sha,
            full_scan,
            files,
            skipped_files: skipped_files
                .iter()
                .map(|file| ImproveSkippedFile {
                    path: file.path.clone(),
                    reason: file.reason.to_string(),
                })
                .collect(),
            items,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(WisetreeError::other(format!(
                "unsupported Improve state version {}.",
                self.schema_version
            )));
        }
        if self.id.is_empty() || self.items.iter().any(|item| item.id.is_empty()) {
            return Err(WisetreeError::other(
                "Improve state contains an empty identity.",
            ));
        }
        let valid_sha =
            |sha: &str| sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !valid_sha(&self.initial_head_sha)
            || !valid_sha(&self.current_head_sha)
            || self.items.iter().any(|item| match &item.state {
                ImproveItemState::Applying {
                    baseline_head_sha, ..
                } => !valid_sha(baseline_head_sha),
                ImproveItemState::Applied { commit_sha } => !valid_sha(commit_sha),
                _ => false,
            })
        {
            return Err(WisetreeError::other(
                "Improve state contains an invalid Git commit identity.",
            ));
        }
        Ok(())
    }

    pub fn next_index(&self) -> Option<usize> {
        self.items.iter().position(|item| !item.state.is_terminal())
    }

    pub fn findings(&self) -> Vec<ReviewFinding> {
        self.items.iter().map(|item| item.finding.clone()).collect()
    }

    pub fn expected_head_sha(&self) -> &str {
        &self.current_head_sha
    }

    pub fn begin_attempt(
        &mut self,
        index: usize,
        baseline_head_sha: String,
        snapshot: BugkillSnapshot,
    ) -> Option<String> {
        let item = self.items.get_mut(index)?;
        let attempt_id = digest(&[
            &self.id,
            &item.id,
            &now_millis().to_string(),
            &TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed).to_string(),
        ]);
        item.state = ImproveItemState::Applying {
            attempt_id: attempt_id.clone(),
            baseline_head_sha,
            snapshot,
        };
        Some(attempt_id)
    }
}

impl ImproveItemState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Applied { .. } | Self::Addressed | Self::Skipped)
    }
}

pub async fn resolve_improve_state_path(worktree: &Path) -> Result<PathBuf> {
    let result = execute_git_command(
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            STATE_GIT_PATH,
        ],
        Some(worktree),
    )
    .await;
    if !result.success {
        return Err(WisetreeError::other(format!(
            "could not resolve Improve state path: {}",
            result.stderr.trim()
        )));
    }
    let path = PathBuf::from(result.stdout.trim());
    if !path.is_absolute() {
        return Err(WisetreeError::other(
            "Git returned a relative Improve state path.",
        ));
    }
    Ok(path)
}

pub fn load_improve_run(path: &Path) -> Result<Option<ImproveRun>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let run: ImproveRun = serde_json::from_slice(&bytes)?;
    run.validate()?;
    Ok(Some(run))
}

pub fn save_improve_run(path: &Path, run: &ImproveRun) -> Result<()> {
    run.validate()?;
    let parent = path
        .parent()
        .ok_or_else(|| WisetreeError::other("Improve state path has no parent."))?;
    fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(run)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    #[cfg(windows)]
    durable_replace(temporary.path(), path)?;
    #[cfg(not(windows))]
    temporary
        .persist(path)
        .map_err(|error| WisetreeError::from(error.error))?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn archive_improve_run(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let parent = path
        .parent()
        .ok_or_else(|| WisetreeError::other("Improve state path has no parent."))?;
    let archived = parent.join(format!(
        "run.archived-{}-{}.json",
        now_millis(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    durable_replace(path, &archived)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(Some(archived))
}

#[cfg(not(windows))]
fn durable_replace(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(windows)]
fn durable_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn clear_improve_run(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn digest(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::dashboard::ReviewSeverity;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn repository() -> TempDir {
        let temp = TempDir::new().unwrap();
        git(temp.path(), &["init", "-b", "main"]);
        git(temp.path(), &["config", "user.name", "Test"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        fs::write(temp.path().join("seed.txt"), "seed").unwrap();
        git(temp.path(), &["add", "seed.txt"]);
        git(temp.path(), &["commit", "-m", "seed"]);
        temp
    }

    fn finding(title: &str) -> ReviewFinding {
        ReviewFinding {
            category: "Test".to_string(),
            severity: ReviewSeverity::Medium,
            file: "src/lib.rs".to_string(),
            start_line: None,
            line: Some(4),
            title: title.to_string(),
            explanation: "explanation".to_string(),
            suggestion: None,
        }
    }

    fn run() -> ImproveRun {
        ImproveRun::new(
            "feature".to_string(),
            "origin/main".to_string(),
            "a".repeat(40),
            false,
            vec![ReviewFile {
                path: "src/lib.rs".to_string(),
                annotated_diff: "4:+change".to_string(),
                full_content: Some("change".to_string()),
                commentable_lines: BTreeSet::from([4]),
                existing_comments: String::new(),
                existing_keys: Vec::new(),
            }],
            &[],
            vec![finding("First"), finding("Second")],
        )
    }

    #[test]
    fn round_trips_atomically_and_replaces_existing_state() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nested/run.json");
        let mut expected = run();
        save_improve_run(&path, &expected).unwrap();
        assert_eq!(load_improve_run(&path).unwrap(), Some(expected.clone()));

        expected.items[0].state = ImproveItemState::Skipped;
        save_improve_run(&path, &expected).unwrap();
        assert_eq!(load_improve_run(&path).unwrap(), Some(expected));
    }

    #[test]
    fn item_identity_survives_finding_edits() {
        let mut run = run();
        let id = run.items[0].id.clone();
        run.items[0].finding.title = "Edited".to_string();
        assert_eq!(run.items[0].id, id);
        assert_ne!(run.items[0].id, run.items[1].id);
    }

    #[test]
    fn failed_findings_remain_resumable_until_applied_or_skipped() {
        let mut run = run();
        run.items[0].state = ImproveItemState::Failed {
            message: "model unavailable".to_string(),
        };
        run.items[1].state = ImproveItemState::Applied {
            commit_sha: "b".repeat(40),
        };
        assert_eq!(run.next_index(), Some(0));

        run.items[0].state = ImproveItemState::Skipped;
        assert_eq!(run.next_index(), None);
    }

    #[test]
    fn malformed_and_unknown_versions_are_errors() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("run.json");
        fs::write(&path, "not json").unwrap();
        assert!(load_improve_run(&path).is_err());

        let mut run = run();
        run.schema_version += 1;
        fs::write(&path, serde_json::to_vec(&run).unwrap()).unwrap();
        assert!(load_improve_run(&path).is_err());
    }

    #[test]
    fn archive_unblocks_a_stale_run_without_deleting_it() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("run.json");
        fs::write(&path, "stale state").unwrap();

        let archived = archive_improve_run(&path).unwrap().unwrap();
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(archived).unwrap(), "stale state");
        assert_eq!(archive_improve_run(&path).unwrap(), None);
    }

    #[test]
    fn clear_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("run.json");
        clear_improve_run(&path).unwrap();
        save_improve_run(&path, &run()).unwrap();
        clear_improve_run(&path).unwrap();
        assert_eq!(load_improve_run(&path).unwrap(), None);
    }

    #[tokio::test]
    async fn state_path_is_private_to_each_linked_worktree() {
        let repo = repository();
        let linked_parent = TempDir::new().unwrap();
        let linked = linked_parent.path().join("linked");
        git(
            repo.path(),
            &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
        );

        let main_path = resolve_improve_state_path(repo.path()).await.unwrap();
        let linked_path = resolve_improve_state_path(&linked).await.unwrap();
        assert_ne!(main_path, linked_path);
        let main_git_dir = PathBuf::from(git(repo.path(), &["rev-parse", "--absolute-git-dir"]));
        let linked_git_dir = PathBuf::from(git(&linked, &["rev-parse", "--absolute-git-dir"]));
        assert_eq!(main_path, main_git_dir.join(STATE_GIT_PATH));
        assert_eq!(linked_path, linked_git_dir.join(STATE_GIT_PATH));

        save_improve_run(&linked_path, &run()).unwrap();
        assert!(load_improve_run(&main_path).unwrap().is_none());
        assert!(load_improve_run(&linked_path).unwrap().is_some());
    }
}
