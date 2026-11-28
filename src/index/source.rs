//! Per-harness transcript discovery and incremental chunk reading for the index.
//!
//! Keeps harness dispatch in one place so `index::Index` stays harness-agnostic and future
//! harnesses (e.g. OpenCode) plug in here without growing `src/index/mod.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::Connection;

use crate::agy::summaries as agy_summaries;
use crate::claude::files::{self, FileKind, TranscriptFile};
use crate::claude::seats::{Seat, SeatId};
use crate::claude::transcript;
use crate::core::harness::Harness;

/// The dirs a session's files were listed from (with their mtimes), and the files.
pub(super) type ListedDirs = (Vec<(PathBuf, Option<SystemTime>)>, Vec<TranscriptFile>);

/// Lists every transcript file belonging to `seat`, dispatching by `seat.harness`.
pub(super) fn list_seat_files(seat: &Seat) -> Vec<TranscriptFile> {
    match seat.harness {
        Harness::Claude => files::list(seat),
        Harness::Agy => {
            let metas = agy_summaries::read_all(&seat.dir);
            crate::agy::meta::list_transcripts(&seat.id, &seat.dir, &metas)
        }
        Harness::Codex => crate::codex::meta::list_transcripts(&seat.id, &seat.dir),
    }
}

/// Reads the new bytes of `f` starting at `offset` using the harness-specific transcript parser.
pub(super) fn read_transcript_chunk(
    f: &TranscriptFile,
    offset: u64,
) -> std::io::Result<transcript::Chunk> {
    match f.harness {
        Harness::Claude => transcript::read_from(&f.path, offset),
        Harness::Agy => {
            let conv_id = if f.agent_id.is_empty() {
                f.session_id.as_str()
            } else {
                f.agent_id.as_str()
            };
            crate::agy::transcript::read_from(&f.path, offset, conv_id, f.default_cwd.as_deref())
        }
        Harness::Codex => crate::codex::rollout::read_from(&f.path, offset, &f.session_id),
    }
}

/// Lists transcript files recently modified in the `files` table (`mtime >= since`), hydrating
/// Antigravity files with targeted `conversation_summaries.db` metadata and Codex files with
/// their thread names.
pub(super) fn recent_files(
    conn: &Connection,
    seats: &[Seat],
    since: i64,
) -> anyhow::Result<Vec<TranscriptFile>> {
    let mut out: Vec<TranscriptFile> = Vec::new();
    let mut agy_recent: HashMap<SeatId, Vec<String>> = HashMap::new();
    {
        let mut st = conn.prepare(
            "SELECT path, seat, session_id, agent_id, kind FROM files WHERE mtime >= ?1",
        )?;
        let rows = st.query_map([since], |r| {
            let kind: String = r.get(4)?;
            let seat_id: SeatId = r.get(1)?;
            let session_id: String = r.get(2)?;
            let agent_id: String = r.get(3)?;
            let path = PathBuf::from(r.get::<_, String>(0)?);
            Ok((path, seat_id, session_id, agent_id, kind))
        })?;
        for r in rows {
            let (path, seat_id, session_id, agent_id, kind) = r?;
            // A file of a seat this process does not know (a harness turned off, a seat found
            // only through another process's environment) is not read: which parser fits it is
            // unknown, and a wrong one would move the offset past its spend.
            let Some(seat) = seats.iter().find(|s| s.id == seat_id) else {
                continue;
            };
            match seat.harness {
                Harness::Agy => {
                    let cid = if agent_id.is_empty() {
                        session_id
                    } else {
                        agent_id
                    };
                    agy_recent.entry(seat_id).or_default().push(cid);
                    continue;
                }
                Harness::Codex => {
                    let titles = crate::codex::meta::titles(&seat.dir);
                    out.extend(crate::codex::meta::transcript_file(&seat.id, path, &titles));
                    continue;
                }
                Harness::Claude => {}
            }
            out.push(TranscriptFile {
                path,
                seat: seat_id,
                session_id,
                agent_id,
                kind: match kind.as_str() {
                    "main" => FileKind::Main,
                    "subagent" => FileKind::Subagent,
                    other => FileKind::Workflow(other.to_string()),
                },
                harness: Harness::Claude,
                default_cwd: None,
                session_title: None,
                inline_meta: None,
            });
        }
    }
    for (seat_id, cids) in agy_recent {
        if let Some(seat) = seats.iter().find(|s| s.id == seat_id) {
            let metas = agy_summaries::read_all(&seat.dir);
            for cid in &cids {
                if let Some(tf) =
                    crate::agy::meta::transcript_file(&seat.id, &seat.dir, cid, &metas)
                {
                    out.push(tf);
                }
            }
        }
    }
    Ok(out)
}

/// Lists the files of a live session, caching Claude directory listings by directory mtime.
pub(super) fn session_files(
    cache: &mut HashMap<(SeatId, String), ListedDirs>,
    seat: &Seat,
    sid: &str,
) -> Vec<TranscriptFile> {
    match seat.harness {
        Harness::Agy => {
            let metas = agy_summaries::read_all(&seat.dir);
            return crate::agy::meta::list_session_transcripts(&seat.id, &seat.dir, sid, &metas);
        }
        // A thread is one rollout, found once and remembered (`codex::meta::find_rollout`).
        Harness::Codex => {
            let titles = crate::codex::meta::titles(&seat.dir);
            return crate::codex::meta::find_rollout(&seat.dir, sid)
                .and_then(|p| crate::codex::meta::transcript_file(&seat.id, p, &titles))
                .into_iter()
                .collect();
        }
        Harness::Claude => {}
    }
    let key = (seat.id.clone(), sid.to_string());
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if let Some((dirs, cached)) = cache.get(&key)
        && !dirs.is_empty()
        && dirs.iter().all(|(d, t)| mtime(d) == *t)
    {
        return cached.clone();
    }
    let list = files::list_session(seat, sid);
    // Watch the dirs that hold these files (main dir, subagents, workflow runs).
    let mut dirs: Vec<PathBuf> = list
        .iter()
        .filter_map(|f| f.path.parent().map(Path::to_path_buf))
        .collect();
    // Also dirs that may not exist yet: creating them is how a first subagent shows up.
    for f in list.iter().filter(|f| f.kind == FileKind::Main) {
        if let Some(pdir) = f.path.parent() {
            let base = pdir.join(sid);
            dirs.push(base.join("subagents").join("workflows"));
            dirs.push(base.join("subagents"));
            dirs.push(base);
        }
    }
    dirs.sort();
    dirs.dedup();
    let stamped = dirs
        .into_iter()
        .map(|d| {
            let t = mtime(&d);
            (d, t)
        })
        .collect();
    cache.insert(key, (stamped, list.clone()));
    list
}

/// Whether the agent of an unchanged transcript finished since it was read. Claude Code writes
/// `"finished": true` into the agent's `meta.json` a moment after the agent's last transcript
/// line, so a refresh between the two has read the whole transcript with the old meta, and the
/// transcript never changes again; a workflow agent is finished by its run's record, written
/// when the run ends (`files::run_ended`). Cheap enough for every pass: one file looked at per
/// unfinished agent, and the meta is read only when it is not older than the transcript (the
/// one written at the spawn is).
pub(super) fn finished_late(f: &TranscriptFile, transcript: &std::fs::Metadata) -> bool {
    if let Some(record) = files::run_record(f) {
        return files::run_ended(&record);
    }
    let Some(path) = f.meta_path() else {
        return false;
    };
    // No meta, or no times on this file system: nothing says it finished.
    let newer = match (
        std::fs::metadata(&path).and_then(|m| m.modified()),
        transcript.modified(),
    ) {
        (Ok(meta), Ok(transcript)) => meta >= transcript,
        (Err(_), Ok(_) | Err(_)) | (Ok(_), Err(_)) => false,
    };
    newer && files::read_meta(&path).is_some_and(|m| m.finished)
}
