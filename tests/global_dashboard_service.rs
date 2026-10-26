#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use wisetree::config::schema::DashboardConfig;
use wisetree::services::global_dashboard::registry::RepositoryRegistry;
use wisetree::services::{
    DashboardNoticeLevel, DashboardRow, DashboardUpdate, DashboardWatch, GlobalDashboardService,
};

mod support;
use support::{git, init_repo_with_main};

struct Fixture {
    tmp: TempDir,
    roots: Vec<PathBuf>,
    registry: RepositoryRegistry,
}

impl Fixture {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let registry = RepositoryRegistry::new(
            tmp.path().join("registry.json"),
            tmp.path().join("cache.json"),
        );
        let mut roots = Vec::new();
        for owner in ["Alpha", "Beta"] {
            let root = tmp.path().join(owner).join("same");
            fs::create_dir_all(&root).unwrap();
            init_repo_with_main(&root);
            git(
                &root,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "initial",
                ],
            );
            git(
                &root,
                &[
                    "remote",
                    "add",
                    "origin",
                    &format!("https://github.com/{owner}/same.git"),
                ],
            );
            let child = tmp.path().join(owner).join("child");
            git(
                &root,
                &["worktree", "add", "-qb", "feature", child.to_str().unwrap()],
            );
            registry.register(&root).await.unwrap();
            roots.push(root.canonicalize().unwrap());
        }
        let fixture = Self {
            tmp,
            roots,
            registry,
        };
        fixture.write(
            "alpha.json",
            page(vec![
                pr("Alpha", "same", "feature", 1),
                pr("Alpha", "remote", "feature", 2),
            ]),
        );
        fixture.write("beta.json", page(vec![pr("Beta", "same", "feature", 3)]));
        fixture.write(
            "local",
            json!({"data":{"repository":{"b0":{"nodes":[]},"b1":{"nodes":[]}}}}),
        );
        fs::write(
            fixture.tmp.path().join("gh"),
            r#"#!/bin/sh
root=${0%/*}
printf '%s\n' "$*" >> "$root/calls"
case "$*" in
  --version) exit 0 ;;
  'api user') printf '{"login":"Viewer"}' ;;
  *user:Alpha*|*user:alpha*)
    if [ -f "$root/limited" ]; then echo 'API rate limit exceeded' >&2; exit 1; fi
    cat "$root/alpha.json" ;;
  *user:Beta*)
    if [ -f "$root/broken" ]; then echo 'HTTP 403: permission denied' >&2; exit 1; fi
    cat "$root/beta.json" ;;
  *graphql*)
    if [ -f "$root/local-broken" ]; then echo 'local PR API unavailable' >&2; exit 1; fi
    cat "$root/local" ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(
            fixture.tmp.path().join("gh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fixture
    }

    fn write(&self, name: &str, value: Value) {
        fs::write(self.tmp.path().join(name), value.to_string()).unwrap();
    }

    fn service(&self, accounts: &[&str], show_prs: bool) -> GlobalDashboardService {
        GlobalDashboardService::new(
            DashboardConfig {
                show_pull_requests: show_prs,
                refresh_interval_ms: 60_000,
                ..DashboardConfig::default()
            },
            accounts.iter().map(|s| s.to_string()).collect(),
        )
        .with_registry(self.registry.clone())
        .with_cache_path(Some(self.tmp.path().join("cache.json")))
        .with_gh_binary(self.tmp.path().join("gh"))
    }

    fn searches(&self) -> usize {
        fs::read_to_string(self.tmp.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("search(type:"))
            .count()
    }
}

fn pr(owner: &str, repository: &str, branch: &str, number: u64) -> Value {
    json!({"number":number,"url":format!("https://github.com/{owner}/{repository}/pull/{number}"),"state":"OPEN","title":"Feature","isDraft":false,"headRefName":branch,"repository":{"name":repository,"owner":{"login":owner,"__typename":"Organization"}}})
}

fn page(nodes: Vec<Value>) -> Value {
    json!({"data":{"search":{"nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}}}})
}

async fn enriched(watch: &mut DashboardWatch) -> (Vec<DashboardRow>, Option<std::time::Instant>) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let DashboardUpdate::WithPRs {
                rows,
                next_pr_fetch_at,
            } = watch.rx.recv().await.unwrap()
            {
                return (rows, next_pr_fetch_at);
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn tags_local_repositories_merges_authored_prs_and_orders_without_duplicates() {
    let fixture = Fixture::new().await;
    let rows = fixture.service(&["Beta", "alpha"], true).snapshot().await;
    assert_eq!(rows.len(), 5);
    let local: Vec<_> = rows
        .iter()
        .filter(|row| !row.worktree.path.is_empty())
        .collect();
    assert_eq!(local.len(), 4);
    for row in local {
        let repo = row.repository.as_ref().unwrap();
        assert_eq!(repo.name, "same");
        let index = usize::from(repo.owner == "Beta");
        assert_eq!(repo.local_root.as_ref(), Some(&fixture.roots[index]));
        assert!(row.last_commit.is_some());
        if row.worktree.branch == "feature" {
            assert!(row.pull_request.is_some());
        }
    }
    let remote = rows
        .iter()
        .find(|row| row.worktree.path.is_empty())
        .unwrap();
    assert!(remote.repository.as_ref().unwrap().local_root.is_none());
    assert!(remote.worktree.branch_status.is_none());
    assert!(remote.worktree.commit.is_empty());
    assert!(remote.last_commit.is_none());
    assert_eq!(remote.pull_request.as_ref().unwrap().number, 2);
    let keys: Vec<_> = rows
        .iter()
        .map(|row| {
            let repo = row.repository.as_ref().unwrap();
            (
                repo.owner.clone(),
                repo.name.clone(),
                !row.worktree.is_main,
                row.worktree.path.clone(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

#[tokio::test]
async fn selected_owners_and_empty_selection_do_not_leak_other_repositories() {
    let fixture = Fixture::new().await;
    let empty = fixture.service(&[], true).snapshot().await;
    assert!(empty.is_empty());
    assert!(!fixture.tmp.path().join("calls").exists());
    let rows = fixture.service(&["Alpha"], true).snapshot().await;
    assert_eq!(rows.len(), 3);
    assert!(rows
        .iter()
        .all(|row| row.repository.as_ref().unwrap().owner == "Alpha"));
    assert_eq!(fixture.searches(), 1);
}

#[tokio::test]
async fn watch_paints_git_then_enriched_and_reuses_cache_until_forced_refresh() {
    let fixture = Fixture::new().await;
    let service = fixture.service(&["Alpha", "Beta"], true);
    let mut watch = service.watch();
    let first = tokio::time::timeout(Duration::from_secs(15), watch.rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(first, DashboardUpdate::GitOnly(_)));
    assert_eq!(first.rows().len(), 4);
    let (rows, deadline) = enriched(&mut watch).await;
    assert_eq!(rows.len(), 5);
    assert!(deadline.is_some());
    assert_eq!(fixture.searches(), 2);
    drop(watch);
    // A slow collection on return cannot delay the cached paint.
    let mut watch = service.watch();
    let cached = tokio::time::timeout(Duration::from_secs(1), watch.rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cached.rows(), &rows);
    let (_, reused_deadline) = enriched(&mut watch).await;
    assert_eq!(reused_deadline, deadline);
    assert_eq!(fixture.searches(), 2);
    fixture.write("alpha.json", page(vec![]));
    watch.refresh();
    let (updated, _) = enriched(&mut watch).await;
    assert_eq!(fixture.searches(), 4);
    assert_eq!(updated.len(), 4); // Closed remote PR removed; local worktrees remain.
    assert!(updated.iter().all(|row| !row.worktree.path.is_empty()));
}

#[tokio::test]
async fn account_and_repository_api_failures_emit_one_warning_and_keep_other_rows() {
    let fixture = Fixture::new().await;
    fs::write(fixture.tmp.path().join("broken"), "").unwrap();
    fs::write(fixture.tmp.path().join("local-broken"), "").unwrap();
    let mut watch = fixture.service(&["Alpha", "Beta"], true).watch();
    let (rows, _) = enriched(&mut watch).await;
    assert_eq!(rows.len(), 5);
    let notice = tokio::time::timeout(Duration::from_secs(1), watch.notice_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(notice.level, DashboardNoticeLevel::Warning);
    assert!(notice.message.contains("Beta"));
    assert!(notice.message.contains("Alpha/same"));
    assert!(watch.notice_rx.try_recv().is_err());
}

#[tokio::test]
async fn rate_limit_keeps_cached_remote_rows_and_blocks_manual_search_for_five_minutes() {
    let fixture = Fixture::new().await;
    let service = fixture.service(&["Alpha"], true);
    let mut watch = service.watch();
    let (before, _) = enriched(&mut watch).await;
    fs::write(fixture.tmp.path().join("limited"), "").unwrap();
    watch.refresh();
    let (after, deadline) = enriched(&mut watch).await;
    let prs = |rows: &[DashboardRow]| {
        rows.iter()
            .map(|row| row.pull_request.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(prs(&after), prs(&before));
    assert!(deadline.unwrap().duration_since(std::time::Instant::now()) > Duration::from_secs(290));
    let notice = tokio::time::timeout(Duration::from_secs(1), watch.notice_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(notice.message.contains("5 min"));
    assert_eq!(fixture.searches(), 2);
    watch.refresh();
    enriched(&mut watch).await;
    assert_eq!(fixture.searches(), 2);
    assert!(watch.notice_rx.try_recv().is_err());
}

#[tokio::test]
async fn missing_or_disabled_gh_keeps_local_rows_without_remote_rows() {
    let fixture = Fixture::new().await;
    let rows = fixture.service(&["Alpha", "Beta"], false).snapshot().await;
    assert_eq!(rows.len(), 4);
    assert_eq!(fixture.searches(), 0);
    let service = fixture
        .service(&["Alpha", "Beta"], true)
        .with_gh_binary(fixture.tmp.path().join("missing"));
    let mut watch = service.watch();
    // The same selection replays the rows painted above, then collects its own.
    let replayed = next_update(&mut watch).await;
    assert_eq!(replayed.rows().len(), 4);
    let collected = next_update(&mut watch).await;
    assert_eq!(collected.rows().len(), 4);
    let warning = tokio::time::timeout(Duration::from_secs(1), watch.notice_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(warning.message, "gh CLI not found - PR column hidden.");
}

#[tokio::test]
async fn deleted_repository_is_pruned_on_refresh_and_warns_without_hiding_other_rows() {
    let fixture = Fixture::new().await;
    let service = fixture.service(&["Alpha", "Beta"], true);
    let mut watch = service.watch();
    enriched(&mut watch).await;
    fs::remove_dir_all(&fixture.roots[1]).unwrap();
    watch.refresh();
    let (rows, _) = enriched(&mut watch).await;
    assert_eq!(
        rows.iter()
            .filter(|row| !row.worktree.path.is_empty())
            .count(),
        2
    );
    let warning = tokio::time::timeout(Duration::from_secs(1), watch.notice_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(warning.message.contains("Beta/same"));
    assert_eq!(fixture.registry.resolve().await.unwrap().len(), 1);
}

#[tokio::test]
async fn optional_repository_identity_does_not_change_existing_json() {
    let fixture = Fixture::new().await;
    let mut row = fixture
        .service(&["Alpha"], false)
        .snapshot()
        .await
        .remove(0);
    row.repository = None;
    let json = serde_json::to_value(&row).unwrap();
    assert!(json.get("repository").is_none());
    assert_eq!(serde_json::from_value::<DashboardRow>(json).unwrap(), row);
}

#[tokio::test]
async fn local_pr_enrichment_keeps_its_cache_and_updates_closed_status() {
    use wisetree::services::PrState;

    let fixture = Fixture::new().await;
    let response = |state| {
        json!({"data":{"repository":{
            "b0":{"nodes":[]},
            "b1":{"nodes":[{"number":1,"state":state,"url":"https://github.com/Alpha/same/pull/1","title":"From per-repository enrichment","isDraft":false}]}
        }}})
    };
    fixture.write("local", response("OPEN"));
    let service = fixture.service(&["Alpha"], true);
    let mut watch = service.watch();
    let (rows, _) = enriched(&mut watch).await;
    let local_pr = |rows: Vec<DashboardRow>| {
        rows.into_iter()
            .find(|row| row.worktree.branch == "feature" && !row.worktree.path.is_empty())
            .unwrap()
            .pull_request
            .unwrap()
    };
    assert_eq!(local_pr(rows).title, "From per-repository enrichment");
    let count = || {
        fs::read_to_string(fixture.tmp.path().join("calls"))
            .unwrap()
            .lines()
            .filter(|line| line.contains("query=") && !line.contains("search(type:"))
            .count()
    };
    let before = count();
    service.snapshot().await;
    assert_eq!(
        count(),
        before,
        "unchanged SHAs reuse the per-repository PR cache"
    );
    let cache: Value =
        serde_json::from_slice(&fs::read(fixture.tmp.path().join("cache.json")).unwrap()).unwrap();
    assert_eq!(
        cache[fixture.roots[0].to_str().unwrap()]["feature"]["pullRequest"]["number"],
        1
    );
    assert!(cache[fixture.roots[0].to_str().unwrap()]["feature"]
        .get("repository")
        .is_none());
    fixture.write("alpha.json", page(vec![]));
    fixture.write("local", response("CLOSED"));
    watch.refresh();
    let (rows, _) = enriched(&mut watch).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(local_pr(rows).state, PrState::Closed);
    // Removing the worktree/branch cannot leave a phantom local or remote row.
    let child = fixture.roots[0].parent().unwrap().join("child");
    git(
        &fixture.roots[0],
        &["worktree", "remove", child.to_str().unwrap()],
    );
    git(&fixture.roots[0], &["branch", "-d", "feature"]);
    watch.refresh();
    let (rows, _) = enriched(&mut watch).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].worktree.is_main);
}

#[tokio::test]
async fn failed_account_retains_last_search_while_successful_accounts_replace_theirs() {
    let fixture = Fixture::new().await;
    fixture.write("beta.json", page(vec![pr("Beta", "remote", "other", 4)]));
    let mut watch = fixture.service(&["Alpha", "Beta"], true).watch();
    enriched(&mut watch).await;
    fixture.write("alpha.json", page(vec![]));
    fs::write(fixture.tmp.path().join("broken"), "").unwrap();
    watch.refresh();
    let (rows, _) = enriched(&mut watch).await;
    let remote: Vec<_> = rows
        .iter()
        .filter(|row| row.worktree.path.is_empty())
        .collect();
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].repository.as_ref().unwrap().owner, "Beta");
}

#[tokio::test]
async fn concurrent_repository_enrichment_is_bounded_and_all_disk_cache_entries_survive() {
    let fixture = Fixture::new().await;
    for index in 0..5 {
        let root = fixture.tmp.path().join(format!("extra{index}"));
        fs::create_dir(&root).unwrap();
        init_repo_with_main(&root);
        git(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        );
        git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                &format!("https://github.com/Alpha/extra{index}.git"),
            ],
        );
        fixture.registry.register(&root).await.unwrap();
    }
    let script = fixture.tmp.path().join("gh");
    let original = fs::read_to_string(&script).unwrap();
    fs::write(&script, original.replace("  *graphql*)", "  *graphql*)\n    printf 'start\\n' >> \"$root/events\"\n    sleep 0.1\n    printf 'end\\n' >> \"$root/events\"" )).unwrap();
    let rows = fixture.service(&["Alpha", "Beta"], true).snapshot().await;
    assert_eq!(rows.len(), 10);
    let events = fs::read_to_string(fixture.tmp.path().join("events")).unwrap();
    let (mut active, mut maximum, mut starts) = (0, 0, 0);
    for event in events.lines() {
        match event {
            "start" => {
                active += 1;
                starts += 1;
                maximum = maximum.max(active);
            }
            "end" => active -= 1,
            _ => panic!("unexpected event: {event}"),
        }
    }
    assert_eq!(starts, 7);
    assert_eq!(active, 0);
    assert!(
        maximum > 1 && maximum <= 4,
        "maximum concurrency: {maximum}"
    );
    let cache: Value =
        serde_json::from_slice(&fs::read(fixture.tmp.path().join("cache.json")).unwrap()).unwrap();
    assert_eq!(cache.as_object().unwrap().len(), 7);
}

async fn next_update(watch: &mut DashboardWatch) -> DashboardUpdate {
    tokio::time::timeout(Duration::from_secs(15), watch.rx.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn cold_watch_opens_on_the_last_local_rows_painted_for_the_same_accounts() {
    let fixture = Fixture::new().await;
    let painted = fixture.service(&["Alpha", "Beta"], true).snapshot().await;
    assert_eq!(painted.len(), 5);
    // Remote-only rows are never replayed from disk.
    let saved: Vec<_> = painted
        .iter()
        .filter(|row| !row.worktree.path.is_empty())
        .cloned()
        .collect();
    // Change local state after the paint: only a fresh collection sees it.
    fs::write(fixture.roots[0].join("dirty.txt"), "").unwrap();
    let dirty = |rows: &[DashboardRow]| rows.iter().any(|row| !row.worktree.is_clean);
    let mut watch = fixture.service(&["beta", "ALPHA"], true).watch();
    let first = next_update(&mut watch).await;
    assert!(matches!(first, DashboardUpdate::GitOnly(_)));
    assert_eq!(first.rows(), &saved);
    assert!(!dirty(first.rows()));
    let collected = next_update(&mut watch).await;
    assert_eq!(collected.rows().len(), 4);
    assert!(dirty(collected.rows()));
    let mut other = fixture.service(&["Alpha"], true).watch();
    let first = next_update(&mut other).await;
    assert_eq!(first.rows().len(), 2);
    assert!(dirty(first.rows()));
}

#[tokio::test]
async fn ticks_reuse_the_resolved_registry_until_a_manual_refresh() {
    let fixture = Fixture::new().await;
    let service = fixture.service(&["Alpha"], false);
    assert_eq!(service.snapshot().await.len(), 2);
    let root = fixture.tmp.path().join("extra");
    fs::create_dir(&root).unwrap();
    init_repo_with_main(&root);
    git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ],
    );
    git(
        &root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/Alpha/extra.git",
        ],
    );
    fixture.registry.register(&root).await.unwrap();
    assert_eq!(service.snapshot().await.len(), 2);
    let mut watch = service.watch();
    watch.refresh();
    tokio::time::timeout(Duration::from_secs(15), async {
        while next_update(&mut watch).await.rows().len() != 3 {}
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn forgotten_worktrees_are_absent_from_warm_and_cold_watch_replays() {
    let fixture = Fixture::new().await;
    let service = fixture.service(&["Alpha"], true);
    let mut watch = service.watch();
    let (rows, _) = enriched(&mut watch).await;
    let path = rows
        .iter()
        .find(|row| !row.worktree.is_main && !row.worktree.path.is_empty())
        .unwrap()
        .worktree
        .path
        .clone();
    drop(watch);
    git(&fixture.roots[0], &["worktree", "remove", &path]);
    service.forget_worktrees(std::slice::from_ref(&path)).await;

    // Read the persisted snapshot before a live refresh can replace it.
    let mut cold = fixture.service(&["Alpha"], true).watch();
    let first = next_update(&mut cold).await;
    assert_eq!(first.rows().len(), 1);
    assert!(first.rows().iter().all(|row| row.worktree.path != path));
    drop(cold);

    let mut warm = service.watch();
    let first = next_update(&mut warm).await;
    assert_eq!(first.rows().len(), rows.len() - 1);
    assert!(first.rows().iter().all(|row| row.worktree.path != path));
    // The still-open authored PR can return only as a live remote row.
    let (live, _) = enriched(&mut warm).await;
    let pr = live
        .iter()
        .find(|row| row.pull_request.as_ref().is_some_and(|pr| pr.number == 1))
        .unwrap();
    assert!(pr.worktree.path.is_empty());
}

#[tokio::test]
async fn watch_forgets_before_its_first_replay_even_when_snapshot_cannot_be_written() {
    let fixture = Fixture::new().await;
    let service = fixture.service(&["Alpha"], false);
    let rows = service.snapshot().await;
    let path = rows
        .iter()
        .find(|row| !row.worktree.is_main)
        .unwrap()
        .worktree
        .path
        .clone();
    let snapshot = fixture
        .tmp
        .path()
        .join(wisetree::constants::GLOBAL_DASHBOARD_ROWS_FILE_NAME);
    fs::remove_file(&snapshot).unwrap();
    fs::create_dir(&snapshot).unwrap();
    let mut watch = service.watch_forgetting(vec![path.clone()]);
    let first = next_update(&mut watch).await;
    assert_eq!(first.rows().len(), rows.len() - 1);
    assert!(first.rows().iter().all(|row| row.worktree.path != path));
}
