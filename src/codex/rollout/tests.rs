use super::*;
use serde_json::json;

const SID: &str = "0a1b2c3d-0000-4000-8000-000000000001";

fn line(ts: &str, kind: &str, payload: Value) -> String {
    json!({"timestamp": ts, "type": kind, "payload": payload}).to_string()
}

fn usage(input: u64, cached: u64, output: u64, reasoning: u64, total: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": cached, "cache_write_input_tokens": 0,
           "output_tokens": output, "reasoning_output_tokens": reasoning, "total_tokens": total})
}

/// One response's usage as Codex 0.172 writes it: a `token_usage_record`, then a `token_count`
/// with the same numbers.
fn response(ts: &str, u: Value, thread_total: Value) -> [String; 2] {
    [
        line(
            ts,
            "token_usage_record",
            json!({"thread_id": SID, "turn_id": "t-1", "response_id": "resp-x", "usage": u,
                   "turn_token_usage": thread_total, "thread_token_usage": thread_total}),
        ),
        line(
            ts,
            "event_msg",
            json!({"type": "token_count", "info": {"total_token_usage": thread_total,
                   "last_token_usage": u, "last_usage_includes_reasoning": true},
                   "rate_limits": {"primary": {"used_percent": 12.0, "window_minutes": 300,
                   "resets_at": 1795530000}, "secondary": null}}),
        ),
    ]
}

fn turn() -> Vec<String> {
    let mut v = vec![
        line(
            "2026-11-24T10:00:00Z",
            "session_meta",
            json!({"id": SID, "cwd": "/w/shop", "cli_version": "0.172.0",
                   "creator_user_id": "user-1", "creator_account_id": "ws-1",
                   "git": {"branch": "main", "commit_hash": "abc"}}),
        ),
        line(
            "2026-11-24T10:00:01Z",
            "event_msg",
            json!({"type": "task_started", "turn_id": "t-1"}),
        ),
        line(
            "2026-11-24T10:00:01Z",
            "turn_context",
            json!({"turn_id": "t-1", "cwd": "/w/shop", "model": "gpt-6-luna",
                   "collaboration_mode": {"settings": {"reasoning_effort": "high"}}}),
        ),
        line(
            "2026-11-24T10:00:01Z",
            "world_state",
            json!({"full": true, "state": {}}),
        ),
        line(
            "2026-11-24T10:00:02Z",
            "event_msg",
            json!({"type": "item_completed", "item": {"type": "UserMessage",
                   "content": [{"type": "text", "text": "Fix the failing test"}]}}),
        ),
        line(
            "2026-11-24T10:00:05Z",
            "response_item",
            json!({"type": "function_call", "name": "shell", "call_id": "call_1",
                   "arguments": "{\"command\":[\"bash\",\"-lc\",\"cargo test\"]}"}),
        ),
    ];
    v.extend(response(
        "2026-11-24T10:00:05Z",
        usage(20_000, 11_000, 300, 120, 20_300),
        usage(20_000, 11_000, 300, 120, 20_300),
    ));
    v.push(line(
        "2026-11-24T10:00:09Z",
        "response_item",
        json!({"type": "function_call_output", "call_id": "call_1",
               "output": {"content": "1 failed", "success": false}}),
    ));
    v.push(line(
        "2026-11-24T10:00:10Z",
        "response_item",
        json!({"type": "custom_tool_call", "name": "apply_patch", "call_id": "call_2",
               "input": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n*** End Patch"}),
    ));
    v.extend(response(
        "2026-11-24T10:00:10Z",
        usage(21_000, 20_000, 500, 0, 21_500),
        usage(41_000, 31_000, 800, 120, 41_800),
    ));
    v.push(line(
        "2026-11-24T10:00:12Z",
        "response_item",
        json!({"type": "custom_tool_call_output", "call_id": "call_2", "output": "Done"}),
    ));
    v.push(line(
        "2026-11-24T10:00:14Z",
        "response_item",
        json!({"type": "message", "role": "assistant",
               "content": [{"type": "output_text", "text": "Fixed the test."}]}),
    ));
    v.push(line(
        "2026-11-24T10:00:15Z",
        "event_msg",
        json!({"type": "task_complete", "turn_id": "t-1", "duration_ms": 14000}),
    ));
    v
}

fn parse_all(lines: &[String]) -> (Vec<Event>, ParseStats, Ctx) {
    let (mut out, mut stats, mut ctx) = (Vec::new(), ParseStats::default(), Ctx::default());
    for l in lines {
        parse_line(l, SID, &mut ctx, &mut out, &mut stats);
    }
    (out, stats, ctx)
}

fn requests(events: &[Event]) -> Vec<&Request> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Request(r) => Some(r),
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
        .collect()
}

#[test]
fn a_turn_gives_its_responses_tools_prompt_and_owner() {
    let (events, stats, _) = parse_all(&turn());
    assert_eq!(stats.malformed, 0);
    assert!(
        stats
            .by_type
            .keys()
            .all(|k| KNOWN_TYPES.contains(&k.as_str())),
        "{:?}",
        stats.by_type
    );
    assert!(
        matches!(&events[0], Event::Owner { account } if account.as_str() == "cdx:user-1/ws-1")
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Prompt { text, .. } if text == "Fix the failing test"))
    );
    let reqs = requests(&events);
    // Two responses, each written twice (record + token_count): the ids say they are two.
    assert_eq!(reqs.len(), 4);
    assert_eq!(reqs[0].msg_id, format!("cdx:{SID}:20300"));
    assert_eq!(reqs[1].msg_id, reqs[0].msg_id);
    assert_eq!(reqs[2].msg_id, format!("cdx:{SID}:41800"));
    assert_eq!(reqs[3].msg_id, reqs[2].msg_id);
    let r = reqs[0];
    assert_eq!(r.model, "gpt-6-luna");
    assert_eq!(r.effort.as_deref(), Some("high"));
    assert_eq!(r.cwd.as_deref(), Some("/w/shop"));
    assert_eq!(r.git_branch.as_deref(), Some("main"));
    // 20,000 input of which 11,000 cached: 9,000 fresh. Reasoning is part of the output.
    assert_eq!(r.usage.input, 9_000);
    assert_eq!(r.usage.cache_read, 11_000);
    assert_eq!(r.usage.output, 300);
    assert_eq!(r.usage.thinking, 120);
    assert!(r.thinking);
    // The call came with the first copy of its response.
    assert_eq!(r.tool_uses.len(), 1);
    assert_eq!(r.tool_uses[0].id, format!("cdx:{SID}:call_1"));
    assert_eq!(r.tool_uses[0].summary, "cargo test");
    assert!(reqs[1].tool_uses.is_empty());
    let patch = &reqs[2].tool_uses[0];
    assert_eq!(patch.name, "apply_patch");
    assert_eq!(patch.target.as_deref(), Some("/w/shop/src/lib.rs"));
    assert_eq!(reqs[2].usage.input, 1_000);
    assert!(events.iter().any(|e| matches!(e,
        Event::ToolResult { tool_use_id, is_error: true, .. } if *tool_use_id == format!("cdx:{SID}:call_1"))));
    assert!(events.iter().any(|e| matches!(e,
        Event::ToolResult { tool_use_id, is_error: false, .. } if *tool_use_id == format!("cdx:{SID}:call_2"))));
    assert!(matches!(
        events.last(),
        Some(Event::TurnEnd {
            duration_ms: 14000,
            ..
        })
    ));
}

#[test]
fn a_repeated_token_count_is_the_same_request() {
    let mut lines = turn();
    lines.extend(response(
        "2026-11-24T10:00:16Z",
        usage(21_000, 20_000, 500, 0, 21_500),
        usage(41_000, 31_000, 800, 120, 41_800),
    ));
    let (events, ..) = parse_all(&lines);
    let mut ids: Vec<&str> = requests(&events)
        .iter()
        .map(|r| r.msg_id.as_str())
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 2);
}

#[test]
fn an_output_before_its_usage_writes_the_call_first() {
    let lines = [
        line(
            "2026-11-24T10:00:01Z",
            "turn_context",
            json!({"cwd": "/w/shop", "model": "gpt-5.6-terra"}),
        ),
        line(
            "2026-11-24T10:00:02Z",
            "response_item",
            json!({"type": "local_shell_call", "call_id": "c9", "status": "completed",
                   "action": {"type": "exec", "command": ["ls", "-la"]}}),
        ),
        line(
            "2026-11-24T10:00:03Z",
            "response_item",
            json!({"type": "function_call_output", "call_id": "c9", "output": "ok"}),
        ),
        line(
            "2026-11-24T10:00:04Z",
            "event_msg",
            json!({"type": "turn_aborted"}),
        ),
    ];
    let (events, ..) = parse_all(&lines);
    let reqs = requests(&events);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].model, "<synthetic>");
    assert_eq!(reqs[0].tool_uses[0].summary, "ls -la");
    assert!(matches!(&events[1], Event::ToolResult { .. }));
}

#[test]
fn the_2025_format_is_counted_as_legacy_and_read_no_further() {
    let lines = [
        r#"{"id":"x","timestamp":"2025-08-14T17:15:09.721Z","instructions":"…","git":{"branch":"b"}}"#.to_string(),
        r#"{"record_type":"state"}"#.to_string(),
        r#"{"type":"function_call","id":null,"name":"shell","arguments":"{}","call_id":"c"}"#
            .to_string(),
    ];
    let (events, stats, _) = parse_all(&lines);
    assert!(events.is_empty());
    assert_eq!(stats.by_type.get(LEGACY), Some(&3));
}

#[test]
fn a_read_from_an_offset_knows_the_model_of_the_lines_before() {
    let t = tempfile::tempdir().unwrap();
    let path = t
        .path()
        .join(format!("rollout-2026-11-24T10-00-00-{SID}.jsonl"));
    let lines = turn();
    // Up to and with the first response.
    let head: String = lines[..8].iter().map(|l| format!("{l}\n")).collect();
    let all: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, &all).unwrap();
    // No read of this file before: the state at the offset is replayed from the file.
    let c = read_from(&path, head.len() as u64, SID).unwrap();
    assert_eq!(c.end, all.len() as u64);
    let reqs = requests(&c.events);
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].model, "gpt-6-luna");
    assert_eq!(reqs[0].git_branch.as_deref(), Some("main"));
    // Read in two steps: the same as in one.
    let path2 = t.path().join("second.jsonl");
    std::fs::write(&path2, &head).unwrap();
    let first = read_from(&path2, 0, SID).unwrap();
    std::fs::write(&path2, &all).unwrap();
    let second = read_from(&path2, first.end, SID).unwrap();
    assert_eq!(requests(&second.events), reqs);
    // A line still being written is left for the next read.
    std::fs::write(&path2, format!("{all}{{\"timestamp\":")).unwrap();
    assert_eq!(read_from(&path2, 0, SID).unwrap().end, all.len() as u64);
}

#[test]
fn the_tail_says_whether_a_turn_runs() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("r.jsonl");
    let lines = turn();
    let running: String = lines[..6].iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, &running).unwrap();
    let s = tail_state(&path).unwrap();
    assert!(s.busy);
    assert_eq!(s.cwd.as_deref(), Some("/w/shop"));
    let all: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, &all).unwrap();
    let s = tail_state(&path).unwrap();
    assert!(!s.busy);
    assert_eq!(s.text.as_deref(), Some("Fixed the test."));
    assert_eq!(last_message(&path).as_deref(), Some("Fixed the test."));
}

#[test]
fn a_patch_target_takes_the_separator_of_its_directory() {
    assert_eq!(join_like("/w/shop", "src/lib.rs"), "/w/shop/src/lib.rs");
    assert_eq!(
        join_like("C:\\w\\shop\\", "src/lib.rs"),
        "C:\\w\\shop\\src\\lib.rs"
    );
    assert!(is_relative("src/lib.rs"));
    assert!(
        !is_relative("/w/shop/a.rs") && !is_relative("C:\\w\\a.rs") && !is_relative("\\\\host\\a")
    );
}

#[test]
fn a_code_mode_script_is_named_after_the_tools_it_calls() {
    // Codex 0.172 runs tools from a script, as one `exec` call (seen on a real rollout).
    let script = "const r1 = await tools.exec_command({cmd:\"ls -la\",yield_time_ms:10000});\ntext(r1.output);\nconst r2 = await tools.exec_command({cmd:\"sleep 25 && wc -l notes.txt\",yield_time_ms:30000});\ntext(r2.output);";
    assert_eq!(
        code_mode(script),
        Some((
            "exec_command".to_string(),
            "ls -la; sleep 25 && wc -l notes.txt".to_string()
        ))
    );
    assert_eq!(
        code_mode(
            "await tools.exec_command({\"cmd\": \"echo \\\"hi\\\"\"}); await tools.view_image({path: \"a.png\"});"
        ),
        Some(("exec".to_string(), "echo \"hi\"".to_string()))
    );
    assert_eq!(code_mode("text('no tools here')"), None);

    let mut lines = turn();
    lines.push(line(
        "2026-11-24T10:00:20Z",
        "response_item",
        json!({"type": "custom_tool_call", "name": "exec", "call_id": "call_9", "input": script}),
    ));
    lines.extend(response(
        "2026-11-24T10:00:21Z",
        usage(1_000, 0, 10, 0, 1_010),
        json!({"input_tokens": 99_000, "total_tokens": 99_000}),
    ));
    let (events, ..) = parse_all(&lines);
    let call = requests(&events)
        .into_iter()
        .flat_map(|r| r.tool_uses.iter())
        .find(|t| t.id == format!("cdx:{SID}:call_9"))
        .cloned()
        .unwrap();
    assert_eq!(call.name, "exec_command");
    assert_eq!(
        crate::core::tools::ToolClass::of(&call.name),
        crate::core::tools::ToolClass::Shell
    );
    assert_eq!(call.summary, "ls -la; sleep 25 && wc -l notes.txt");
}
