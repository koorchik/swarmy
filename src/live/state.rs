//! How an agent's state is decided: a pure function of what the index, the session registry and
//! the clock say, tested by table.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::claude::registry::{LiveSession, SessionStatus};
use crate::index::from_ms;
use crate::index::live::OpenTool;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum State {
    /// Blocked on the human (permission, question, dialog).
    NeedsYou(String),
    /// Running a tool.
    Tool {
        name: String,
        summary: String,
    },
    /// Waiting for the model.
    Thinking,
    /// Idle main thread with background agents still running.
    WaitingAgents(usize),
    Idle,
    /// Unfinished, no transcript activity for a while.
    Stalled,
    Done,
    /// The Claude Code process is gone.
    Gone,
}

impl State {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            State::Tool { .. } | State::Thinking | State::WaitingAgents(_)
        )
    }
}

pub struct Options {
    pub stall_minutes: i64,
    /// Finished agents stay on screen this long.
    pub linger_minutes: i64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            stall_minutes: 20,
            linger_minutes: 3,
        }
    }
}

/// What decides an agent's state.
#[derive(Debug, Clone, Copy)]
pub struct StateInputs<'a> {
    /// The main thread of a session (else a subagent).
    pub is_main: bool,
    /// Subagents: the transcript says it finished.
    pub finished: bool,
    /// The running Claude Code session the agent belongs to (its registry entry, process alive).
    pub session: Option<&'a LiveSession>,
    /// The newest tool call since the agent's last request that has not finished.
    pub tool: Option<&'a OpenTool>,
    /// The agent's last activity (transcript, request or tool result).
    pub last: DateTime<Utc>,
}

/// The state of an agent and since when, or `None` when it is no longer shown.
///
/// - Main thread: exited (`Gone`) without a running session, else what the session registry says
///   (needs you, a tool or thinking while busy, idle).
/// - Subagent: `Done` once finished; `Gone` when its session exited before it finished (it can
///   not continue, so it is not "stalled"); `Stalled` when its session runs but the agent was
///   silent longer than `stall_minutes`; else a tool or thinking.
/// - Hidden: done agents after `linger_minutes`, stalled ones after a day, and subagents of an
///   exited session as long after as their main thread is (`stall_minutes`, at least 30).
pub fn agent_state(
    i: &StateInputs,
    opts: &Options,
    now: DateTime<Utc>,
) -> Option<(State, DateTime<Utc>)> {
    let silent = now - i.last;
    let status_since = || i.session.and_then(|l| l.status_since).unwrap_or(i.last);
    let tool = |t: &OpenTool| {
        (
            State::Tool {
                name: t.name.clone(),
                summary: t.summary.clone().unwrap_or_default(),
            },
            from_ms(t.ts),
        )
    };
    let (state, since) = if i.is_main {
        match i.session.map(|l| &l.status) {
            None => (State::Gone, i.last),
            Some(SessionStatus::Waiting(w)) => (State::NeedsYou(w.clone()), status_since()),
            Some(SessionStatus::Busy) => match i.tool {
                Some(t) => tool(t),
                None => (State::Thinking, status_since()),
            },
            Some(SessionStatus::Idle | SessionStatus::Other(_)) => (State::Idle, status_since()),
        }
    } else if i.finished {
        (State::Done, i.last)
    } else if i.session.is_none() {
        (State::Gone, i.last)
    } else if silent > Duration::minutes(opts.stall_minutes) {
        (State::Stalled, i.last)
    } else {
        match i.tool {
            Some(t) => tool(t),
            None => (State::Thinking, i.last),
        }
    };
    let hidden = match &state {
        State::Done => silent > Duration::minutes(opts.linger_minutes),
        State::Stalled => silent > Duration::hours(24),
        State::Gone => !i.is_main && silent > Duration::minutes(opts.stall_minutes.max(30)),
        State::NeedsYou(_)
        | State::Tool { .. }
        | State::Thinking
        | State::WaitingAgents(_)
        | State::Idle => false,
    };
    (!hidden).then_some((state, since))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(min: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-20T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::minutes(min)
    }

    fn session(status: SessionStatus, since: Option<i64>) -> LiveSession {
        LiveSession {
            seat: "work".into(),
            pid: 4242,
            session_id: "s1".into(),
            cwd: "/w/proj".into(),
            name: None,
            status,
            pane: None,
            version: None,
            kind: None,
            started_at: t(-120),
            status_since: since.map(t),
        }
    }

    fn bash(at: i64) -> OpenTool {
        OpenTool {
            name: "Bash".into(),
            summary: Some("cargo test".into()),
            ts: t(at).timestamp_millis(),
        }
    }

    fn running_bash() -> State {
        State::Tool {
            name: "Bash".into(),
            summary: "cargo test".into(),
        }
    }

    #[test]
    fn agent_states_by_table() {
        // Defaults: stalled after 20 min of silence, done agents linger 3 min. Now = t(0).
        let opts = Options::default();
        let busy = session(SessionStatus::Busy, Some(-5));
        let idle = session(SessionStatus::Idle, Some(-7));
        let asks = session(SessionStatus::Waiting("permission".into()), Some(-2));
        let other = session(SessionStatus::Other("compacting".into()), None);
        let tool = bash(-1);
        // (case, main, finished, session, tool, last activity, expected)
        type Case<'a> = (
            &'a str,
            bool,
            bool,
            Option<&'a LiveSession>,
            Option<&'a OpenTool>,
            i64,
            Option<(State, i64)>,
        );
        let cases: Vec<Case> = vec![
            // Main threads.
            (
                "main, session exited",
                true,
                false,
                None,
                None,
                -10,
                Some((State::Gone, -10)),
            ),
            (
                "main, needs you",
                true,
                false,
                Some(&asks),
                None,
                -10,
                Some((State::NeedsYou("permission".into()), -2)),
            ),
            (
                "main, busy in a tool",
                true,
                false,
                Some(&busy),
                Some(&tool),
                -1,
                Some((running_bash(), -1)),
            ),
            (
                "main, busy thinking",
                true,
                false,
                Some(&busy),
                None,
                -1,
                Some((State::Thinking, -5)),
            ),
            (
                "main, idle",
                true,
                false,
                Some(&idle),
                None,
                -30,
                Some((State::Idle, -7)),
            ),
            (
                "main, other status, no time",
                true,
                false,
                Some(&other),
                None,
                -4,
                Some((State::Idle, -4)),
            ),
            // Subagents of a running session.
            (
                "sub, in a tool",
                false,
                false,
                Some(&busy),
                Some(&tool),
                -1,
                Some((running_bash(), -1)),
            ),
            (
                "sub, thinking",
                false,
                false,
                Some(&busy),
                None,
                -3,
                Some((State::Thinking, -3)),
            ),
            (
                "sub, silent 20 min is not yet stalled",
                false,
                false,
                Some(&busy),
                None,
                -20,
                Some((State::Thinking, -20)),
            ),
            (
                "sub, silent 21 min",
                false,
                false,
                Some(&busy),
                None,
                -21,
                Some((State::Stalled, -21)),
            ),
            (
                "sub, stalled for a day",
                false,
                false,
                Some(&busy),
                None,
                -24 * 60,
                Some((State::Stalled, -24 * 60)),
            ),
            (
                "sub, stalled over a day: hidden",
                false,
                false,
                Some(&busy),
                None,
                -24 * 60 - 1,
                None,
            ),
            (
                "sub, done 3 min ago",
                false,
                true,
                Some(&busy),
                None,
                -3,
                Some((State::Done, -3)),
            ),
            (
                "sub, done 4 min ago: hidden",
                false,
                true,
                Some(&busy),
                None,
                -4,
                None,
            ),
            // Subagents of an exited session: gone with it, never stalled (review finding).
            (
                "sub, session exited",
                false,
                false,
                None,
                None,
                -2,
                Some((State::Gone, -2)),
            ),
            (
                "sub, session exited, a tool was open",
                false,
                false,
                None,
                Some(&tool),
                -1,
                Some((State::Gone, -1)),
            ),
            (
                "sub, session exited 30 min ago",
                false,
                false,
                None,
                None,
                -30,
                Some((State::Gone, -30)),
            ),
            (
                "sub, session exited 31 min ago: hidden",
                false,
                false,
                None,
                None,
                -31,
                None,
            ),
            (
                "sub, session exited 3 h ago: hidden, not stalled",
                false,
                false,
                None,
                None,
                -180,
                None,
            ),
            (
                "sub, finished, session exited",
                false,
                true,
                None,
                None,
                -1,
                Some((State::Done, -1)),
            ),
        ];
        for (name, is_main, finished, session, tool, last, want) in cases {
            let got = agent_state(
                &StateInputs {
                    is_main,
                    finished,
                    session,
                    tool,
                    last: t(last),
                },
                &opts,
                t(0),
            );
            assert_eq!(got, want.map(|(s, at)| (s, t(at))), "{name}");
        }
    }
}
