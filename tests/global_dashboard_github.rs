#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use wisetree::services::global_dashboard::github::{AccountKind, GitHubError, GitHubService};
use wisetree::services::global_dashboard::registry::LocalRepository;
use wisetree::services::{CheckStatus, MergeStatus, PrState, ReviewStatus};

struct Fixture(TempDir);

impl Fixture {
    fn new() -> Self {
        let fixture = Self(tempfile::tempdir().unwrap());
        fixture.write("viewer", json!({"login": "Viewer"}));
        fixture.write("orgs", json!([{"login":"Alpha"}, {"login":"VIEWER"}]));
        fixture.write("discovery", page(vec![pr("Alpha", 1), pr("Beta", 2)], None));
        fixture.write("alpha1", page(vec![pr("Alpha", 1)], Some("next-page")));
        fixture.write("alpha2", page(vec![pr("Alpha", 3)], None));
        fixture.write("beta", page(vec![pr("Beta", 2)], None));
        fixture.write("empty", page(vec![], None));
        fixture.write(
            "owners",
            owners(&[
                ("Beta", "Organization"),
                ("Local", "Organization"),
                ("Alpha", "Organization"),
                ("Saved", "User"),
            ]),
        );
        let script = fixture.0.path().join("fake-gh");
        fs::write(
            &script,
            r#"#!/bin/sh
root=${0%/*}
printf '%s\n' "$*" >> "$root/calls"
if [ -f "$root/slow" ]; then
  printf 'start\n' >> "$root/events"
  sleep 0.2
  printf 'end\n' >> "$root/events"
fi
if [ "$2" = --cache ]; then shift 3; set -- api "$@"; fi
case "$*" in
  'api user') cat "$root/viewer" ;;
  'api user/orgs?'*)
    if [ -f "$root/org-error" ]; then cat "$root/org-error" >&2; exit 1; fi
    cat "$root/orgs" ;;
  *repositoryOwner*) cat "$root/owners" ;;
  *user:Alpha*)
    case "$*" in
      *'after: "next-page"'*)
        if [ -f "$root/page-error" ]; then cat "$root/page-error" >&2; exit 1; fi
        cat "$root/alpha2" ;;
      *) cat "$root/alpha1" ;;
    esac ;;
  *user:Beta*) cat "$root/beta" ;;
  *user:Empty*) cat "$root/empty" ;;
  *user:Broken*) echo 'HTTP 404: account unavailable' >&2; exit 1 ;;
  *user:Limited*) echo 'GraphQL: API rate limit exceeded' >&2; exit 1 ;;
  *user:Expired*) echo 'HTTP 401: Bad credentials' >&2; exit 1 ;;
  *graphql*) cat "$root/discovery" ;;
  *) echo 'unexpected fake gh command' >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
        fixture
    }

    fn write(&self, name: &str, value: Value) {
        fs::write(self.0.path().join(name), value.to_string()).unwrap();
    }

    fn service(&self) -> GitHubService {
        GitHubService::default().with_gh_binary(self.0.path().join("fake-gh"))
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.0.path().join("calls")).unwrap()
    }
}

fn pr(owner: &str, number: u64) -> Value {
    json!({
        "number":number,"url":format!("https://github.com/{owner}/same/pull/{number}"),
        "title":"Feature", "state":"OPEN", "isDraft":false,"headRefName":"feature",
        "baseRefName":"main", "baseRepository":{"nameWithOwner":format!("{owner}/same")},
        "headRefOid":"abc", "headRepository":{"nameWithOwner":"Viewer/fork"},
        "repository":{"name":"same","isArchived":false,"owner":{"login":owner,"__typename":"Organization"}},
        "mergeStateStatus":"CLEAN","reviewDecision":"APPROVED",
        "reviewRequests":{"totalCount":0,"nodes":[]},
        "latestOpinionatedReviews":{"nodes":[]},
        "latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"Reviewer"}}]},
        "labels":{"nodes":[{"name":"feature"}]},
        "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[
            {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"}
        ]}}}}]}
    })
}

/// Aliased `repositoryOwner` answers, in the order the logins were requested.
fn owners(accounts: &[(&str, &str)]) -> Value {
    let data: serde_json::Map<_, _> = accounts
        .iter()
        .enumerate()
        .map(|(index, (login, kind))| {
            let owner = if kind.is_empty() {
                Value::Null
            } else {
                json!({"login": login, "__typename": kind})
            };
            (format!("a{index}"), owner)
        })
        .collect();
    json!({ "data": data })
}

fn page(nodes: Vec<Value>, next: Option<&str>) -> Value {
    json!({"data":{"search":{"nodes":nodes,"pageInfo":{"hasNextPage":next.is_some(),"endCursor":next}}}})
}

#[tokio::test]
async fn accounts_follow_source_order_and_case_insensitive_deduplication() {
    let fixture = Fixture::new();
    let local = LocalRepository {
        git_common_dir: "/unused".into(),
        mother_worktree: "/unused".into(),
        github_slugs: vec!["beta/same".into(), "Local/same".into()],
        pr_hosting_slug: None,
    };
    let result = fixture
        .service()
        .discover_accounts(&[local], &["alpha".into(), "Saved".into()])
        .await
        .unwrap();
    assert_eq!(result.viewer, "Viewer");
    assert_eq!(
        result
            .accounts
            .iter()
            .map(|account| account.login.as_str())
            .collect::<Vec<_>>(),
        ["Viewer", "Alpha", "Beta", "Local", "Saved"]
    );
    assert_eq!(
        result
            .accounts
            .iter()
            .map(|account| account.kind)
            .collect::<Vec<_>>(),
        [
            AccountKind::User,
            AccountKind::Organization,
            AccountKind::Organization,
            AccountKind::Organization,
            AccountKind::User
        ]
    );
    assert!(result.warnings.is_empty());
    let calls = fixture.calls();
    let lookups: Vec<_> = calls
        .lines()
        .filter(|line| line.contains("repositoryOwner"))
        .collect();
    assert_eq!(lookups.len(), 1, "stored logins resolve in one request");
    for login in ["beta", "Local", "alpha", "Saved"] {
        assert!(lookups[0].contains(&format!("repositoryOwner(login: \"{login}\")")));
    }
    assert!(!calls.contains("api users/"));
}

#[tokio::test]
async fn discovery_searches_only_owner_fields_and_sends_its_requests_concurrently() {
    let fixture = Fixture::new();
    fs::write(fixture.0.path().join("slow"), "").unwrap();
    fixture.write("owners", owners(&[("Saved", "User")]));
    let result = fixture
        .service()
        .discover_accounts(&[], &["Saved".into()])
        .await
        .unwrap();
    assert_eq!(result.accounts.len(), 4);
    let calls = fixture.calls();
    let search = calls
        .lines()
        .find(|line| line.contains("search(type:"))
        .unwrap();
    assert!(search.contains("is:pr is:open archived:false author:@me"));
    assert!(!search.contains("statusCheckRollup"));
    let events = fs::read_to_string(fixture.0.path().join("events")).unwrap();
    let (mut running, mut maximum) = (0, 0);
    for event in events.lines() {
        match event {
            "start" => {
                running += 1;
                maximum = maximum.max(running);
            }
            "end" => running -= 1,
            _ => panic!("unexpected event: {event}"),
        }
    }
    assert!(maximum > 1, "maximum concurrency: {maximum}");
}

#[tokio::test]
async fn cached_discovery_reads_every_request_through_the_gh_cache() {
    let fixture = Fixture::new();
    fixture.write("owners", owners(&[("Saved", "User")]));
    let result = fixture
        .service()
        .with_cache(Duration::from_secs(600))
        .discover_accounts(&[], &["Saved".into()])
        .await
        .unwrap();
    assert_eq!(result.accounts.len(), 4);
    let calls = fixture.calls();
    assert_eq!(calls.lines().count(), 4);
    assert!(calls
        .lines()
        .all(|line| line.starts_with("api --cache 600s ")));
}

#[tokio::test]
async fn unknown_stored_accounts_warn_without_hiding_the_others() {
    let fixture = Fixture::new();
    fixture.write("owners", owners(&[("Saved", "User"), ("Gone", "")]));
    let result = fixture
        .service()
        .discover_accounts(&[], &["Saved".into(), "Gone".into()])
        .await
        .unwrap();
    assert_eq!(
        result
            .accounts
            .iter()
            .map(|account| account.login.as_str())
            .collect::<Vec<_>>(),
        ["Viewer", "Alpha", "Beta", "Saved"]
    );
    assert_eq!(result.warnings.len(), 1);
    assert!(result.warnings[0].to_string().contains("Gone"));
}

#[tokio::test]
async fn searches_viewer_authorship_per_owner_and_follows_cursors_with_shared_pr_mapping() {
    let fixture = Fixture::new();
    let result = fixture
        .service()
        .fetch_authored_pull_requests(&["Alpha".into(), "Beta".into(), "ALPHA".into()])
        .await
        .unwrap();
    assert!(result.failures.is_empty());
    assert_eq!(
        result
            .pull_requests
            .iter()
            .map(|pr| (
                pr.owner.login.as_str(),
                pr.repository.as_str(),
                pr.pull_request.number
            ))
            .collect::<Vec<_>>(),
        [
            ("Alpha", "same", 1),
            ("Alpha", "same", 3),
            ("Beta", "same", 2)
        ]
    );
    let item = &result.pull_requests[0];
    assert_eq!(item.head_branch, "feature");
    let pr = &item.pull_request;
    assert_eq!(pr.state, PrState::Open);
    assert_eq!(pr.checks_status, Some(CheckStatus::Passed));
    assert_eq!(pr.review_status, Some(ReviewStatus::Approved));
    assert_eq!(pr.merge_status, Some(MergeStatus::Clean));
    assert_eq!(pr.base_repository.as_deref(), Some("Alpha/same"));
    assert_eq!(pr.head_repository.as_deref(), Some("Viewer/fork"));
    assert_eq!(pr.head_ref_oid.as_deref(), Some("abc"));
    assert_eq!(pr.labels, ["feature"]);
    assert_eq!(pr.reviewers.approved, ["Reviewer"]);
    let calls = fixture.calls();
    let searches: Vec<_> = calls
        .lines()
        .filter(|line| line.contains("graphql"))
        .collect();
    assert_eq!(searches.len(), 3);
    assert!(searches
        .iter()
        .all(|query| query.contains("is:pr is:open archived:false author:Viewer user:")));
    assert_eq!(
        searches
            .iter()
            .filter(|query| query.contains("user:Alpha"))
            .count(),
        2
    );
    assert_eq!(
        searches
            .iter()
            .filter(|query| query.contains("user:Beta"))
            .count(),
        1
    );
    assert!(searches
        .iter()
        .any(|query| query.contains("after: \"next-page\"")));
    assert!(searches
        .iter()
        .all(|query| query.contains("latestOpinionatedReviews")
            && query.contains("statusCheckRollup")));
}

#[tokio::test]
async fn forbidden_organizations_warn_but_keep_other_discovery_sources() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("org-error"),
        "HTTP 403: requires read:org or SSO authorization",
    )
    .unwrap();
    fixture.write("owners", owners(&[("Saved", "User")]));
    let result = fixture
        .service()
        .discover_accounts(&[], &["Saved".into()])
        .await
        .unwrap();
    assert_eq!(
        result
            .accounts
            .iter()
            .map(|account| account.login.as_str())
            .collect::<Vec<_>>(),
        ["Viewer", "Alpha", "Beta", "Saved"]
    );
    assert_eq!(result.warnings.len(), 1);
    assert!(result.warnings[0].to_string().contains("read:org"));
}

#[tokio::test]
async fn account_failures_preserve_other_results_and_classify_rate_limits_and_authentication() {
    let fixture = Fixture::new();
    let result = fixture
        .service()
        .fetch_authored_pull_requests(&[
            "Broken".into(),
            "Beta".into(),
            "Limited".into(),
            "Expired".into(),
            "Empty".into(),
        ])
        .await
        .unwrap();
    assert_eq!(result.pull_requests.len(), 1);
    assert_eq!(result.pull_requests[0].owner.login, "Beta");
    assert_eq!(result.failures.len(), 3);
    assert_eq!(result.failures[0].account, "Broken");
    assert!(matches!(
        result.failures[1].error,
        GitHubError::RateLimited(_)
    ));
    assert!(matches!(
        result.failures[2].error,
        GitHubError::Authentication(_)
    ));
    assert!(result.failures[2]
        .error
        .to_string()
        .contains("gh auth login"));
}

#[tokio::test]
async fn missing_cli_and_expired_viewer_are_actionable_errors() {
    let fixture = Fixture::new();
    let error = GitHubService::default()
        .with_gh_binary(fixture.0.path().join("missing"))
        .discover_accounts(&[], &[])
        .await
        .unwrap_err();
    assert_eq!(error, GitHubError::NotInstalled);
    assert!(error.to_string().contains("Install gh"));
    let script = fixture.0.path().join("expired");
    fs::write(
        &script,
        "#!/bin/sh\necho 'HTTP 401: Bad credentials' >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let service = GitHubService::default().with_gh_binary(script);
    assert!(matches!(
        service.discover_accounts(&[], &[]).await.unwrap_err(),
        GitHubError::Authentication(_)
    ));
    assert!(matches!(
        service
            .fetch_authored_pull_requests(&["Alpha".into()])
            .await
            .unwrap_err(),
        GitHubError::Authentication(_)
    ));
}

#[tokio::test]
async fn archived_repositories_are_excluded_and_empty_accounts_succeed() {
    let fixture = Fixture::new();
    let mut archived = pr("Beta", 4);
    archived["repository"]["isArchived"] = json!(true);
    fixture.write("beta", page(vec![archived], None));
    let result = fixture
        .service()
        .fetch_authored_pull_requests(&["Beta".into(), "Empty".into()])
        .await
        .unwrap();
    assert!(result.pull_requests.is_empty());
    assert!(result.failures.is_empty());
    let result = fixture
        .service()
        .fetch_authored_pull_requests(&[])
        .await
        .unwrap();
    assert!(result.pull_requests.is_empty());
}

#[tokio::test]
async fn later_page_failure_keeps_completed_pages_and_graphql_errors_are_recognized() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("page-error"),
        "HTTP 429: secondary rate-limit",
    )
    .unwrap();
    fixture.write(
        "beta",
        json!({"errors":[{"message":"API rate limit exceeded"}]}),
    );
    let result = fixture
        .service()
        .fetch_authored_pull_requests(&["Alpha".into(), "Beta".into()])
        .await
        .unwrap();
    assert_eq!(result.pull_requests.len(), 1);
    assert_eq!(result.failures.len(), 2);
    assert!(result
        .failures
        .iter()
        .all(|failure| matches!(failure.error, GitHubError::RateLimited(_))));
}

#[tokio::test]
async fn account_searches_have_bounded_concurrency() {
    let fixture = Fixture::new();
    let script = fixture.0.path().join("fake-gh");
    fs::write(
        &script,
        r#"#!/bin/sh
root=${0%/*}
if [ "$2" = user ]; then cat "$root/viewer"; exit 0; fi
printf 'start\n' >> "$root/events"
sleep 0.1
printf 'end\n' >> "$root/events"
cat "$root/empty"
"#,
    )
    .unwrap();
    let accounts = (0..9)
        .map(|index| format!("Account{index}"))
        .collect::<Vec<_>>();
    let result = fixture
        .service()
        .fetch_authored_pull_requests(&accounts)
        .await
        .unwrap();
    assert!(result.failures.is_empty());
    let events = fs::read_to_string(fixture.0.path().join("events")).unwrap();
    let mut running = 0;
    let mut maximum = 0;
    let mut starts = 0;
    for event in events.lines() {
        match event {
            "start" => {
                running += 1;
                starts += 1;
                maximum = maximum.max(running);
            }
            "end" => running -= 1,
            _ => panic!("unexpected event: {event}"),
        }
    }
    assert_eq!(starts, 9);
    assert_eq!(running, 0);
    assert!(
        maximum > 1 && maximum <= 4,
        "maximum concurrency: {maximum}"
    );
}

#[tokio::test]
async fn account_names_cannot_inject_additional_search_qualifiers() {
    let fixture = Fixture::new();
    let result = fixture
        .service()
        .fetch_authored_pull_requests(&["Alpha author:Other".into(), "Beta".into()])
        .await
        .unwrap();
    assert_eq!(result.failures.len(), 1);
    assert_eq!(result.pull_requests.len(), 1);
    assert!(!fixture.calls().contains("author:Other"));
}
