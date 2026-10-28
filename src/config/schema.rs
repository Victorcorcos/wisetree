//! Configuration types and defaults.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default `worktreeCopyPatterns`.
pub fn default_copy_patterns() -> Vec<String> {
    vec![".env*".to_string(), ".vscode/**".to_string()]
}

/// Default `worktreeCopyIgnores`.
pub fn default_copy_ignores() -> Vec<String> {
    vec![
        "**/node_modules/**".to_string(),
        "**/dist/**".to_string(),
        "**/.git/**".to_string(),
        "**/Thumbs.db".to_string(),
        "**/.DS_Store".to_string(),
    ]
}

/// Default `worktreePathTemplate`.
pub fn default_path_template() -> String {
    "$BASE_PATH.worktree".to_string()
}

pub fn default_refresh_ms() -> u64 {
    5_000
}

pub fn default_columns() -> Vec<String> {
    vec![
        "branch".to_string(),
        "from".to_string(),
        "status".to_string(),
        "ahead_behind".to_string(),
        "last_commit".to_string(),
    ]
}

pub fn clamp_interval_ms(value: u64, min_ms: u64, max_ms: u64) -> u64 {
    value.clamp(min_ms, max_ms)
}

pub fn clamp_dashboard_refresh_interval(value: u64) -> u64 {
    clamp_interval_ms(value, 5_000, 60_000)
}

pub fn normalize_dashboard_columns(columns: &[String]) -> (Vec<String>, Vec<String>) {
    let mut resolved = Vec::new();
    let mut warnings = Vec::new();

    for column in columns {
        let normalized = column.trim().to_ascii_lowercase();
        let known = matches!(
            normalized.as_str(),
            "branch" | "from" | "status" | "ahead_behind" | "diff" | "last_commit" | "pull_request"
        );

        if !known {
            // Ignore legacy columns just like unknown configuration fields.
            continue;
        }

        resolved.push(normalized);
    }

    if resolved.is_empty() {
        warnings.push("No valid dashboard columns configured; using defaults.".to_string());
        resolved = default_columns();
    }

    (resolved, warnings)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DashboardConfig {
    #[serde(rename = "refreshIntervalMs", default = "default_refresh_ms")]
    pub refresh_interval_ms: u64,

    #[serde(rename = "showPullRequests", default)]
    pub show_pull_requests: bool,

    #[serde(rename = "columns", default = "default_columns")]
    pub columns: Vec<String>,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            refresh_interval_ms: default_refresh_ms(),
            show_pull_requests: false,

            columns: default_columns(),
        }
    }
}

impl DashboardConfig {
    pub fn clamp(&mut self) {
        self.refresh_interval_ms = clamp_dashboard_refresh_interval(self.refresh_interval_ms);
    }

    pub fn normalize_columns(&mut self) -> Vec<String> {
        let (columns, warnings) = normalize_dashboard_columns(&self.columns);
        self.columns = columns;
        warnings
    }
}

/// The available Global Dashboard filter fields, including compound columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DashboardFilterField {
    Profile,
    Repository,
    Location,
    Status,
    Checks,
    Review,
    Mergeability,
    Worktree,
    Branch,
    From,
    LastCommit,
    PrTitle,
    Ahead,
    Behind,
    Insertions,
    Deletions,
    PrNumber,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardFilterKind {
    Enum,
    Text,
    Number,
}

impl DashboardFilterField {
    pub const ALL: [Self; 17] = [
        Self::Profile,
        Self::Repository,
        Self::Location,
        Self::Status,
        Self::Checks,
        Self::Review,
        Self::Mergeability,
        Self::Worktree,
        Self::Branch,
        Self::From,
        Self::LastCommit,
        Self::PrTitle,
        Self::Ahead,
        Self::Behind,
        Self::Insertions,
        Self::Deletions,
        Self::PrNumber,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Profile => "Profile",
            Self::Repository => "Repository",
            Self::Location => "Location",
            Self::Status => "Status",
            Self::Checks => "Checks",
            Self::Review => "Review",
            Self::Mergeability => "Mergeability",
            Self::Worktree => "Worktree",
            Self::Branch => "Branch",
            Self::From => "From",
            Self::LastCommit => "Last commit",
            Self::PrTitle => "PR title",
            Self::Ahead => "Ahead",
            Self::Behind => "Behind",
            Self::Insertions => "Insertions",
            Self::Deletions => "Deletions",
            Self::PrNumber => "PR number",
        }
    }

    pub fn kind(self) -> DashboardFilterKind {
        match self {
            Self::Profile
            | Self::Repository
            | Self::Location
            | Self::Status
            | Self::Checks
            | Self::Review
            | Self::Mergeability => DashboardFilterKind::Enum,
            Self::Ahead | Self::Behind | Self::Insertions | Self::Deletions | Self::PrNumber => {
                DashboardFilterKind::Number
            }
            _ => DashboardFilterKind::Text,
        }
    }

    /// Profile and Repository choices come from the current rows.
    pub fn enum_values(self) -> &'static [&'static str] {
        match self {
            Self::Location => &["Local", "Not cloned"],
            Self::Status => &[
                "Mother", "Merged", "Opened", "Drafted", "Closed", "Clean", "Dirty",
            ],
            Self::Checks => &["Passed", "Failed", "Running", "Pending", "Errored", "None"],
            Self::Review => &["Approved", "Rejected", "Pending", "None"],
            Self::Mergeability => &[
                "Draft", "Dirty", "Blocked", "Unknown", "Behind", "HasHooks", "Unstable", "Clean",
                "None",
            ],
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DashboardFilterOperator {
    IsAnyOf,
    IsNoneOf,
    Contains,
    DoesNotContain,
    Is,
    IsNot,
    Equal,
    NotEqual,
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
}

impl DashboardFilterOperator {
    pub fn for_kind(kind: DashboardFilterKind) -> &'static [Self] {
        match kind {
            DashboardFilterKind::Enum => &[Self::IsAnyOf, Self::IsNoneOf],
            DashboardFilterKind::Text => {
                &[Self::Contains, Self::DoesNotContain, Self::Is, Self::IsNot]
            }
            DashboardFilterKind::Number => &[
                Self::Equal,
                Self::NotEqual,
                Self::GreaterThan,
                Self::GreaterThanOrEqual,
                Self::LessThan,
                Self::LessThanOrEqual,
            ],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::IsAnyOf => "is any of",
            Self::IsNoneOf => "is none of",
            Self::Contains => "contains",
            Self::DoesNotContain => "does not contain",
            Self::Is => "is",
            Self::IsNot => "is not",
            Self::Equal => "=",
            Self::NotEqual => "≠",
            Self::GreaterThan => ">",
            Self::GreaterThanOrEqual => "≥",
            Self::LessThan => "<",
            Self::LessThanOrEqual => "≤",
        }
    }

    pub fn is_negated(self) -> bool {
        matches!(
            self,
            Self::IsNoneOf | Self::DoesNotContain | Self::IsNot | Self::NotEqual
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DashboardFilterRule {
    pub field: DashboardFilterField,
    pub operator: DashboardFilterOperator,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DashboardFilterGroup {
    pub rules: Vec<DashboardFilterRule>,
}

/// OR of AND groups. No groups means show everything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DashboardFilter {
    pub groups: Vec<DashboardFilterGroup>,
}

impl Default for DashboardFilter {
    fn default() -> Self {
        Self {
            groups: vec![DashboardFilterGroup {
                rules: vec![DashboardFilterRule {
                    field: DashboardFilterField::Status,
                    operator: DashboardFilterOperator::IsNoneOf,
                    values: vec!["Mother".to_string()],
                }],
            }],
        }
    }
}

impl DashboardFilter {
    pub fn normalize(&mut self) -> std::result::Result<(), String> {
        for group in &mut self.groups {
            group.rules.retain(|rule| !rule.values.is_empty());
            for rule in &mut group.rules {
                if !DashboardFilterOperator::for_kind(rule.field.kind()).contains(&rule.operator) {
                    return Err(format!("Invalid operator for {}", rule.field.label()));
                }
                let choices = rule.field.enum_values();
                if !choices.is_empty() && rule.values.iter().any(|v| !choices.contains(&v.as_str()))
                {
                    return Err(format!("Unknown value for {}", rule.field.label()));
                }
                if rule.field.kind() == DashboardFilterKind::Number
                    && rule.values.iter().any(|v| v.parse::<i128>().is_err())
                {
                    return Err(format!("Invalid number for {}", rule.field.label()));
                }
                let mut seen = std::collections::HashSet::new();
                rule.values
                    .retain(|value| seen.insert(value.to_lowercase()));
            }
        }
        self.groups.retain(|group| !group.rules.is_empty());
        Ok(())
    }
}

impl<'de> Deserialize<'de> for DashboardFilter {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Wire {
            groups: Vec<DashboardFilterGroup>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let mut filter = Self {
            groups: wire.groups,
        };
        filter.normalize().map_err(serde::de::Error::custom)?;
        Ok(filter)
    }
}

fn deserialize_dashboard_filter<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<DashboardFilter, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_else(|err| {
        eprintln!("Invalid globalDashboard.filter; using default: {err}");
        DashboardFilter::default()
    }))
}

/// Global Dashboard account selection, independent of project configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GlobalDashboardConfig {
    #[serde(default)]
    pub selected_accounts: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_dashboard_filter")]
    pub filter: DashboardFilter,
}

impl GlobalDashboardConfig {
    /// Keep the first spelling and order of each GitHub account.
    pub fn deduplicate(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.selected_accounts
            .retain(|account| seen.insert(account.to_ascii_lowercase()));
    }
}

/// Configuration for the worktree manager.
///
/// JSON keys use camelCase. Unknown legacy fields are ignored when loading
/// and omitted when saving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeConfig {
    /// File patterns to copy to new worktrees (glob patterns supported).
    #[serde(rename = "worktreeCopyPatterns", default = "default_copy_patterns")]
    pub worktree_copy_patterns: Vec<String>,

    /// File patterns to ignore when copying (glob patterns supported).
    #[serde(rename = "worktreeCopyIgnores", default = "default_copy_ignores")]
    pub worktree_copy_ignores: Vec<String>,

    /// Template for worktree directory names. Variables: $BASE_PATH,
    /// $WORKTREE_PATH, $BRANCH_NAME, $SOURCE_BRANCH.
    #[serde(rename = "worktreePathTemplate", default = "default_path_template")]
    pub worktree_path_template: String,

    /// Commands to run after creating a worktree. Variables: $BASE_PATH,
    /// $WORKTREE_PATH, $BRANCH_NAME, $SOURCE_BRANCH.
    #[serde(rename = "postCreateCmd", default)]
    pub post_create_cmd: Vec<String>,

    /// File and directory patterns to symlink directly to the Mother worktree.
    #[serde(rename = "worktreeLinkPatterns", default)]
    pub worktree_link_patterns: Vec<String>,

    /// Command to open terminal in new worktree directory (e.g., 'code $WORKTREE_PATH').
    #[serde(rename = "terminalCommand", default)]
    pub terminal_command: String,

    /// Also delete the associated git branch when deleting a worktree.
    #[serde(rename = "deleteBranchWithWorktree", default)]
    pub delete_branch_with_worktree: bool,

    /// Live dashboard preferences.
    #[serde(rename = "dashboard", default)]
    pub dashboard: DashboardConfig,

    /// Stored only in the global config; project saves omit this field.
    #[serde(
        rename = "globalDashboard",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub global_dashboard: Option<GlobalDashboardConfig>,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            worktree_copy_patterns: default_copy_patterns(),
            worktree_copy_ignores: default_copy_ignores(),
            worktree_path_template: default_path_template(),
            post_create_cmd: Vec::new(),
            worktree_link_patterns: Vec::new(),
            terminal_command: String::new(),
            delete_branch_with_worktree: false,
            dashboard: DashboardConfig::default(),
            global_dashboard: None,
        }
    }
}
