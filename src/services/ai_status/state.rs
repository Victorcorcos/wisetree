//! Status enums and per-(harness, cwd) state model.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One of the four supported AI harnesses. Snake-case wire format keeps the
/// JSON shape stable for downstream consumers (`wisetree dashboard --json`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AiHarness {
    ClaudeCode,
    Opencode,
    CodexCli,
    GeminiCli,
}

impl AiHarness {
    pub const ALL: [AiHarness; 4] = [
        AiHarness::ClaudeCode,
        AiHarness::Opencode,
        AiHarness::CodexCli,
        AiHarness::GeminiCli,
    ];

    pub fn name(self) -> &'static str {
        match self {
            AiHarness::ClaudeCode => "claude_code",
            AiHarness::Opencode => "opencode",
            AiHarness::CodexCli => "codex_cli",
            AiHarness::GeminiCli => "gemini_cli",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude_code" | "claude" => Some(AiHarness::ClaudeCode),
            "opencode" => Some(AiHarness::Opencode),
            "codex_cli" | "codex" => Some(AiHarness::CodexCli),
            "gemini_cli" | "gemini" => Some(AiHarness::GeminiCli),
            _ => None,
        }
    }
}

/// Per-(harness, cwd) state. Aggregation rules live on [`AiStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum AiHarnessState {
    #[default]
    Absent,
    Idle,
    Running,
    Failed,
}

impl AiHarnessState {
    /// Merge two states for the same (harness, cwd) pair. Priority follows
    /// the aggregation rules in `PLAN.md` §2: Running > Idle > Failed > Absent.
    pub fn merge(a: Self, b: Self) -> Self {
        fn rank(state: AiHarnessState) -> u8 {
            match state {
                AiHarnessState::Running => 3,
                AiHarnessState::Idle => 2,
                AiHarnessState::Failed => 1,
                AiHarnessState::Absent => 0,
            }
        }
        if rank(a) >= rank(b) {
            a
        } else {
            b
        }
    }
}

/// Aggregated dashboard-row status. Glyph and label come from `PLAN.md` §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum AiStatus {
    #[default]
    None,
    InProgress,
    Finished,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AiStatusReport {
    pub aggregated: AiStatus,
    /// Always contains an entry for every enabled harness, even when its
    /// state is `Absent`. Disabled harnesses are omitted entirely.
    pub per_harness: BTreeMap<AiHarness, AiHarnessState>,
}

impl AiStatusReport {
    pub fn empty() -> Self {
        Self {
            aggregated: AiStatus::None,
            per_harness: BTreeMap::new(),
        }
    }

    /// Compute the aggregated status from `per_harness`. Priority order
    /// (PLAN §2): Running → InProgress, else Idle → Finished, else Failed
    /// → Failed, else None.
    pub fn aggregate(per_harness: &BTreeMap<AiHarness, AiHarnessState>) -> AiStatus {
        let mut saw_idle = false;
        let mut saw_failed = false;
        for state in per_harness.values() {
            match state {
                AiHarnessState::Running => return AiStatus::InProgress,
                AiHarnessState::Idle => saw_idle = true,
                AiHarnessState::Failed => saw_failed = true,
                AiHarnessState::Absent => {}
            }
        }
        if saw_idle {
            AiStatus::Finished
        } else if saw_failed {
            AiStatus::Failed
        } else {
            AiStatus::None
        }
    }
}
