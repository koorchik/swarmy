//! Parsing of Google Antigravity (`agy`) transcripts:
//! `<seat>/brain/<conversation_id>/.system_generated/logs/transcript.jsonl`.
//!
//! Each line is one append-only step (`step_index = 0, 1, 2, …`):
//! - `USER_INPUT`: human prompt
//! - `PLANNER_RESPONSE`: model answer with `model`, `input_tokens` (uncached),
//!   `cache_read_tokens`, `output_tokens`, `content`, `thinking`, `tool_calls`
//! - `GENERIC`: tool result for `tool_calls[k]` of the preceding `PLANNER_RESPONSE`, at
//!   `step_index = planner_step + 1 + k`
//! - `CHECKPOINT`: context compaction boundary
//! - `ERROR_MESSAGE`: model/API error
//! - `SYSTEM_MESSAGE`: system notification (skipped)

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::core::models::split_effort_suffix;
use crate::core::tools::ToolClass;
use crate::core::usage::Usage;
use crate::model::event::{Chunk, Event, ParseStats, Request, ToolUse};

/// The model of a step whose usage Antigravity did not record (see `parse_line`).
const UNRECORDED_MODEL: &str = "<synthetic>";

/// The step types of an Antigravity transcript swarmy knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepKind {
    UserInput,
    PlannerResponse,
    Generic,
    Checkpoint,
    ErrorMessage,
    SystemMessage,
}

const STEP_KINDS: [(&str, StepKind); 6] = [
    ("USER_INPUT", StepKind::UserInput),
    ("PLANNER_RESPONSE", StepKind::PlannerResponse),
    ("GENERIC", StepKind::Generic),
    ("CHECKPOINT", StepKind::Checkpoint),
    ("ERROR_MESSAGE", StepKind::ErrorMessage),
    ("SYSTEM_MESSAGE", StepKind::SystemMessage),
];

/// Record types swarmy recognises in Antigravity transcripts (for `doctor` format-drift checks).
pub const KNOWN_TYPES: [&str; 6] = [
    STEP_KINDS[0].0,
    STEP_KINDS[1].0,
    STEP_KINDS[2].0,
    STEP_KINDS[3].0,
    STEP_KINDS[4].0,
    STEP_KINDS[5].0,
];

impl StepKind {
    fn of(raw: &str) -> Option<StepKind> {
        STEP_KINDS.iter().find(|(k, _)| *k == raw).map(|(_, v)| *v)
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawStep {
    step_index: Option<u64>,
    #[serde(rename = "type")]
    kind: Option<String>,
    status: Option<String>,
    source: Option<String>,
    created_at: Option<String>,
    model: Option<String>,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    content: Option<String>,
    thinking: Option<String>,
    error: Option<String>,
    tool_calls: Vec<RawToolCall>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawToolCall {
    name: String,
    args: Value,
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn truncate(s: &str, max: usize) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let mut t: String = line.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Antigravity sometimes stores tool arg values as JSON-encoded strings inside strings
/// (e.g. `"\"cargo test\""`). Unwrap one layer of JSON string quoting when present.
fn clean_arg(v: Option<&Value>) -> String {
    let Some(s) = v.and_then(Value::as_str) else {
        return String::new();
    };
    if s.starts_with('"')
        && s.ends_with('"')
        && s.len() >= 2
        && let Ok(inner) = serde_json::from_str::<String>(s)
    {
        return inner;
    }
    s.to_string()
}

/// The argument that says what a tool did, for the tools that have one (and how much of it).
const OTHER_TOOL_ARGS: &[(&str, &[&str], usize)] = &[
    ("grep_search", &["Query", "query"], 80),
    ("find_by_name", &["Pattern", "SearchDirectory"], 100),
    ("list_dir", &["DirectoryPath"], 100),
    ("search_web", &["query", "Query"], 100),
    ("read_url_content", &["Url", "url"], 100),
    ("manage_task", &["Action", "TaskId"], 80),
];

fn summarize_tool(name: &str, args: &Value) -> (String, Option<String>) {
    let get = |keys: &[&str]| -> String {
        keys.iter()
            .map(|k| clean_arg(args.get(*k)))
            .find(|s| !s.is_empty())
            .unwrap_or_default()
    };
    let path = {
        let p = get(&["Path", "AbsolutePath", "TargetFile", "file_path", "path"]);
        (!p.is_empty()).then_some(p)
    };
    let class = ToolClass::of(name);
    let summary = match class {
        ToolClass::Shell => truncate(&get(&["CommandLine", "command"]), 120),
        ToolClass::Edit | ToolClass::Read => path.clone().unwrap_or_default(),
        ToolClass::Agent => {
            let role = get(&["Role", "TypeName", "name", "action"]);
            let desc = truncate(&get(&["Prompt", "brief", "Description"]), 80);
            if role.is_empty() {
                desc
            } else if desc.is_empty() {
                role
            } else {
                format!("{role}: {desc}")
            }
        }
        ToolClass::AskUser | ToolClass::Plan | ToolClass::Other => {
            match OTHER_TOOL_ARGS.iter().find(|(tool, _, _)| *tool == name) {
                Some((tool, keys, max)) => {
                    let v = truncate(&get(keys), *max);
                    if *tool == "grep_search" {
                        format!("\"{v}\"")
                    } else {
                        v
                    }
                }
                // Any other tool: its first short string argument.
                None => args
                    .as_object()
                    .and_then(|m| {
                        m.values().find_map(|v| {
                            let s = clean_arg(Some(v));
                            (!s.is_empty() && s.len() < 200).then_some(s)
                        })
                    })
                    .map(|s| truncate(&s, 80))
                    .unwrap_or_default(),
            }
        }
    };
    let target = class.has_target().then_some(path).flatten();
    (summary, target)
}

/// Extracts `Completed At: <RFC3339>` from a `GENERIC` tool output block when present.
fn parse_completed_at(content: Option<&str>) -> Option<DateTime<Utc>> {
    let text = content?;
    for line in text.lines().take(4) {
        if let Some(rest) = line.strip_prefix("Completed At:")
            && let Some(dt) = parse_ts(Some(rest.trim()))
        {
            return Some(dt);
        }
    }
    None
}

/// Strips `<USER_REQUEST>` wrapper tags from a `USER_INPUT` body before counting/truncating.
fn clean_prompt(raw: &str) -> String {
    let t = raw.trim();
    let stripped = t.strip_prefix("<USER_REQUEST>").unwrap_or(t).trim_start();
    let body = match stripped.find("</USER_REQUEST>") {
        Some(i) => &stripped[..i],
        None => stripped,
    };
    body.trim().to_string()
}

/// Parses one JSONL line of an Antigravity transcript belonging to `conv_id`.
pub fn parse_line(
    line: &str,
    conv_id: &str,
    default_cwd: Option<&str>,
    out: &mut Vec<Event>,
    stats: &mut ParseStats,
) {
    let line = line.trim_end();
    if line.is_empty() {
        return;
    }
    stats.lines += 1;
    let Ok(step) = serde_json::from_str::<RawStep>(line) else {
        stats.malformed += 1;
        return;
    };
    let kind = step.kind.as_deref().unwrap_or("");
    *stats.by_type.entry(kind.to_string()).or_default() += 1;
    let ts = parse_ts(step.created_at.as_deref());
    let Some(step_index) = step.step_index else {
        return;
    };

    // An unknown step type is counted (`stats.by_type`, `doctor`) and otherwise skipped.
    let Some(kind) = StepKind::of(kind) else {
        return;
    };
    match kind {
        StepKind::PlannerResponse => {
            let Some(ts) = ts else { return };
            let raw_model = step.model.unwrap_or_default();
            let (base_model, effort) = split_effort_suffix(&raw_model);
            // Older Antigravity versions record the model and the tokens on some steps only:
            // a step with neither is a turn whose usage was not written down. It stays a record
            // for its tool calls, under `<synthetic>` like Claude Code's non-API records, so it
            // counts as no request and is never shown as an unknown model.
            let unrecorded = raw_model.trim().is_empty()
                && step.input_tokens == 0
                && step.cache_read_tokens == 0
                && step.output_tokens == 0;
            let model = if unrecorded {
                UNRECORDED_MODEL.to_string()
            } else {
                base_model.to_string()
            };
            let effort = effort.map(str::to_string);
            let web_search = step
                .tool_calls
                .iter()
                .filter(|tc| tc.name == "search_web")
                .count() as u64;
            let usage = Usage {
                input: step.input_tokens,
                cache_write_5m: 0,
                cache_write_1h: 0,
                cache_read: step.cache_read_tokens,
                output: step.output_tokens,
                thinking: 0,
                web_search,
            };
            let tool_uses: Vec<ToolUse> = step
                .tool_calls
                .iter()
                .enumerate()
                .map(|(k, tc)| {
                    let (summary, target) = summarize_tool(&tc.name, &tc.args);
                    // Antigravity writes each tool result as a `GENERIC` record at
                    // `step_index + 1 + k` (verified across all transcripts), allowing
                    // stateless incremental matching across byte-offset reads.
                    let result_step = step_index + 1 + k as u64;
                    ToolUse {
                        id: format!("agy:{conv_id}:{result_step}"),
                        name: tc.name.clone(),
                        summary,
                        target,
                    }
                })
                .collect();
            let text = step
                .content
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .map(|t| truncate(t, 160));
            let thinking = step
                .thinking
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty());
            out.push(Event::Request(Request {
                msg_id: format!("agy:{conv_id}:{step_index}"),
                request_id: None,
                ts,
                model,
                requested_model: None,
                effort,
                fast: false,
                usage,
                cwd: default_cwd.filter(|c| !c.is_empty()).map(str::to_string),
                git_branch: None,
                agent_id: None,
                tool_uses,
                stop_reason: step.status,
                text,
                thinking,
            }));
        }
        StepKind::Generic => {
            if step.source.as_deref() == Some("MODEL") {
                let done_ts = parse_completed_at(step.content.as_deref()).or(ts);
                let is_error = step.status.as_deref() == Some("ERROR") || step.error.is_some();
                out.push(Event::ToolResult {
                    tool_use_id: format!("agy:{conv_id}:{step_index}"),
                    ts: done_ts,
                    is_error,
                });
            }
        }
        StepKind::UserInput => {
            if let Some(raw) = step.content.as_deref() {
                let text = clean_prompt(raw);
                if !text.is_empty() {
                    out.push(Event::Prompt {
                        ts,
                        text: truncate(&text, 200),
                        id: None,
                    });
                }
            }
        }
        // A checkpoint (context compaction) carries token counts but no model, so its cost is
        // not known: counted as a compaction only (a known gap).
        StepKind::Checkpoint => {
            out.push(Event::Compact { ts });
        }
        StepKind::ErrorMessage => {
            let text = step
                .error
                .or(step.content)
                .map(|t| truncate(&t, 200))
                .unwrap_or_default();
            out.push(Event::ApiError {
                ts,
                kind: "agy_error".into(),
                text,
                id: None,
            });
        }
        StepKind::SystemMessage => {}
    }
}

/// Reads complete JSONL lines from `offset` to EOF.
pub fn read_from(
    path: &Path,
    offset: u64,
    conv_id: &str,
    default_cwd: Option<&str>,
) -> std::io::Result<Chunk> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut rd = BufReader::with_capacity(256 * 1024, f);
    let mut chunk = Chunk {
        end: offset,
        ..Default::default()
    };
    let mut buf = Vec::with_capacity(64 * 1024);
    loop {
        buf.clear();
        let n = rd.read_until(b'\n', &mut buf)?;
        if n == 0 || buf.last() != Some(&b'\n') {
            break;
        }
        chunk.end += n as u64;
        match std::str::from_utf8(&buf) {
            Ok(line) => parse_line(
                line,
                conv_id,
                default_cwd,
                &mut chunk.events,
                &mut chunk.stats,
            ),
            Err(_) => chunk.stats.malformed += 1,
        }
    }
    Ok(chunk)
}

/// Reads the last text response from the tail of an Antigravity transcript (for `AGENT` panel).
pub fn last_message(path: &Path) -> Option<String> {
    let buf = crate::fsutil::tail_lines(path, 256 * 1024).ok()?;
    let mut events = Vec::new();
    let mut stats = ParseStats::default();
    for line in buf.lines() {
        parse_line(line, "", None, &mut events, &mut stats);
    }
    events.iter().rev().find_map(|e| match e {
        Event::Request(r) => r.text.clone(),
        Event::ToolResult { .. }
        | Event::Prompt { .. }
        | Event::Owner { .. }
        | Event::Title { .. }
        | Event::TurnEnd { .. }
        | Event::Compact { .. }
        | Event::Interrupted { .. }
        | Event::AgentLaunch { .. }
        | Event::ApiError { .. }
        | Event::CostState { .. } => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_planner_response_tool_results_prompt_and_checkpoint() {
        let lines = [
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-23T20:00:00Z","content":"<USER_REQUEST>\nFix the bug\n</USER_REQUEST>"}"#,
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-23T20:00:01Z","model":"gemini-4-neon-high","input_tokens":7688,"cache_read_tokens":7417,"output_tokens":1348,"content":"Checking files now.","thinking":"Plan...","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"cargo test\""}},{"name":"read_file","args":{"Path":"/w/shop/src/lib.rs"}}]}"#,
            r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-23T20:00:05Z","content":"Created At: 2026-11-23T20:00:02Z\nCompleted At: 2026-11-23T20:00:04Z\nok"}"#,
            r#"{"step_index":3,"source":"MODEL","type":"GENERIC","status":"ERROR","created_at":"2026-11-23T20:00:06Z","error":"file does not exist"}"#,
            r#"{"step_index":4,"source":"SYSTEM","type":"CHECKPOINT","status":"DONE","created_at":"2026-11-23T20:01:00Z","content":"{{ CHECKPOINT 0 }}"}"#,
            r#"{"step_index":5,"source":"SYSTEM","type":"ERROR_MESSAGE","status":"DONE","created_at":"2026-11-23T20:02:00Z","content":"Error: model output error"}"#,
        ];
        let mut events = Vec::new();
        let mut stats = ParseStats::default();
        for l in lines {
            parse_line(l, "conv-1", Some("/w/shop"), &mut events, &mut stats);
        }
        assert_eq!(stats.malformed, 0);
        assert_eq!(events.len(), 6);
        assert!(matches!(&events[0], Event::Prompt { text, .. } if text == "Fix the bug"));
        let Event::Request(req) = &events[1] else {
            panic!("expected Request: {:?}", events[1]);
        };
        assert_eq!(req.msg_id, "agy:conv-1:1");
        assert_eq!(req.model, "gemini-4-neon");
        assert_eq!(req.effort.as_deref(), Some("high"));
        assert_eq!(req.cwd.as_deref(), Some("/w/shop"));
        assert_eq!(req.usage.input, 7688);
        assert_eq!(req.usage.cache_read, 7417);
        assert_eq!(req.usage.output, 1348);
        assert_eq!(req.usage.prompt(), 15105);
        assert!(req.thinking);
        assert_eq!(req.text.as_deref(), Some("Checking files now."));
        assert_eq!(req.tool_uses.len(), 2);
        assert_eq!(req.tool_uses[0].id, "agy:conv-1:2");
        assert_eq!(req.tool_uses[0].summary, "cargo test");
        assert_eq!(req.tool_uses[1].id, "agy:conv-1:3");
        assert_eq!(
            req.tool_uses[1].target.as_deref(),
            Some("/w/shop/src/lib.rs")
        );

        assert!(matches!(
            &events[2],
            Event::ToolResult { tool_use_id, is_error: false, ts: Some(t) }
            if tool_use_id == "agy:conv-1:2" && t.to_rfc3339() == "2026-11-23T20:00:04+00:00"
        ));
        assert!(matches!(
            &events[3],
            Event::ToolResult { tool_use_id, is_error: true, .. }
            if tool_use_id == "agy:conv-1:3"
        ));
        assert!(matches!(&events[4], Event::Compact { .. }));
        assert!(matches!(&events[5], Event::ApiError { kind, .. } if kind == "agy_error"));
    }

    #[test]
    fn a_step_without_model_or_tokens_is_no_request_but_keeps_its_tools() {
        let line = r#"{"step_index":7,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-23T20:00:01Z","thinking":"t","tool_calls":[{"name":"run_command","args":{"CommandLine":"ls"}}]}"#;
        let mut events = Vec::new();
        let mut stats = ParseStats::default();
        parse_line(line, "c", None, &mut events, &mut stats);
        let Event::Request(r) = &events[0] else {
            panic!("{events:?}")
        };
        assert_eq!(r.model, "<synthetic>");
        assert_eq!(r.tool_uses.len(), 1);
        assert_eq!(r.tool_uses[0].id, "agy:c:8");
    }
}
