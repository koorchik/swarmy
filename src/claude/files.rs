//! The transcript files of a seat and the metadata of subagents.
//!
//! ```text
//! projects/<encoded cwd>/<session>.jsonl                                main thread
//! projects/<encoded cwd>/<session>/subagents/agent-<id>.jsonl           subagent (+ .meta.json)
//! projects/<encoded cwd>/<session>/subagents/workflows/<run>/agent-<id>.jsonl   workflow agent
//! projects/<encoded cwd>/<session>/workflows/<run>.json                 a workflow run that ended
//! ```

use std::path::{Path, PathBuf};

use super::seats::Seat;
use crate::core::harness::Harness;
pub use crate::model::files::{AgentMeta, FileKind, TranscriptFile};

fn jsonl_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    v.sort();
    v
}

fn agent_id_of(p: &Path) -> Option<String> {
    let stem = p.file_stem()?.to_string_lossy();
    stem.strip_prefix("agent-").map(str::to_string)
}

fn claude_file(
    path: PathBuf,
    seat: &Seat,
    session_id: String,
    agent_id: String,
    kind: FileKind,
) -> TranscriptFile {
    TranscriptFile {
        path,
        seat: seat.id.clone(),
        session_id,
        agent_id,
        kind,
        harness: Harness::Claude,
        default_cwd: None,
        session_title: None,
        inline_meta: None,
    }
}

pub fn list(seat: &Seat) -> Vec<TranscriptFile> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(seat.projects_dir()) else {
        return out;
    };
    for proj in projects.flatten() {
        let pdir = proj.path();
        if !pdir.is_dir() {
            continue;
        }
        // Main transcripts, and session dirs (which may exist without a main file).
        let mut sessions: Vec<String> = Vec::new();
        for f in jsonl_in(&pdir) {
            let sid = f
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            sessions.push(sid.clone());
            out.push(claude_file(f, seat, sid, String::new(), FileKind::Main));
        }
        let Ok(entries) = std::fs::read_dir(&pdir) else {
            continue;
        };
        for e in entries.flatten() {
            let sdir = e.path();
            if !sdir.is_dir() {
                continue;
            }
            let sid = e.file_name().to_string_lossy().into_owned();
            let sub = sdir.join("subagents");
            for f in jsonl_in(&sub) {
                if let Some(aid) = agent_id_of(&f) {
                    out.push(claude_file(f, seat, sid.clone(), aid, FileKind::Subagent));
                }
            }
            if let Ok(runs) = std::fs::read_dir(sub.join("workflows")) {
                for run in runs.flatten() {
                    let run_id = run.file_name().to_string_lossy().into_owned();
                    for f in jsonl_in(&run.path()) {
                        if let Some(aid) = agent_id_of(&f) {
                            out.push(claude_file(
                                f,
                                seat,
                                sid.clone(),
                                aid,
                                FileKind::Workflow(run_id.clone()),
                            ));
                        }
                    }
                }
            }
        }
    }
    out
}

/// The files of one session (main thread, subagents, workflow agents) wherever its project dir is.
pub fn list_session(seat: &Seat, session_id: &str) -> Vec<TranscriptFile> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(seat.projects_dir()) else {
        return out;
    };
    for proj in projects.flatten() {
        let pdir = proj.path();
        let main = pdir.join(format!("{session_id}.jsonl"));
        let sdir = pdir.join(session_id);
        if !main.exists() && !sdir.is_dir() {
            continue;
        }
        if main.exists() {
            out.push(claude_file(
                main,
                seat,
                session_id.into(),
                String::new(),
                FileKind::Main,
            ));
        }
        let sub = sdir.join("subagents");
        for f in jsonl_in(&sub) {
            if let Some(aid) = agent_id_of(&f) {
                out.push(claude_file(
                    f,
                    seat,
                    session_id.into(),
                    aid,
                    FileKind::Subagent,
                ));
            }
        }
        if let Ok(runs) = std::fs::read_dir(sub.join("workflows")) {
            for run in runs.flatten() {
                let run_id = run.file_name().to_string_lossy().into_owned();
                for f in jsonl_in(&run.path()) {
                    if let Some(aid) = agent_id_of(&f) {
                        out.push(claude_file(
                            f,
                            seat,
                            session_id.into(),
                            aid,
                            FileKind::Workflow(run_id.clone()),
                        ));
                    }
                }
            }
        }
    }
    out
}

pub fn read_meta(path: &Path) -> Option<AgentMeta> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The record of the workflow run a workflow agent belongs to (`<session>/workflows/<run>.json`).
/// The run is named by the transcript's directory, not by `FileKind::Workflow`: a file listed
/// from the index (`index::source::recent_files`) does not carry its run there.
pub fn run_record(f: &TranscriptFile) -> Option<PathBuf> {
    match &f.kind {
        FileKind::Workflow(_) if f.harness == Harness::Claude => {
            // <session>/subagents/workflows/<run>/agent-<id>.jsonl: the session dir is four up.
            let run = f.path.parent()?.file_name()?.to_string_lossy();
            let session = f.path.ancestors().nth(4)?;
            Some(session.join("workflows").join(format!("{run}.json")))
        }
        FileKind::Workflow(_) | FileKind::Main | FileKind::Subagent => None,
    }
}

/// Whether the workflow run of `record` ended. Claude Code never marks a workflow agent's
/// `meta.json` finished: it writes the run's record once, when the run ends (`completed`,
/// `failed` or `killed`), and that is what finishes the run's agents.
pub fn run_ended(record: &Path) -> bool {
    #[derive(serde::Deserialize)]
    struct Run {
        status: String,
    }
    std::fs::read_to_string(record)
        .ok()
        .and_then(|text| serde_json::from_str::<Run>(&text).ok())
        .is_some_and(|run| run.status != "running")
}

/// `custom-title.json` in a session dir.
pub fn read_custom_title(session_dir: &Path) -> Option<String> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(session_dir.join("custom-title.json")).ok()?)
            .ok()?;
    v.get("customTitle")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_main_subagent_and_workflow_files() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("projects/-w-shop");
        std::fs::create_dir_all(p.join("s1/subagents/workflows/wf_1")).unwrap();
        std::fs::create_dir_all(p.join("s2/tool-results")).unwrap();
        std::fs::write(p.join("s1.jsonl"), "").unwrap();
        std::fs::write(p.join("s1/subagents/agent-a1.jsonl"), "").unwrap();
        std::fs::write(
            p.join("s1/subagents/agent-a1.meta.json"),
            r#"{"agentType":"general-purpose","model":"opus","finished":true,"spawnDepth":1}"#,
        )
        .unwrap();
        std::fs::write(p.join("s1/subagents/workflows/wf_1/agent-w1.jsonl"), "").unwrap();
        let seat = Seat::new(
            "default",
            t.path().into(),
            t.path().join(".claude.json"),
            true,
        );
        let files = list(&seat);
        let kinds: Vec<_> = files
            .iter()
            .map(|f| (f.session_id.as_str(), f.agent_id.as_str(), f.kind.tag()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("s1", "", "main"),
                ("s1", "a1", "subagent"),
                ("s1", "w1", "workflow")
            ]
        );
        let meta = read_meta(&files[1].meta_path().unwrap()).unwrap();
        assert_eq!(meta.model.as_deref(), Some("opus"));
        assert!(meta.finished);
        assert_eq!(meta.spawn_depth, Some(1));
    }
}
