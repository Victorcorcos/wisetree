//! Shared state and services for the Global Dashboard.

pub mod github;
pub mod registry;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinSet;

use crate::config::schema::DashboardConfig;
use crate::constants::{dashboard_pr_cache_file, GLOBAL_DASHBOARD_ROWS_FILE_NAME};
use crate::git::types::GitWorktree;
use crate::services::dashboard::{binary_available, RATE_LIMIT_BACKOFF};
use crate::services::{
    default_dashboard_warning, DashboardNotice, DashboardNoticeLevel, DashboardRepository,
    DashboardRow, DashboardService, DashboardUpdate, DashboardWatch, PR_REFRESH_PERIOD_MS,
};
use github::{AuthoredPullRequest, GitHubError, GitHubService};
use registry::{LocalRepository, RepositoryRegistry};

/// GitHub requests in flight; GitHub asks clients to keep concurrency low.
const REPOSITORY_CONCURRENCY: usize = 4;
/// Repositories whose local git state is collected at once.
const LOCAL_REPOSITORY_CONCURRENCY: usize = 16;

#[derive(Debug, Default)]
struct GlobalState {
    services: HashMap<PathBuf, (DashboardRepository, DashboardService)>,
    /// Last registry resolution. It runs dozens of git commands, so it is
    /// reused between ticks and re-resolved each PR cycle or manual refresh.
    repositories: Option<(Instant, Result<Vec<LocalRepository>, String>)>,
    rows: Option<Vec<DashboardRow>>,
    /// Rows last written to the snapshot file, to skip unchanged writes.
    snapshot: Option<Vec<DashboardRow>>,
    search: Vec<AuthoredPullRequest>,
    next_pr_fetch_at: Option<Instant>,
    rate_limited_until: Option<Instant>,
}

/// The local rows last painted for an account selection. Collecting them runs
/// several git commands per worktree, so a cold watch opens on this first.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RowsSnapshot {
    accounts: Vec<String>,
    rows: Vec<DashboardRow>,
}

/// Shares repository caches, authored search results and the refresh deadline
/// across watches. Account selection is fixed for the lifetime of a service.
#[derive(Debug, Clone)]
pub struct GlobalDashboardService {
    config: DashboardConfig,
    selected_accounts: Vec<String>,
    registry: RepositoryRegistry,
    gh_binary: PathBuf,
    cache_path: Option<PathBuf>,
    state: Arc<Mutex<GlobalState>>,
}

impl GlobalDashboardService {
    pub fn new(mut config: DashboardConfig, selected_accounts: Vec<String>) -> Self {
        config.clamp();
        Self {
            config,
            selected_accounts,
            registry: RepositoryRegistry::default(),
            gh_binary: "gh".into(),
            cache_path: Some(dashboard_pr_cache_file()),
            state: Arc::new(Mutex::new(GlobalState::default())),
        }
    }

    pub fn with_registry(mut self, registry: RepositoryRegistry) -> Self {
        self.registry = registry;
        self
    }

    pub fn with_gh_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.gh_binary = binary.into();
        self
    }

    /// Where the per-repository PR cache persists; the rows snapshot lives
    /// beside it. `None` disables both.
    pub fn with_cache_path(mut self, path: Option<PathBuf>) -> Self {
        self.cache_path = path;
        self
    }

    pub fn watch(&self) -> DashboardWatch {
        let (tx, rx) = mpsc::channel(8);
        let (notice_tx, notice_rx) = mpsc::channel(8);
        let (refresh_tx, mut refresh_rx) = mpsc::channel(1);
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let service = self.clone();
        tokio::spawn(async move {
            {
                let state = service.state.lock().await;
                if let Some(rows) = &state.rows {
                    let _ = tx
                        .send(DashboardUpdate::WithPRs {
                            rows: rows.clone(),
                            next_pr_fetch_at: state.next_pr_fetch_at,
                        })
                        .await;
                } else if let Some(rows) = service.load_rows_snapshot() {
                    let _ = tx.send(DashboardUpdate::GitOnly(rows)).await;
                }
            }
            let mut force = false;
            loop {
                tokio::select! {
                    _ = &mut cancel_rx => break,
                    _ = service.refresh(force, Some(&tx), Some(&notice_tx)) => {}
                }
                let deadline = service.state.lock().await.next_pr_fetch_at;
                let delay = deadline
                    .map(|due| due.saturating_duration_since(Instant::now()))
                    .unwrap_or(Duration::from_millis(service.config.refresh_interval_ms))
                    .min(Duration::from_millis(service.config.refresh_interval_ms));
                force = tokio::select! {
                    _ = &mut cancel_rx => break,
                    _ = tokio::time::sleep(delay) => false,
                    refresh = refresh_rx.recv() => {
                        if refresh.is_none() { break; }
                        true
                    }
                };
            }
        });
        DashboardWatch {
            rx,
            notice_rx,
            cancel: Some(cancel_tx),
            refresh_tx,
        }
    }

    pub async fn snapshot(&self) -> Vec<DashboardRow> {
        self.refresh(false, None, None).await
    }

    async fn refresh(
        &self,
        force: bool,
        tx: Option<&mpsc::Sender<DashboardUpdate>>,
        notice_tx: Option<&mpsc::Sender<DashboardNotice>>,
    ) -> Vec<DashboardRow> {
        // Serialize concurrent watches so only one performs a due search.
        let mut state = self.state.lock().await;
        if self.selected_accounts.is_empty() {
            state.rows = Some(Vec::new());
            if let Some(tx) = tx {
                let _ = tx.send(DashboardUpdate::GitOnly(Vec::new())).await;
            }
            return Vec::new();
        }
        let mut warnings = Vec::new();
        let resolve_due = force
            || state
                .repositories
                .as_ref()
                .map_or(true, |(resolved_at, _)| {
                    resolved_at.elapsed() >= Duration::from_millis(PR_REFRESH_PERIOD_MS)
                });
        if resolve_due {
            let resolved = self.registry.resolve().await;
            state.repositories = Some((Instant::now(), resolved.map_err(|e| e.to_string())));
        }
        let repositories = match state.repositories.as_ref().map(|(_, resolved)| resolved) {
            Some(Ok(repositories)) => repositories.clone(),
            Some(Err(error)) => {
                warnings.push(format!("Local repository registry: {error}"));
                Vec::new()
            }
            None => Vec::new(),
        };
        // Probe once: every repository shares the same gh binary.
        let gh_available = binary_available(&self.gh_binary);
        let mut live = HashSet::new();
        for repository in repositories {
            let Some((owner, name)) = repository
                .pr_hosting_slug
                .as_deref()
                .and_then(|slug| slug.split_once('/'))
            else {
                continue;
            };
            if !self
                .selected_accounts
                .iter()
                .any(|selected| selected.eq_ignore_ascii_case(owner))
            {
                continue;
            }
            let root = repository.mother_worktree;
            live.insert(root.clone());
            let identity = DashboardRepository {
                owner: owner.into(),
                name: name.into(),
                local_root: Some(root.clone()),
            };
            state.services.entry(root.clone()).or_insert_with(|| {
                let service = DashboardService::new_with_probed_gh(
                    root,
                    self.config.clone(),
                    self.gh_binary.clone(),
                    gh_available,
                )
                .with_cache_path(self.cache_path.clone());
                (identity, service)
            });
        }
        state.services.retain(|root, (identity, _)| {
            if live.contains(root) {
                return true;
            }
            warnings.push(format!(
                "{}/{} ({}): local repository unavailable",
                identity.owner,
                identity.name,
                root.display()
            ));
            false
        });
        let enrich = self.config.show_pull_requests && gh_available;
        if let Some(warning) = default_dashboard_warning(&self.config, gh_available) {
            warnings.push(warning);
        }
        if !enrich {
            state.search.clear();
            state.next_pr_fetch_at = None;
        }
        let services: Vec<_> = state.services.values().cloned().collect();
        let mut local = Vec::new();
        // Bound queued work as well as active repositories.
        for chunk in services.chunks(LOCAL_REPOSITORY_CONCURRENCY) {
            let mut tasks = JoinSet::new();
            for (identity, service) in chunk {
                let (identity, service) = (identity.clone(), service.clone());
                tasks.spawn(async move {
                    let rows = service.collect_git_rows().await;
                    (identity, service, rows)
                });
            }
            while let Some(result) = tasks.join_next().await {
                match result {
                    Ok((identity, service, Ok(mut rows))) => {
                        for row in &mut rows {
                            row.repository = Some(identity.clone());
                        }
                        local.push((identity, service, rows));
                    }
                    Ok((identity, _, Err(error))) => {
                        warnings.push(format!("{}/{}: {error}", identity.owner, identity.name))
                    }
                    Err(error) => warnings.push(format!("Local repository task: {error}")),
                }
            }
        }
        let rows = merge_rows(&local, &state.search);
        state.rows = Some(rows.clone());
        if let Some(tx) = tx {
            let _ = tx.send(DashboardUpdate::GitOnly(rows)).await;
        }

        let now = Instant::now();
        let backed_off = state.rate_limited_until.is_some_and(|due| now < due);
        let on_cycle = force || state.next_pr_fetch_at.map_or(true, |due| now >= due);
        let mut rate_limited = false;
        if enrich && !backed_off {
            if on_cycle {
                let search = GitHubService::default()
                    .with_gh_binary(self.gh_binary.clone())
                    .fetch_authored_pull_requests(&self.selected_accounts)
                    .await;
                match search {
                    Ok(search) => {
                        let failed: HashSet<_> = search
                            .failures
                            .iter()
                            .map(|failure| failure.account.to_ascii_lowercase())
                            .collect();
                        // A failed account retains its last good results; successful
                        // accounts replace theirs, removing closed/deleted PRs.
                        state
                            .search
                            .retain(|pr| failed.contains(&pr.owner.login.to_ascii_lowercase()));
                        state.search.extend(search.pull_requests);
                        for failure in search.failures {
                            rate_limited |= matches!(failure.error, GitHubError::RateLimited(_));
                            warnings.push(format!("{}: {}", failure.account, failure.error));
                        }
                    }
                    Err(error) => {
                        rate_limited = matches!(error, GitHubError::RateLimited(_));
                        warnings.push(format!("GitHub authored PR search: {error}"));
                    }
                }
                state.next_pr_fetch_at =
                    Some(Instant::now() + Duration::from_millis(PR_REFRESH_PERIOD_MS));
            }
            if !rate_limited {
                for chunk in local.chunks_mut(REPOSITORY_CONCURRENCY) {
                    let mut tasks = JoinSet::new();
                    for (identity, service, rows) in chunk.iter() {
                        let (identity, service, mut rows) =
                            (identity.clone(), service.clone(), rows.clone());
                        tasks.spawn(async move {
                            let (notices, mut received) = mpsc::channel(8);
                            service.set_notice_sender(notices);
                            service.refresh_pull_requests(&rows, on_cycle).await;
                            service.apply_cached_prs(&mut rows);
                            let limited = service.is_rate_limited();
                            let mut warnings = Vec::new();
                            while let Ok(notice) = received.try_recv() {
                                warnings.push(notice.message);
                            }
                            (identity, rows, warnings, limited)
                        });
                    }
                    while let Some(result) = tasks.join_next().await {
                        match result {
                            Ok((identity, rows, notices, limited)) => {
                                rate_limited |= limited;
                                warnings.extend(notices.into_iter().map(|notice| {
                                    format!("{}/{}: {notice}", identity.owner, identity.name)
                                }));
                                if let Some(entry) =
                                    chunk.iter_mut().find(|(repo, _, _)| repo == &identity)
                                {
                                    entry.2 = rows;
                                }
                            }
                            Err(error) => warnings.push(format!("Repository PR refresh: {error}")),
                        }
                    }
                    if rate_limited {
                        break;
                    }
                }
            }
        }
        if rate_limited {
            let due = Instant::now() + RATE_LIMIT_BACKOFF;
            state.rate_limited_until = Some(due);
            state.next_pr_fetch_at = Some(due);
            warnings.push(
                "GitHub API rate-limited — pausing PR refresh for 5 min; showing cached data."
                    .into(),
            );
        }
        // Each save merges with the same on-disk document. Serialize writes
        // after concurrent enrichment so repositories cannot overwrite each other.
        for (_, service, _) in &local {
            service.save_cache();
        }
        let rows = merge_rows(&local, &state.search);
        state.rows = Some(rows.clone());
        self.save_rows_snapshot(&mut state, &rows);
        if enrich {
            if let Some(tx) = tx {
                let _ = tx
                    .send(DashboardUpdate::WithPRs {
                        rows: rows.clone(),
                        next_pr_fetch_at: state.next_pr_fetch_at,
                    })
                    .await;
            }
        }
        if !warnings.is_empty() {
            warnings.sort();
            warnings.dedup();
            if let Some(tx) = notice_tx {
                let _ = tx.try_send(DashboardNotice {
                    level: DashboardNoticeLevel::Warning,
                    message: warnings.join("; "),
                });
            }
        }
        rows
    }

    fn rows_snapshot_path(&self) -> Option<PathBuf> {
        Some(
            self.cache_path
                .as_ref()?
                .with_file_name(GLOBAL_DASHBOARD_ROWS_FILE_NAME),
        )
    }

    fn load_rows_snapshot(&self) -> Option<Vec<DashboardRow>> {
        let raw = std::fs::read(self.rows_snapshot_path()?).ok()?;
        let snapshot: RowsSnapshot = serde_json::from_slice(&raw).ok()?;
        same_accounts(&snapshot.accounts, &self.selected_accounts).then_some(snapshot.rows)
    }

    /// Best-effort: a failed write only costs the next cold open its head start.
    fn save_rows_snapshot(&self, state: &mut GlobalState, rows: &[DashboardRow]) {
        let Some(path) = self.rows_snapshot_path() else {
            return;
        };
        // Remote-only rows return with the next search instead of being
        // replayed from disk, so they never flicker in and out.
        let rows: Vec<_> = rows
            .iter()
            .filter(|row| {
                row.repository
                    .as_ref()
                    .is_some_and(|repo| repo.local_root.is_some())
            })
            .cloned()
            .collect();
        if state.snapshot.as_ref() == Some(&rows) {
            return;
        }
        let snapshot = RowsSnapshot {
            accounts: self.selected_accounts.clone(),
            rows,
        };
        if write_json(&path, &snapshot).is_ok() {
            state.snapshot = Some(snapshot.rows);
        }
    }
}

fn same_accounts(first: &[String], second: &[String]) -> bool {
    let normalized = |accounts: &[String]| {
        accounts
            .iter()
            .map(|account| account.to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
    };
    normalized(first) == normalized(second)
}

/// Readers see only a complete old or new document.
fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, value)?;
    file.persist(path).map_err(|err| err.error)?;
    Ok(())
}

fn merge_rows(
    local: &[(DashboardRepository, DashboardService, Vec<DashboardRow>)],
    search: &[AuthoredPullRequest],
) -> Vec<DashboardRow> {
    let mut rows: Vec<_> = local.iter().flat_map(|(_, _, rows)| rows.clone()).collect();
    let mut remote = HashSet::new();
    for pr in search {
        let matches = |row: &&mut DashboardRow| {
            row.repository.as_ref().is_some_and(|repo| {
                repo.local_root.is_some()
                    && repo.owner.eq_ignore_ascii_case(&pr.owner.login)
                    && repo.name.eq_ignore_ascii_case(&pr.repository)
            }) && row.worktree.branch == pr.head_branch
        };
        let mut matched = false;
        for row in rows.iter_mut().filter(matches) {
            matched = true;
            if row.pull_request.is_none() {
                row.pull_request = Some(pr.pull_request.clone());
            }
        }
        if matched
            || !remote.insert((
                pr.owner.login.to_ascii_lowercase(),
                pr.repository.to_ascii_lowercase(),
                pr.pull_request.number,
            ))
        {
            continue;
        }
        rows.push(DashboardRow {
            repository: Some(DashboardRepository {
                owner: pr.owner.login.clone(),
                name: pr.repository.clone(),
                local_root: None,
            }),
            worktree: GitWorktree {
                branch: pr.head_branch.clone(),
                ..GitWorktree::default()
            },
            last_commit: None,
            pull_request: Some(pr.pull_request.clone()),
            error: None,
        });
    }
    rows.sort_by_key(|row| {
        let repo = row.repository.as_ref().expect("global row repository");
        (
            repo.owner.to_ascii_lowercase(),
            repo.name.to_ascii_lowercase(),
            !row.worktree.is_main,
            row.worktree.path.clone(),
            row.worktree.branch.clone(),
        )
    });
    rows
}
