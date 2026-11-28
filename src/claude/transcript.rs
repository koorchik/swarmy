//! Parsing of Claude Code transcripts (`*.jsonl`, one JSON record per line).
//!
//! The format is internal to Claude Code and changes between versions, so parsing is lenient:
//! unknown record types are counted and skipped, unknown fields are ignored, and a malformed line
//! never stops the file. Large records (tool results, request blobs) are skipped without being
//! materialised where possible.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::core::tools::ToolClass;
use crate::core::usage::Usage;
pub use crate::model::event::{Chunk, Event, MALFORMED, ParseStats, Request, TitleKind, ToolUse};

/// Record types that carry nothing this tool needs; skipped without parsing.
const SKIP: &[&str] = &[
    "data-flow-records",
    "queue-operation",
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "mode",
    "atis-latch",
    "permission-mode",
    "last-prompt",
    "file-history-snapshot",
    "file-history-delta",
    "tools-used",
    "attachment",
    "derived-from",
    "progress",
    "artifact-autoreact-ledger",
    "artifact-comment-monitor",
    "bridge-session",
    "fork-context-ref",
    "frame-link",
    "history-suppression",
    "pr-link",
    "relocated",
    "worktree-state",
];

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawRecord<'a> {
    #[serde(rename = "type")]
    kind: Option<String>,
    subtype: Option<String>,
    timestamp: Option<String>,
    uuid: Option<String>,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    #[serde(rename = "requestedModel")]
    requested_model: Option<String>,
    effort: Option<String>,
    #[serde(rename = "isApiErrorMessage")]
    is_api_error: bool,
    #[serde(rename = "isMeta")]
    is_meta: bool,
    error: Option<String>,
    #[serde(borrow)]
    message: Option<RawMessage<'a>>,
    #[serde(rename = "toolUseResult", borrow)]
    tool_use_result: Option<&'a RawValue>,
    #[serde(rename = "accountUuid")]
    account_uuid: Option<String>,
    #[serde(rename = "organizationUuid")]
    organization_uuid: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "agentName")]
    agent_name: Option<String>,
    #[serde(rename = "durationMs")]
    duration_ms: Option<u64>,
    #[serde(rename = "pendingBackgroundAgentCount")]
    pending_background: Option<u64>,
    #[serde(rename = "totalCostUSD")]
    total_cost_usd: Option<f64>,
    #[serde(rename = "modelUsage", borrow)]
    model_usage: Option<&'a RawValue>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawMessage<'a> {
    id: Option<String>,
    model: Option<String>,
    #[serde(borrow)]
    content: Option<&'a RawValue>,
    usage: Option<RawUsage>,
    stop_reason: Option<String>,
}

/// A token count written as `null` reads as 0: `#[serde(default)]` covers a missing key only,
/// and one `null` made the whole record malformed, its spend gone (review B-M22).
fn null_as_zero<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    Ok(Option::<u64>::deserialize(d)?.unwrap_or(0))
}

/// An assistant record with usage but no message id or no timestamp: it cannot be counted, so
/// it is counted here instead (`doctor` lists it), never dropped silently (review B-M22).
pub const UNUSABLE: &str = "(assistant without id or timestamp)";

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawUsage {
    #[serde(deserialize_with = "null_as_zero")]
    input_tokens: u64,
    #[serde(deserialize_with = "null_as_zero")]
    output_tokens: u64,
    #[serde(deserialize_with = "null_as_zero")]
    cache_creation_input_tokens: u64,
    #[serde(deserialize_with = "null_as_zero")]
    cache_read_input_tokens: u64,
    cache_creation: Option<RawCacheCreation>,
    output_tokens_details: Option<RawOutputDetails>,
    server_tool_use: Option<RawServerTools>,
    speed: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawCacheCreation {
    #[serde(deserialize_with = "null_as_zero")]
    ephemeral_5m_input_tokens: u64,
    #[serde(deserialize_with = "null_as_zero")]
    ephemeral_1h_input_tokens: u64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawOutputDetails {
    #[serde(deserialize_with = "null_as_zero")]
    thinking_tokens: u64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawServerTools {
    #[serde(deserialize_with = "null_as_zero")]
    web_search_requests: u64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawBlock<'a> {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    name: Option<String>,
    #[serde(borrow)]
    input: Option<&'a RawValue>,
    tool_use_id: Option<String>,
    is_error: Option<bool>,
    text: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawAgentResult {
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
    status: Option<String>,
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// The record type when the line starts with `{"type":"…"`, the shape of most records.
fn leading_type(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("{\"type\":\"")?;
    rest.split('"').next()
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

fn summarize_tool(name: &str, input: Option<&RawValue>) -> (String, Option<String>) {
    let v: serde_json::Value = input
        .and_then(|r| serde_json::from_str(r.get()).ok())
        .unwrap_or_default();
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let path = ["file_path", "notebook_path", "path"]
        .iter()
        .map(|k| s(k))
        .find(|p| !p.is_empty())
        .map(str::to_string);
    let class = ToolClass::of(name);
    let summary = match class {
        ToolClass::Shell => truncate(s("command"), 120),
        ToolClass::Edit | ToolClass::Read => path.clone().unwrap_or_default(),
        ToolClass::Agent => {
            let t = s("subagent_type");
            let d = truncate(s("description"), 80);
            if t.is_empty() {
                d
            } else {
                format!("{d} ({t})")
            }
        }
        ToolClass::AskUser | ToolClass::Plan | ToolClass::Other => match name {
            "Grep" => format!("\"{}\"", truncate(s("pattern"), 80)),
            "Glob" => truncate(s("pattern"), 100),
            "WebFetch" => truncate(s("url"), 100),
            "WebSearch" => truncate(s("query"), 100),
            "Skill" => truncate(s("skill"), 80),
            "Workflow" => truncate(s("name"), 80),
            _ => {
                // First short string field, if any.
                v.as_object()
                    .and_then(|m| {
                        m.values()
                            .find_map(|x| x.as_str().filter(|t| !t.is_empty() && t.len() < 200))
                    })
                    .map(|t| truncate(t, 80))
                    .unwrap_or_default()
            }
        },
    };
    let target = class.has_target().then_some(path).flatten();
    (summary, target)
}

const KNOWN: &[&str] = &[
    "assistant",
    "user",
    "system",
    "session-owner",
    "custom-title",
    "ai-title",
    "agent-name",
    "summary",
    "cost-state",
];

impl ParseStats {
    /// Record types neither handled nor deliberately skipped: a sign the format moved on.
    #[cfg(test)]
    pub fn unknown_types(&self) -> Vec<(&str, u64)> {
        self.by_type
            .iter()
            .filter(|(k, _)| {
                !KNOWN.contains(&k.as_str())
                    && !SKIP.contains(&k.as_str())
                    && k.as_str() != MALFORMED
            })
            .map(|(k, v)| (k.as_str(), *v))
            .collect()
    }
}

/// Whether this reader knows a record type (reads it, or skips it on purpose).
pub fn is_known_type(t: &str) -> bool {
    KNOWN.contains(&t) || SKIP.contains(&t) || t == MALFORMED
}

pub fn parse_line(line: &str, out: &mut Vec<Event>, stats: &mut ParseStats) {
    let line = line.trim_end();
    if line.is_empty() {
        return;
    }
    stats.lines += 1;
    if let Some(t) = leading_type(line)
        && SKIP.contains(&t)
    {
        *stats.by_type.entry(t.to_string()).or_default() += 1;
        return;
    }
    let Ok(r) = serde_json::from_str::<RawRecord>(line) else {
        stats.malformed += 1;
        return;
    };
    let kind = r.kind.as_deref().unwrap_or("");
    *stats.by_type.entry(kind.to_string()).or_default() += 1;
    let ts = parse_ts(r.timestamp.as_deref());
    match kind {
        "assistant" => {
            if !parse_assistant(r, ts, out) {
                *stats.by_type.entry(UNUSABLE.to_string()).or_default() += 1;
            }
        }
        "user" => parse_user(r, ts, out),
        "system" => match r.subtype.as_deref() {
            Some("turn_duration") => out.push(Event::TurnEnd {
                ts,
                duration_ms: r.duration_ms.unwrap_or(0),
                pending_background: r.pending_background.unwrap_or(0),
            }),
            Some("compact_boundary") => out.push(Event::Compact { ts }),
            _ => {}
        },
        "session-owner" => {
            if let Some(a) = r.account_uuid.filter(|a| !a.is_empty()) {
                let org = r.organization_uuid.filter(|o| !o.is_empty());
                out.push(Event::Owner {
                    account: crate::core::ids::AccountId::of(&a, org.as_deref()),
                });
            }
        }
        "custom-title" => {
            if let Some(t) = r.custom_title {
                out.push(Event::Title {
                    kind: TitleKind::Custom,
                    text: t,
                });
            }
        }
        "ai-title" => {
            if let Some(t) = r.ai_title {
                out.push(Event::Title {
                    kind: TitleKind::Ai,
                    text: t,
                });
            }
        }
        "cost-state" => {
            if let Some(total_usd) = r.total_cost_usd {
                let models = r
                    .model_usage
                    .and_then(|m| {
                        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(m.get())
                            .ok()
                    })
                    .map(|m| {
                        m.iter()
                            .map(|(k, v)| {
                                (
                                    k.clone(),
                                    v.get("costUSD").and_then(|c| c.as_f64()).unwrap_or(0.0),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(Event::CostState { total_usd, models });
            }
        }
        "agent-name" => {
            if let Some(t) = r.agent_name {
                out.push(Event::Title {
                    kind: TitleKind::AgentName,
                    text: t,
                });
            }
        }
        _ => {}
    }
}

/// Whether the record could be read (false: a response without id or timestamp).
fn parse_assistant(r: RawRecord, ts: Option<DateTime<Utc>>, out: &mut Vec<Event>) -> bool {
    let Some(msg) = r.message else { return true };
    let blocks: Vec<RawBlock> = msg
        .content
        .and_then(|c| serde_json::from_str(c.get()).ok())
        .unwrap_or_default();
    if r.is_api_error {
        let text = blocks
            .iter()
            .find_map(|b| b.text.as_deref())
            .map(|t| truncate(t, 200))
            .unwrap_or_default();
        out.push(Event::ApiError {
            ts,
            kind: r.error.unwrap_or_else(|| "api_error".into()),
            text,
            id: r.uuid,
        });
        return true;
    }
    let (Some(msg_id), Some(ts)) = (msg.id, ts) else {
        return false;
    };
    let u = msg.usage.unwrap_or_default();
    let cc = u.cache_creation.unwrap_or_default();
    // Older records have only the total; count it as 5-minute writes, the default TTL.
    let (w5, w1) = if cc.ephemeral_5m_input_tokens + cc.ephemeral_1h_input_tokens == 0 {
        (u.cache_creation_input_tokens, 0)
    } else {
        (cc.ephemeral_5m_input_tokens, cc.ephemeral_1h_input_tokens)
    };
    let usage = Usage {
        input: u.input_tokens,
        cache_write_5m: w5,
        cache_write_1h: w1,
        cache_read: u.cache_read_input_tokens,
        output: u.output_tokens,
        thinking: u
            .output_tokens_details
            .map(|d| d.thinking_tokens)
            .unwrap_or(0),
        web_search: u
            .server_tool_use
            .map(|s| s.web_search_requests)
            .unwrap_or(0),
    };
    let mut tool_uses = Vec::new();
    let mut text = None;
    let mut thinking = false;
    for b in &blocks {
        match b.kind.as_str() {
            "tool_use" | "server_tool_use" => {
                let name = b.name.clone().unwrap_or_default();
                let (summary, target) = summarize_tool(&name, b.input);
                tool_uses.push(ToolUse {
                    id: b.id.clone().unwrap_or_default(),
                    name,
                    summary,
                    target,
                });
            }
            "text" => {
                if let Some(t) = b.text.as_deref().filter(|t| !t.trim().is_empty()) {
                    text = Some(truncate(t, 160));
                }
            }
            "thinking" | "redacted_thinking" => thinking = true,
            _ => {}
        }
    }
    out.push(Event::Request(Request {
        msg_id,
        request_id: r.request_id,
        ts,
        model: msg.model.unwrap_or_default(),
        // An empty string says nothing: stored as unknown, like a missing field (the rollups of
        // old spend keep unknown as '').
        requested_model: r.requested_model.filter(|s| !s.is_empty()),
        effort: r.effort.filter(|s| !s.is_empty()),
        fast: u.speed.as_deref() == Some("fast"),
        usage,
        cwd: r.cwd.filter(|s| !s.is_empty()),
        git_branch: r.git_branch,
        agent_id: r.agent_id,
        tool_uses,
        stop_reason: msg.stop_reason,
        text,
        thinking,
    }));
    true
}

fn parse_user(r: RawRecord, ts: Option<DateTime<Utc>>, out: &mut Vec<Event>) {
    let content = r.message.as_ref().and_then(|m| m.content);
    let mut first_tool_use_id = None;
    let mut has_tool_result = false;
    if let Some(c) = content {
        if let Ok(blocks) = serde_json::from_str::<Vec<RawBlock>>(c.get()) {
            for b in &blocks {
                if b.kind == "tool_result"
                    && let Some(id) = &b.tool_use_id
                {
                    has_tool_result = true;
                    first_tool_use_id.get_or_insert_with(|| id.clone());
                    out.push(Event::ToolResult {
                        tool_use_id: id.clone(),
                        ts,
                        is_error: b.is_error.unwrap_or(false),
                    });
                }
            }
            if !has_tool_result && !r.is_meta && r.tool_use_result.is_none() {
                let text: String = blocks
                    .iter()
                    .filter(|b| b.kind == "text")
                    .filter_map(|b| b.text.as_deref())
                    .collect::<Vec<_>>()
                    .join(" ");
                push_prompt(ts, &text, r.uuid.clone(), out);
            }
        } else if let Ok(text) = serde_json::from_str::<String>(c.get())
            && !r.is_meta
            && r.tool_use_result.is_none()
        {
            push_prompt(ts, &text, r.uuid.clone(), out);
        }
    }
    if let Some(res) = r.tool_use_result
        && let Ok(a) = serde_json::from_str::<RawAgentResult>(res.get())
        && let Some(agent_id) = a.agent_id
    {
        out.push(Event::AgentLaunch {
            ts,
            tool_use_id: first_tool_use_id,
            agent_id,
            status: a.status,
        });
    }
}

fn push_prompt(ts: Option<DateTime<Utc>>, text: &str, id: Option<String>, out: &mut Vec<Event>) {
    let t = text.trim_start();
    if t.starts_with("[Request interrupted") {
        out.push(Event::Interrupted { ts });
        return;
    }
    // Injected by the harness, not typed by a person.
    let injected = [
        "<task-notification",
        "<system-reminder",
        "<local-command",
        "<command-",
        "Caveat:",
    ];
    if t.is_empty() || injected.iter().any(|p| t.starts_with(p)) {
        return;
    }
    out.push(Event::Prompt {
        ts,
        text: truncate(t, 200),
        id,
    });
}

/// Reads complete lines from `offset` to the end. A last line without a newline is still being
/// written and is left for the next read.
pub fn read_from(path: &Path, offset: u64) -> std::io::Result<Chunk> {
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
            Ok(line) => parse_line(line, &mut chunk.events, &mut chunk.stats),
            Err(_) => chunk.stats.malformed += 1,
        }
    }
    Ok(chunk)
}

/// What the end of a transcript says about its last API response: when it was written and the
/// account the session ran under then (the `session-owner` record before it, else the last one
/// in the file).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tail {
    pub owner: Option<crate::core::ids::AccountId>,
    pub last_response: Option<DateTime<Utc>>,
}

/// The last text response in the tail of a transcript (the AGENT card).
pub fn last_message(path: &Path) -> Option<String> {
    let buf = crate::fsutil::tail_lines(path, 256 * 1024).ok()?;
    let mut events = Vec::new();
    let mut stats = ParseStats::default();
    for line in buf.lines() {
        parse_line(line, &mut events, &mut stats);
    }
    events.iter().rev().find_map(|e| {
        if let Event::Request(r) = e {
            r.text.clone()
        } else {
            None
        }
    })
}

/// Reads the end of a transcript: 64 KiB, then four times more each step up to `max` bytes,
/// until the window holds the last response and the owner it ran under, or reaches the start of
/// the file. For the status line, which needs these two facts and must not read whole files.
pub fn tail(path: &Path, max: u64) -> std::io::Result<Tail> {
    use std::io::Read as _;
    let len = std::fs::metadata(path)?.len();
    let mut window: u64 = 64 * 1024;
    loop {
        let start = len.saturating_sub(window);
        let mut f = File::open(path)?;
        f.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        f.take(len - start).read_to_end(&mut buf)?;
        let text = String::from_utf8_lossy(&buf);
        let mut lines = text.split('\n');
        if start > 0 {
            // Starts inside a record.
            lines.next();
        }
        let (mut owner, mut last, mut last_owner) = (None, None, None);
        let (mut events, mut stats) = (Vec::new(), ParseStats::default());
        for line in lines {
            parse_line(line, &mut events, &mut stats);
            for e in events.drain(..) {
                if let Event::Owner { account } = e {
                    owner = Some(account);
                } else if let Event::Request(r) = e {
                    last = Some(last.map_or(r.ts, |t: DateTime<Utc>| t.max(r.ts)));
                    last_owner.clone_from(&owner);
                }
            }
        }
        if (last.is_some() && last_owner.is_some()) || start == 0 || window >= max {
            return Ok(Tail {
                owner: last_owner.or(owner),
                last_response: last,
            });
        }
        window = (window * 4).min(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(lines: &[&str]) -> (Vec<Event>, ParseStats) {
        let mut out = Vec::new();
        let mut st = ParseStats::default();
        for l in lines {
            parse_line(l, &mut out, &mut st);
        }
        (out, st)
    }

    const ASSISTANT: &str = r#"{"parentUuid":"p","isSidechain":false,"cwd":"/w/proj","gitBranch":"main","effort":"high","requestedModel":"claude-opus-5-5","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test --no-fail-fast\nsecond line"}}],"stop_reason":"tool_use","usage":{"input_tokens":2,"cache_creation_input_tokens":409,"cache_read_input_tokens":155527,"output_tokens":192,"output_tokens_details":{"thinking_tokens":7},"server_tool_use":{"web_search_requests":1},"cache_creation":{"ephemeral_1h_input_tokens":409,"ephemeral_5m_input_tokens":0},"speed":"standard"}},"requestId":"req_1","type":"assistant","uuid":"u1","timestamp":"2026-11-22T11:53:56.181Z","sessionId":"s"}"#;

    /// The status line's view of a transcript: the last response and the owner it ran under,
    /// found from the end, however far back the owner record is.
    #[test]
    fn the_tail_names_the_last_response_and_its_owner() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("s.jsonl");
        let owner =
            |a: &str| format!(r#"{{"type":"session-owner","accountUuid":"{a}","sessionId":"s"}}"#);
        let at = |ts: &str| ASSISTANT.replace("2026-11-22T11:53:56.181Z", ts);
        let filler = format!(
            r#"{{"type":"user","message":{{"content":"{}"}}}}"#,
            "x".repeat(70_000)
        );
        let mut lines = vec![owner("acc-a"), at("2026-11-22T10:00:00Z")];
        lines.extend(std::iter::repeat_n(filler.clone(), 3));
        lines.push(at("2026-11-22T11:00:00Z"));
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        let got = tail(&p, 1 << 20).unwrap();
        assert_eq!(
            got.owner.as_ref().map(|a| a.as_str()),
            Some("acc-a"),
            "found 200 KiB back"
        );
        assert_eq!(
            got.last_response,
            Some(parse_ts(Some("2026-11-22T11:00:00Z")).unwrap())
        );
        // A newer owner record after the last response (the next turn has not answered yet):
        // the response still ran under the old one.
        lines.push(owner("acc-b"));
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        assert_eq!(
            tail(&p, 1 << 20)
                .unwrap()
                .owner
                .as_ref()
                .map(|a| a.as_str()),
            Some("acc-a")
        );
        lines.push(at("2026-11-22T12:00:00Z"));
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        assert_eq!(
            tail(&p, 1 << 20)
                .unwrap()
                .owner
                .as_ref()
                .map(|a| a.as_str()),
            Some("acc-b")
        );
        // Too far back for the window: no owner, rather than reading the whole file.
        let got = tail(&p, 64 * 1024).unwrap();
        assert_eq!(got.owner.as_ref().map(|a| a.as_str()), Some("acc-b"));
        std::fs::write(
            &p,
            [
                owner("acc-a"),
                filler.clone(),
                filler,
                at("2026-11-22T13:00:00Z"),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        assert_eq!(tail(&p, 64 * 1024).unwrap().owner, None);
    }

    #[test]
    fn assistant_request_with_tool_use() {
        let (ev, _) = parse(&[ASSISTANT]);
        let Event::Request(r) = &ev[0] else {
            panic!("{ev:?}")
        };
        assert_eq!(r.msg_id, "msg_1");
        assert_eq!(r.model, "claude-opus-5-5");
        assert_eq!(r.effort.as_deref(), Some("high"));
        assert_eq!(
            r.usage,
            Usage {
                input: 2,
                cache_write_5m: 0,
                cache_write_1h: 409,
                cache_read: 155_527,
                output: 192,
                thinking: 7,
                web_search: 1
            }
        );
        assert!(!r.fast);
        assert_eq!(r.tool_uses[0].name, "Bash");
        assert_eq!(r.tool_uses[0].summary, "cargo test --no-fail-fast");
        assert_eq!(r.cwd.as_deref(), Some("/w/proj"));
    }

    #[test]
    fn legacy_cache_total_counts_as_five_minute() {
        let l = r#"{"type":"assistant","timestamp":"2025-08-12T10:00:00Z","message":{"id":"m","model":"claude-sonnet-4-20250514","content":[],"usage":{"input_tokens":1,"cache_creation_input_tokens":50,"cache_read_input_tokens":0,"output_tokens":3}}}"#;
        let (ev, _) = parse(&[l]);
        let Event::Request(r) = &ev[0] else { panic!() };
        assert_eq!((r.usage.cache_write_5m, r.usage.cache_write_1h), (50, 0));
    }

    #[test]
    fn user_records_tool_results_prompts_and_agent_launches() {
        let tool_result = r#"{"parentUuid":"x","type":"user","timestamp":"2026-11-22T11:54:00Z","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"huge output ...","is_error":true}]},"toolUseResult":{"stdout":"..."}}"#;
        let prompt = r#"{"type":"user","timestamp":"2026-11-22T11:50:00Z","message":{"role":"user","content":"Fix the VAT report"}}"#;
        let notification = r#"{"type":"user","timestamp":"2026-11-22T11:50:00Z","message":{"role":"user","content":"<task-notification>\n<task-id>b</task-id>"}}"#;
        let launch = r#"{"type":"user","timestamp":"2026-11-22T11:55:00Z","message":{"role":"user","content":[{"tool_use_id":"toolu_9","type":"tool_result","content":[{"type":"text","text":"launched"}]}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a1bf","description":"d"}}"#;
        let (ev, _) = parse(&[tool_result, prompt, notification, launch]);
        assert_eq!(
            ev[0],
            Event::ToolResult {
                tool_use_id: "toolu_1".into(),
                ts: parse_ts(Some("2026-11-22T11:54:00Z")),
                is_error: true
            }
        );
        assert!(matches!(&ev[1], Event::Prompt { text, .. } if text == "Fix the VAT report"));
        assert!(
            matches!(&ev[3], Event::AgentLaunch { agent_id, tool_use_id: Some(t), status: Some(s), .. } if agent_id == "a1bf" && t == "toolu_9" && s == "async_launched")
        );
        assert_eq!(ev.len(), 4, "notification is not a human prompt: {ev:?}");
    }

    #[test]
    fn owner_titles_turns_errors_and_skips() {
        let lines = [
            r#"{"type":"session-owner","accountUuid":"acc-1","organizationUuid":"org","sessionId":"s"}"#,
            r#"{"type":"custom-title","customTitle":"modules","sessionId":"s"}"#,
            r#"{"type":"data-flow-records","v":1,"records":[1,2,3]}"#,
            r#"{"parentUuid":"x","type":"system","subtype":"turn_duration","durationMs":5789,"messageCount":211,"pendingBackgroundAgentCount":10,"timestamp":"2026-11-22T11:53:57.598Z"}"#,
            r#"{"type":"assistant","isApiErrorMessage":true,"error":"rate_limit","timestamp":"2026-11-22T12:00:00Z","message":{"id":"x","model":"<synthetic>","content":[{"type":"text","text":"API Error: Rate limit reached"}]}}"#,
            r#"{"type":"cost-state","sessionId":"s","totalCostUSD":1.25,"modelUsage":{"claude-haiku-5-5":{"inputTokens":9,"costUSD":0.25},"claude-opus-5-5":{"costUSD":1.0}}}"#,
            r#"{"type":"brand-new-thing","x":1}"#,
            "not json",
        ];
        let (ev, st) = parse(&lines);
        assert_eq!(
            ev[0],
            Event::Owner {
                account: crate::core::ids::AccountId::of("acc-1", Some("org")),
            }
        );
        assert_eq!(
            ev[1],
            Event::Title {
                kind: TitleKind::Custom,
                text: "modules".into()
            }
        );
        assert!(matches!(
            ev[2],
            Event::TurnEnd {
                duration_ms: 5789,
                pending_background: 10,
                ..
            }
        ));
        assert!(
            matches!(&ev[3], Event::ApiError { kind, text, .. } if kind == "rate_limit" && text.starts_with("API Error"))
        );
        assert_eq!(
            ev[4],
            Event::CostState {
                total_usd: 1.25,
                models: vec![
                    ("claude-haiku-5-5".into(), 0.25),
                    ("claude-opus-5-5".into(), 1.0)
                ]
            }
        );
        assert_eq!(ev.len(), 5);
        assert_eq!(st.malformed, 1);
        assert_eq!(st.unknown_types(), vec![("brand-new-thing", 1)]);
    }

    /// Review B-M22: a `null` count reads as 0 (the record and its spend are kept); a response
    /// without a message id is counted as unusable, never dropped silently.
    #[test]
    fn null_counts_and_responses_without_id_are_not_lost() {
        let mut out = Vec::new();
        let mut stats = ParseStats::default();
        let with_null = r#"{"type":"assistant","timestamp":"2026-11-20T10:00:00Z","message":{"id":"m1","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":null,"cache_read_input_tokens":0}}}"#;
        parse_line(with_null, &mut out, &mut stats);
        assert_eq!(stats.malformed, 0);
        assert!(
            matches!(&out[..], [Event::Request(r)] if r.usage.input == 10 && r.usage.cache_write_5m == 0)
        );
        let no_id = r#"{"type":"assistant","timestamp":"2026-11-20T10:00:00Z","message":{"model":"claude-opus-5-5","content":[],"usage":{"input_tokens":10,"output_tokens":5}}}"#;
        parse_line(no_id, &mut out, &mut stats);
        assert_eq!(out.len(), 1);
        assert_eq!(stats.by_type.get(UNUSABLE), Some(&1));
        assert!(!is_known_type(UNUSABLE), "doctor lists it");
    }

    #[test]
    fn partial_last_line_is_left_for_later() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("s.jsonl");
        let full = format!("{ASSISTANT}\n");
        std::fs::write(&p, format!("{full}{{\"type\":\"user\",\"mess")).unwrap();
        let c = read_from(&p, 0).unwrap();
        assert_eq!(c.events.len(), 1);
        assert_eq!(c.end, full.len() as u64);
        let again = read_from(&p, c.end).unwrap();
        assert!(again.events.is_empty());
        assert_eq!(again.end, c.end);
    }

    #[test]
    fn tool_summaries() {
        let raw = |s: &str| RawValue::from_string(s.to_string()).unwrap();
        let r = raw(r#"{"file_path":"/w/src/vat.rs","old_string":"a"}"#);
        assert_eq!(
            summarize_tool("Edit", Some(&r)),
            ("/w/src/vat.rs".into(), Some("/w/src/vat.rs".into()))
        );
        let r = raw(r#"{"pattern":"usecase","path":"/w"}"#);
        assert_eq!(summarize_tool("Grep", Some(&r)).0, "\"usecase\"");
        let r =
            raw(r#"{"description":"Audit VAT","subagent_type":"general-purpose","prompt":"long"}"#);
        assert_eq!(
            summarize_tool("Agent", Some(&r)).0,
            "Audit VAT (general-purpose)"
        );
    }
}
