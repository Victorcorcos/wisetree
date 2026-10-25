//! Authenticated GitHub discovery and cross-repository authored PR searches.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::task::JoinSet;

use super::registry::LocalRepository;
use crate::messages;
use crate::services::dashboard::{
    is_rate_limit_error, map_pull_request, GhNode, PullRequest, PR_GRAPHQL_FIELDS,
};

const SEARCH_CONCURRENCY: usize = 4;
/// Open authored PRs in unarchived repositories; `@me` needs no viewer lookup first.
const AUTHORED_PULL_REQUESTS: &str = "is:pr is:open archived:false author:@me";

// Account discovery reads through gh's on-disk response cache, keyed by
// request and token: opening the picker renders cached accounts at once, then
// revalidates them in the background.
/// Oldest cached response the picker renders immediately.
pub const DISCOVERY_CACHE_MAX_STALE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Cached responses older than this are refetched by the background revalidation.
pub const DISCOVERY_CACHE_MAX_AGE: Duration = Duration::from_secs(10 * 60);
/// An explicit refresh: effectively a fresh fetch that still updates the cache.
pub const DISCOVERY_CACHE_REFRESH: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountKind {
    User,
    Organization,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubAccount {
    pub login: String,
    pub kind: AccountKind,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitHubError {
    #[error("{}", messages::GLOBAL_GH_INSTALL)]
    NotInstalled,
    #[error("{}: {0}", messages::GLOBAL_GH_AUTH)]
    Authentication(String),
    #[error("GitHub API rate limited: {0}")]
    RateLimited(String),
    #[error("{0}")]
    Request(String),
}

impl GitHubError {
    fn response(message: String) -> Self {
        let lower = message.to_ascii_lowercase();
        if is_rate_limit_error(&message) || lower.contains("429") {
            Self::RateLimited(message)
        } else if lower.contains("401")
            || lower.contains("bad credentials")
            || lower.contains("authentication")
            || lower.contains("gh auth login")
            || lower.contains("token has expired")
        {
            Self::Authentication(message)
        } else {
            Self::Request(message)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredPullRequest {
    pub owner: GitHubAccount,
    pub repository: String,
    pub head_branch: String,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct AccountDiscovery {
    pub viewer: String,
    pub accounts: Vec<GitHubAccount>,
    pub warnings: Vec<GitHubError>,
}

#[derive(Debug)]
pub struct AccountSearchFailure {
    pub account: String,
    pub error: GitHubError,
}

#[derive(Debug, Default)]
pub struct AuthoredPullRequests {
    pub pull_requests: Vec<AuthoredPullRequest>,
    pub failures: Vec<AccountSearchFailure>,
}

#[derive(Debug, Clone)]
pub struct GitHubService {
    gh_binary: PathBuf,
    cache_ttl: Option<Duration>,
}

impl Default for GitHubService {
    fn default() -> Self {
        Self {
            gh_binary: "gh".into(),
            cache_ttl: None,
        }
    }
}

impl GitHubService {
    pub fn with_gh_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.gh_binary = binary.into();
        self
    }

    /// Serve responses from gh's cache (`gh api --cache`) while younger than
    /// `ttl`; any request it has to send refreshes the cached entry.
    pub fn with_cache(mut self, ttl: Duration) -> Self {
        self.cache_ttl = Some(ttl);
        self
    }

    async fn api(&self, args: &[&str]) -> Result<Value, GitHubError> {
        let mut command = Command::new(&self.gh_binary);
        command.arg("api");
        if let Some(ttl) = self.cache_ttl {
            command
                .arg("--cache")
                .arg(format!("{}s", ttl.as_secs().max(1)));
        }
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            command.args(args).kill_on_drop(true).output(),
        )
        .await
        .map_err(|_| GitHubError::Request("GitHub request timed out after 15s".into()))?
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                GitHubError::NotInstalled
            } else {
                GitHubError::Request(error.to_string())
            }
        })?;
        if !output.status.success() {
            return Err(GitHubError::response(
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stderr),
                    String::from_utf8_lossy(&output.stdout)
                )
                .trim()
                .to_string(),
            ));
        }
        let value: Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| GitHubError::Request(format!("Invalid GitHub response: {error}")))?;
        if let Some(errors) = value.get("errors").and_then(Value::as_array) {
            if !errors.is_empty() {
                return Err(GitHubError::response(
                    errors
                        .iter()
                        .map(|error| error["message"].as_str().unwrap_or("Unknown GitHub error"))
                        .collect::<Vec<_>>()
                        .join("; "),
                ));
            }
        }
        Ok(value)
    }

    async fn viewer(&self) -> Result<String, GitHubError> {
        let user = self.api(&["user"]).await?;
        let login = required_string(&user, "login")?;
        validate_login(&login)?;
        Ok(login)
    }

    /// Sources are kept in priority order, preserving the first spelling and type.
    pub async fn discover_accounts(
        &self,
        local_repositories: &[LocalRepository],
        selected_accounts: &[String],
    ) -> Result<AccountDiscovery, GitHubError> {
        let mut stored_logins: Vec<String> = Vec::new();
        for login in local_repositories
            .iter()
            .flat_map(|repo| repo.github_slugs.iter())
            .filter_map(|slug| slug.split_once('/').map(|(owner, _)| owner.to_string()))
            .chain(selected_accounts.iter().cloned())
        {
            validate_login(&login)?;
            if !stored_logins
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&login))
            {
                stored_logins.push(login);
            }
        }
        // The sources are independent, so discovery takes as long as the
        // slowest request instead of the sum of every request.
        let (viewer, organizations, (authored, search_failure), stored) = tokio::join!(
            self.viewer(),
            self.organizations(),
            self.search(
                AUTHORED_PULL_REQUESTS,
                "repository { isArchived owner { __typename login } }",
                |node| parse_account(&node["repository"]["owner"]),
            ),
            self.resolve_accounts(&stored_logins),
        );
        let viewer = viewer?;
        let mut result = AccountDiscovery {
            viewer: viewer.clone(),
            accounts: vec![GitHubAccount {
                login: viewer.clone(),
                kind: AccountKind::User,
            }],
            warnings: Vec::new(),
        };
        let mut seen = HashSet::from([viewer.to_ascii_lowercase()]);
        match organizations {
            Ok(accounts) => {
                for account in accounts {
                    add_account(&mut result.accounts, &mut seen, account);
                }
            }
            Err(error) => result.warnings.push(into_warning(error)?),
        }
        for account in authored {
            add_account(&mut result.accounts, &mut seen, account);
        }
        if let Some(error) = search_failure {
            result.warnings.push(into_warning(error)?);
        }
        match stored {
            Ok(resolved) => {
                for (login, account) in resolved {
                    match account {
                        Some(account) => add_account(&mut result.accounts, &mut seen, account),
                        None if !seen.contains(&login.to_ascii_lowercase()) => {
                            result.warnings.push(GitHubError::Request(format!(
                                "GitHub account not found: {login}"
                            )))
                        }
                        None => {}
                    }
                }
            }
            Err(error) => result.warnings.push(into_warning(error)?),
        }
        Ok(result)
    }

    async fn organizations(&self) -> Result<Vec<GitHubAccount>, GitHubError> {
        let mut accounts = Vec::new();
        let mut page = 1;
        loop {
            let value = self
                .api(&[&format!("user/orgs?per_page=100&page={page}")])
                .await?;
            let orgs = value.as_array().ok_or_else(|| {
                GitHubError::Request("Invalid GitHub organizations response".into())
            })?;
            for org in orgs {
                accounts.push(GitHubAccount {
                    login: required_string(org, "login")?,
                    kind: AccountKind::Organization,
                });
            }
            if orgs.len() < 100 {
                return Ok(accounts);
            }
            page += 1;
        }
    }

    /// Local remotes and saved selections store logins only. Resolve their
    /// account types in one aliased GraphQL request; unknown logins are `None`.
    async fn resolve_accounts(
        &self,
        logins: &[String],
    ) -> Result<Vec<(String, Option<GitHubAccount>)>, GitHubError> {
        if logins.is_empty() {
            return Ok(Vec::new());
        }
        let owners: String = logins
            .iter()
            .enumerate()
            .map(|(index, login)| {
                format!(
                    "a{index}: repositoryOwner(login: {}) {{ __typename login }} ",
                    json!(login)
                )
            })
            .collect();
        let value = self
            .api(&["graphql", "-f", &format!("query=query {{ {owners}}}")])
            .await?;
        logins
            .iter()
            .enumerate()
            .map(|(index, login)| {
                let owner = &value["data"][format!("a{index}")];
                let account = (!owner.is_null())
                    .then(|| parse_account(owner))
                    .transpose()?;
                Ok((login.clone(), account))
            })
            .collect()
    }

    pub async fn fetch_authored_pull_requests(
        &self,
        selected_accounts: &[String],
    ) -> Result<AuthoredPullRequests, GitHubError> {
        let viewer = self.viewer().await?;
        let mut seen = HashSet::new();
        let accounts: Vec<_> = selected_accounts
            .iter()
            .filter(|account| seen.insert(account.to_ascii_lowercase()))
            .cloned()
            .collect();
        let mut result = AuthoredPullRequests::default();
        // Chunking bounds both running tasks and queued work. Preserve selection order.
        for chunk in accounts.chunks(SEARCH_CONCURRENCY) {
            let mut tasks = JoinSet::new();
            for (index, account) in chunk.iter().enumerate() {
                let service = self.clone();
                let viewer = viewer.clone();
                let account = account.clone();
                tasks.spawn(async move {
                    let (prs, failure) = service.authored_pull_requests(&viewer, &account).await;
                    (index, account, prs, failure)
                });
            }
            let mut completed = Vec::new();
            while let Some(task) = tasks.join_next().await {
                completed.push(task.map_err(|error| GitHubError::Request(error.to_string()))?);
            }
            completed.sort_by_key(|(index, ..)| *index);
            for (_, account, prs, failure) in completed {
                result.pull_requests.extend(prs);
                if let Some(error) = failure {
                    result
                        .failures
                        .push(AccountSearchFailure { account, error });
                }
            }
        }
        Ok(result)
    }

    async fn authored_pull_requests(
        &self,
        viewer: &str,
        owner: &str,
    ) -> (Vec<AuthoredPullRequest>, Option<GitHubError>) {
        if let Err(error) = validate_login(owner) {
            return (Vec::new(), Some(error));
        }
        self.search(
            &format!("is:pr is:open archived:false author:{viewer} user:{owner}"),
            &format!(
                "headRefName repository {{ name isArchived owner {{ __typename login }} }} {PR_GRAPHQL_FIELDS}"
            ),
            parse_authored_pull_request,
        )
        .await
    }

    /// Pages through a pull request search, selecting only `fields` per node.
    /// Keeps completed pages when a later page fails.
    async fn search<T>(
        &self,
        query_text: &str,
        fields: &str,
        parse: fn(&Value) -> Result<T, GitHubError>,
    ) -> (Vec<T>, Option<GitHubError>) {
        let mut items = Vec::new();
        let mut cursor = Value::Null;
        loop {
            let query = format!(
                "query {{ search(type: ISSUE, first: 100, query: {}, after: {}) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ ... on PullRequest {{ {fields} }} }} }} }}",
                json!(query_text), cursor
            );
            let page = match self
                .api(&["graphql", "-f", &format!("query={query}")])
                .await
            {
                Ok(value) => value,
                Err(error) => return (items, Some(error)),
            };
            let (page_items, next_cursor) = match parse_search_page(&page, parse) {
                Ok(parsed) => parsed,
                Err(error) => return (items, Some(error)),
            };
            items.extend(page_items);
            match next_cursor {
                Some(next) if cursor != next => cursor = next,
                Some(_) => {
                    return (
                        items,
                        Some(GitHubError::Request(
                            "GitHub search repeated its pagination cursor".into(),
                        )),
                    )
                }
                None => return (items, None),
            }
        }
    }
}

/// A failed discovery source only warns, unless no request can succeed.
fn into_warning(error: GitHubError) -> Result<GitHubError, GitHubError> {
    match error {
        GitHubError::Authentication(_) | GitHubError::NotInstalled => Err(error),
        error => Ok(error),
    }
}

fn add_account(
    accounts: &mut Vec<GitHubAccount>,
    seen: &mut HashSet<String>,
    account: GitHubAccount,
) {
    if seen.insert(account.login.to_ascii_lowercase()) {
        accounts.push(account);
    }
}

// Account names become search qualifiers and REST path components.
fn validate_login(login: &str) -> Result<(), GitHubError> {
    if login.is_empty()
        || !login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(GitHubError::Request(format!(
            "Invalid GitHub account: {login}"
        )));
    }
    Ok(())
}

fn required_string(value: &Value, field: &str) -> Result<String, GitHubError> {
    value[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| GitHubError::Request(format!("GitHub response missing {field}")))
}

fn parse_account(value: &Value) -> Result<GitHubAccount, GitHubError> {
    let kind = match value["__typename"]
        .as_str()
        .or_else(|| value["type"].as_str())
    {
        Some("Organization") => AccountKind::Organization,
        Some("User") => AccountKind::User,
        _ => return Err(GitHubError::Request("Unknown GitHub account type".into())),
    };
    Ok(GitHubAccount {
        login: required_string(value, "login")?,
        kind,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchPageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

fn parse_search_page<T>(
    page: &Value,
    parse: fn(&Value) -> Result<T, GitHubError>,
) -> Result<(Vec<T>, Option<Value>), GitHubError> {
    let search = &page["data"]["search"];
    let nodes = search["nodes"]
        .as_array()
        .ok_or_else(|| GitHubError::Request("GitHub response missing search nodes".into()))?;
    let info: SearchPageInfo = serde_json::from_value(search["pageInfo"].clone())
        .map_err(|error| GitHubError::Request(format!("Invalid GitHub pagination: {error}")))?;
    let mut items = Vec::new();
    for node in nodes {
        if node["repository"]["isArchived"].as_bool() == Some(true) {
            continue;
        }
        items.push(parse(node)?);
    }
    let cursor = if info.has_next_page {
        Some(json!(info
            .end_cursor
            .filter(|cursor| !cursor.is_empty())
            .ok_or_else(|| GitHubError::Request(
            "GitHub response missing next cursor".into()
        ))?))
    } else {
        None
    };
    Ok((items, cursor))
}

fn parse_authored_pull_request(node: &Value) -> Result<AuthoredPullRequest, GitHubError> {
    let parsed: GhNode = serde_json::from_value(node.clone())
        .map_err(|error| GitHubError::Request(format!("Invalid GitHub pull request: {error}")))?;
    Ok(AuthoredPullRequest {
        owner: parse_account(&node["repository"]["owner"])?,
        repository: required_string(&node["repository"], "name")?,
        head_branch: required_string(node, "headRefName")?,
        pull_request: map_pull_request(parsed),
    })
}
