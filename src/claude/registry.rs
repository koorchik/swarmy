//! Claude Code's registry of live sessions: `<config dir>/sessions/<pid>.json`.
//!
//! Each interactive Claude Code writes its pid, session id, cwd, name, tmux pane and a status
//! (`busy`, `idle`, `waiting` with `waitingFor`) there. Files of dead processes linger, so a
//! session counts as live only when its pid runs and, on Linux, the start time still matches.

use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use super::procs::{ProcTable, proc_start_ticks};
use crate::core::ids::{SeatId, SessionId};
use crate::core::pane::Pane;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct RawSession {
    pid: u32,
    session_id: String,
    cwd: String,
    started_at: i64,
    proc_start: Option<String>,
    version: Option<String>,
    kind: Option<String>,
    entrypoint: Option<String>,
    tmux: Option<String>,
    name: Option<String>,
    status: Option<String>,
    waiting_for: Option<String>,
    updated_at: Option<i64>,
    status_updated_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveSession {
    pub seat: SeatId,
    pub pid: u32,
    pub session_id: SessionId,
    pub cwd: String,
    pub name: Option<String>,
    pub status: SessionStatus,
    /// Its tmux pane or kitty window, for `J`.
    pub pane: Option<Pane>,
    pub version: Option<String>,
    pub kind: Option<String>,
    pub started_at: DateTime<Utc>,
    pub status_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SessionStatus {
    Busy,
    Idle,
    /// Blocked on the user: a permission prompt, a question, a dialog…
    Waiting(String),
    Other(String),
}

impl SessionStatus {
    fn parse(status: Option<&str>, waiting_for: Option<&str>) -> Self {
        match status.unwrap_or("") {
            "busy" => SessionStatus::Busy,
            "idle" | "" => SessionStatus::Idle,
            "waiting" => SessionStatus::Waiting(waiting_for.unwrap_or("input").to_string()),
            s => SessionStatus::Other(s.to_string()),
        }
    }
}

fn ms(t: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(t).single()
}

/// The pids named in a seat's registry files (alive or not).
pub fn session_pids(sessions_dir: &Path) -> Vec<u32> {
    let Ok(rd) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").and_then(|n| n.parse().ok())
        })
        .collect()
}

/// Live sessions of one seat. Stale files are ignored, never deleted.
pub fn live_sessions(seat: &SeatId, sessions_dir: &Path, procs: &ProcTable) -> Vec<LiveSession> {
    let Ok(rd) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(r) = serde_json::from_str::<RawSession>(&text) else {
            continue;
        };
        // A record without a session id names nothing the index knows (review M10).
        if r.pid == 0 || r.session_id.is_empty() || !procs.alive(r.pid) {
            continue;
        }
        if let (Some(want), Some(have)) = (&r.proc_start, proc_start_ticks(r.pid))
            && *want != have
        {
            continue; // the pid was reused by another process
        }
        out.push(LiveSession {
            seat: seat.clone(),
            pid: r.pid,
            session_id: SessionId::new(r.session_id),
            cwd: r.cwd,
            name: r.name,
            status: SessionStatus::parse(r.status.as_deref(), r.waiting_for.as_deref()),
            pane: super::pane::of(r.tmux, r.pid, r.proc_start.as_deref()),
            version: r.version,
            kind: r.kind.or(r.entrypoint),
            started_at: ms(r.started_at).unwrap_or_else(Utc::now),
            status_since: r.status_updated_at.or(r.updated_at).and_then(ms),
        });
    }
    out.sort_by_key(|s| s.started_at);
    out
}

/// A session blocked on the user.
#[derive(Debug, Clone)]
pub struct Waiting {
    pub session_id: SessionId,
    pub name: Option<String>,
    pub what: String,
    pub pane: Option<Pane>,
    pub cwd: String,
}

impl Waiting {
    /// Its title, else its directory's name.
    pub fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.cwd.rsplit('/').next().unwrap_or("session").to_string())
    }
}

/// Sessions of one config dir that wait for the user, read straight from the registry with a
/// cheap liveness check (for the status line when the dashboard is not running).
pub fn waiting_sessions(sessions_dir: &Path) -> Vec<Waiting> {
    let Ok(rd) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(r) = std::fs::read_to_string(&p)
            .ok()
            .and_then(|t| serde_json::from_str::<RawSession>(&t).ok())
        else {
            continue;
        };
        if r.status.as_deref() != Some("waiting") || r.pid == 0 || !pid_alive(r.pid) {
            continue;
        }
        out.push(Waiting {
            session_id: SessionId::new(r.session_id),
            name: r.name,
            what: r.waiting_for.unwrap_or_else(|| "input".into()),
            pane: super::pane::of(r.tmux, r.pid, r.proc_start.as_deref()),
            cwd: r.cwd,
        });
    }
    out
}

fn pid_alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        Path::new("/proc").join(pid.to_string()).exists()
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parsing() {
        assert_eq!(
            SessionStatus::parse(Some("busy"), None),
            SessionStatus::Busy
        );
        assert_eq!(
            SessionStatus::parse(Some("waiting"), Some("input needed")),
            SessionStatus::Waiting("input needed".into())
        );
        assert_eq!(SessionStatus::parse(None, None), SessionStatus::Idle);
    }

    #[test]
    fn reads_own_process_and_skips_dead_ones() {
        let t = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let start = proc_start_ticks(me)
            .map(|s| format!(",\"procStart\":\"{s}\""))
            .unwrap_or_default();
        std::fs::write(
            t.path().join(format!("{me}.json")),
            format!(r#"{{"pid":{me},"sessionId":"s1","cwd":"/w","startedAt":1795348783929{start},"name":"n","status":"waiting","waitingFor":"input needed","tmux":"0:@0.%6"}}"#),
        )
        .unwrap();
        std::fs::write(
            t.path().join("999999999.json"),
            r#"{"pid":999999999,"sessionId":"dead","cwd":"/w","startedAt":1}"#,
        )
        .unwrap();
        std::fs::write(t.path().join("x.key"), "secret").unwrap();
        // Review M10: no session id is no session (it would show as an empty short id).
        let other = std::process::id();
        std::fs::write(
            t.path().join("1.json"),
            format!(r#"{{"pid":{other},"cwd":"/w","startedAt":1795348783929}}"#),
        )
        .unwrap();
        let procs = ProcTable::snapshot();
        let seat = SeatId::from_stored("0123456789abcdef");
        let s = live_sessions(&seat, t.path(), &procs);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].seat, seat);
        assert_eq!(s[0].session_id, "s1");
        assert_eq!(s[0].status, SessionStatus::Waiting("input needed".into()));
        assert_eq!(
            s[0].pane,
            Some(Pane::Tmux {
                pane: "0:@0.%6".into()
            })
        );
    }
}
