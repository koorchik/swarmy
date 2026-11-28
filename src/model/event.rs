//! What every harness's transcript reader produces: requests with their usage, tool calls,
//! prompts and the other events the index records, and what a read counted. Owned by no reader
//! (reviews A-H5, B-M14: they were Claude Code's, and agy and Codex conformed to them).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::core::usage::Usage;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    /// One line describing the call: the command, the file, the pattern…
    pub summary: String,
    /// The file a Read/Edit/Write call touches.
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub msg_id: String,
    pub request_id: Option<String>,
    pub ts: DateTime<Utc>,
    pub model: String,
    pub requested_model: Option<String>,
    pub effort: Option<String>,
    pub fast: bool,
    pub usage: Usage,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub agent_id: Option<String>,
    pub tool_uses: Vec<ToolUse>,
    pub stop_reason: Option<String>,
    /// The beginning of the last text block, for activity display.
    pub text: Option<String>,
    pub thinking: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TitleKind {
    Custom,
    Ai,
    AgentName,
}

#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "short-lived parse output; boxing every request costs more than it saves"
)]
pub enum Event {
    Request(Request),
    ToolResult {
        tool_use_id: String,
        ts: Option<DateTime<Utc>>,
        is_error: bool,
    },
    /// A prompt typed by the human.
    Prompt {
        ts: Option<DateTime<Utc>>,
        text: String,
        /// The record's own id, where the harness writes one (Claude Code's `uuid`).
        id: Option<String>,
    },
    /// `session-owner`: the account (and organization) this session runs under from here on.
    Owner {
        account: crate::core::ids::AccountId,
    },
    Title {
        kind: TitleKind,
        text: String,
    },
    TurnEnd {
        ts: Option<DateTime<Utc>>,
        duration_ms: u64,
        pending_background: u64,
    },
    Compact {
        ts: Option<DateTime<Utc>>,
    },
    /// The harness stopped the agent's turn (Claude Code's `[Request interrupted by user…]`):
    /// the human pressed Esc, or the parent stopped a background agent.
    Interrupted {
        ts: Option<DateTime<Utc>>,
    },
    AgentLaunch {
        ts: Option<DateTime<Utc>>,
        tool_use_id: Option<String>,
        agent_id: String,
        status: Option<String>,
    },
    /// An API error shown to the user (rate limit, auth, overload…).
    ApiError {
        ts: Option<DateTime<Utc>>,
        kind: String,
        text: String,
        /// The record's own id, where the harness writes one (Claude Code's `uuid`).
        id: Option<String>,
    },
    /// Claude Code's own running cost total for the process (`cost-state`), including background
    /// calls (titles, summaries) that never appear as transcript records.
    CostState {
        total_usd: f64,
        models: Vec<(String, f64)>,
    },
}

/// Statistics of a parse, for `doctor` (format-drift detection).
#[derive(Debug, Default, Clone)]
pub struct ParseStats {
    pub lines: u64,
    pub malformed: u64,
    pub by_type: BTreeMap<String, u64>,
}

/// The name under which the index keeps a file's count of damaged records (next to its record
/// types): lines that were no JSON record, and records with numbers out of range. What they held
/// is not counted, and `doctor` says so.
pub const MALFORMED: &str = "(malformed line)";

/// The result of reading a file from an offset.
#[derive(Debug, Default)]
pub struct Chunk {
    pub events: Vec<Event>,
    /// Byte offset just past the last complete line read.
    pub end: u64,
    pub stats: ParseStats,
}
