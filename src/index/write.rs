//! Writing what was read of one transcript file, inside the batch's transaction.

use rusqlite::{OptionalExtension as _, params};

use super::{AgentOf, Work, marks, ms, schema};
use crate::claude::files::{self, FileKind};
use crate::claude::transcript::{self, Event, TitleKind};
use crate::core::ids::SeatId;

#[derive(Default)]
struct FileAgg {
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    cwd: Option<String>,
    title: Option<String>,
    ai_title: Option<String>,
}

/// The times a write is measured against (epoch ms).
pub(super) struct At {
    /// Spend older than this is settled (see [`super::SETTLED_AFTER_MS`]).
    pub settled_before: i64,
    /// `meta.rolled_until`, read in the write transaction.
    pub rolled_until: i64,
}

/// What writing one file did.
pub(super) struct Written {
    /// A request older than `At::settled_before` was written: settled spend changed.
    pub late: bool,
    /// Records skipped because they were rolled up already (`marks`).
    pub skipped: u64,
    /// The agents this file's session took copied rows from (`takes_over`): their totals changed.
    pub taken_from: Vec<AgentOf>,
}

/// When a session's responses started and ended (epoch ms), as far as the index and the read in
/// hand know.
#[derive(Clone, Copy)]
struct Span {
    first: Option<i64>,
    last: Option<i64>,
}

/// Who holds a row another file wrote first (a response, a prompt or an API error that a fork or
/// a resumed session repeats, same id and time): its seat, session and agent, and that
/// session's span.
struct Held {
    seat: String,
    session: String,
    agent: String,
    span: Span,
}

/// The holders of rows by their record's id; the session's span from `sessions`.
const REQUEST_HELD: &str =
    "SELECT r.seat, r.session_id, r.agent_id, s.first_ts, s.last_ts FROM requests r
     LEFT JOIN sessions s ON s.seat = r.seat AND s.session_id = r.session_id WHERE r.msg_id = ?1";
const PROMPT_HELD: &str = "SELECT r.seat, r.session_id, '', s.first_ts, s.last_ts FROM prompts r
     LEFT JOIN sessions s ON s.seat = r.seat AND s.session_id = r.session_id WHERE r.record_id = ?1";
const ERROR_HELD: &str = "SELECT r.seat, r.session_id, r.agent_id, s.first_ts, s.last_ts FROM api_errors r
     LEFT JOIN sessions s ON s.seat = r.seat AND s.session_id = r.session_id WHERE r.record_id = ?1";

fn held(tx: &rusqlite::Transaction, sql: &str, id: &str) -> rusqlite::Result<Option<Held>> {
    tx.prepare_cached(sql)?
        .query_row([id], |r| {
            Ok(Held {
                seat: r.get(0)?,
                session: r.get(1)?,
                agent: r.get(2)?,
                span: Span {
                    first: r.get(3)?,
                    last: r.get(4)?,
                },
            })
        })
        .optional()
}

/// Whether the session of `span` takes a copied row from the session holding it. Nothing in a
/// fork's records names the session they were copied from, so the rule is one that every read
/// order agrees on: the row belongs to the session that started first; on a tie (a fork or a
/// resume that copied the whole session), to the one that ended first, since the copy holds
/// the original and more. Rows stay where they are when the two cannot be told apart, when a
/// span is unknown, and between seats of one session (a moved config dir).
fn takes_over(span: Span, session: &str, held: &Held) -> bool {
    use std::cmp::Ordering;
    if held.session == session {
        return false;
    }
    let (Some(first), Some(their_first)) = (span.first, held.span.first) else {
        return false;
    };
    match first.cmp(&their_first) {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal => matches!((span.last, held.span.last), (Some(l), Some(t)) if l < t),
    }
}

/// Marks agents finished whose transcripts did not change (`source::finished_late`). The caller
/// counts them as changed, so the live view reads them again.
pub(super) fn finish(conn: &mut rusqlite::Connection, agents: &[AgentOf]) -> anyhow::Result<()> {
    if agents.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for (seat, session, agent) in agents {
        tx.prepare_cached(
            "UPDATE agents SET finished = 1 WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3",
        )?
        .execute(params![seat, session, agent])?;
    }
    tx.commit()?;
    Ok(())
}

/// Writes what was read of one file. Records the index rolled up already are skipped (requests
/// with their tool calls, prompts, API errors; see `marks`); everything else about them (times,
/// working directory, owner) still counts, as if they had been read.
/// What a transcript's side files say (a session's custom title, an agent's `meta.json`): read
/// with the transcript, before the write transaction, so the lock another process and the
/// compactor wait on is not held over file reads (review B-M23).
pub(super) struct Side {
    title: Option<String>,
    meta: files::AgentMeta,
}

pub(super) fn read_side(f: &crate::model::files::TranscriptFile) -> Side {
    let title = (f.kind == FileKind::Main)
        .then(|| f.path.parent().map(|p| p.join(&f.session_id)))
        .flatten()
        .and_then(|dir| files::read_custom_title(&dir));
    let mut meta = match &f.inline_meta {
        Some(m) => m.clone(),
        None => f
            .meta_path()
            .and_then(|p| files::read_meta(&p))
            .unwrap_or_default(),
    };
    // A workflow agent's meta never says so: its run's record does.
    meta.finished |= files::run_record(f).is_some_and(|r| files::run_ended(&r));
    Side { title, meta }
}

pub(super) fn write_file(
    tx: &rusqlite::Transaction,
    w: &Work,
    chunk: transcript::Chunk,
    side: Side,
    cwds: &mut std::collections::HashSet<String>,
    at: At,
    rowid: &mut i64,
) -> anyhow::Result<Written> {
    let mut late = false;
    let mut skipped = 0;
    let mut refused: i64 = 0;
    let mut taken_from: Vec<AgentOf> = Vec::new();
    let marked = match w.mark {
        marks::Mark::New => false,
        marks::Mark::Repeats => true,
        marks::Mark::MaybeCopies => marks::all_held(tx, &chunk.events, at.rolled_until)?,
    };
    let rolled = |t: i64| marks::rolled_up(marked, at.rolled_until, t);
    // Rows older than this were rolled up and are not read again: a reset keeps them.
    let kept_before = if marked { at.rolled_until } else { i64::MIN };
    let f = &w.file;
    let main = f.kind == FileKind::Main;
    if w.reset {
        // The file is read again from the start: forget the rows only it adds, so a file that
        // changed does not keep what it no longer says. Rows with keys (requests, tool calls,
        // prompts, errors, compactions) would not count twice anyway. Prompts and compactions
        // belong to the main transcript: a subagent's file never touches them.
        if main {
            tx.prepare_cached(
                "DELETE FROM prompts WHERE seat = ?1 AND session_id = ?2 AND ts >= ?3",
            )?
            .execute(params![f.seat, f.session_id, kept_before])?;
            tx.prepare_cached("DELETE FROM compact_events WHERE seat = ?1 AND session_id = ?2")?
                .execute(params![f.seat, f.session_id])?;
        }
        tx.prepare_cached("DELETE FROM api_errors WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3 AND ts >= ?4")?.execute(params![f.seat, f.session_id, f.agent_id, kept_before])?;
        tx.prepare_cached("DELETE FROM record_types WHERE path = ?1")?
            .execute([f.path.to_string_lossy()])?;
    }
    // Exactly once per byte range: the offset is checked and advanced in this transaction.
    // Lines that are no record are counted too: what they held is lost, and `doctor` says so.
    let malformed = (chunk.stats.malformed > 0)
        .then(|| (transcript::MALFORMED.to_string(), chunk.stats.malformed));
    for (t, n) in chunk
        .stats
        .by_type
        .iter()
        .chain(malformed.iter().map(|(t, n)| (t, n)))
    {
        tx.prepare_cached(
            "INSERT INTO record_types(path, type, count) VALUES (?1, ?2, ?3)
             ON CONFLICT(path, type) DO UPDATE SET count = count + excluded.count",
        )?
        .execute(params![f.path.to_string_lossy(), t, *n as i64])?;
    }
    // This file's session's span, as far as the index and this read know: what decides which
    // session a copied row belongs to (`takes_over`). An aggregate: one row, NULLs for a session
    // the index has not seen.
    let (known_first, known_last): (Option<i64>, Option<i64>) = tx
        .prepare_cached(
            "SELECT MIN(first_ts), MAX(last_ts) FROM sessions WHERE seat = ?1 AND session_id = ?2",
        )?
        .query_row(params![f.seat, f.session_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let span = chunk.events.iter().fold(
        Span {
            first: known_first,
            last: known_last,
        },
        |s, e| {
            if let Event::Request(r) = e {
                let t = ms(r.ts);
                Span {
                    first: Some(s.first.map_or(t, |x| x.min(t))),
                    last: Some(s.last.map_or(t, |x| x.max(t))),
                }
            } else {
                s
            }
        },
    );
    // Responses this write inserted or claimed: their later records are the same response.
    let mut mine: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Whether each response of a marked file was rolled up, decided by its first record.
    let mut first_seen: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    let mut account = w.account.clone();
    let mut last_ts = w.last_ts;
    let mut agg = FileAgg::default();
    // The branch of the request before (a lookup only when it changes).
    let mut branch: Option<String> = None;
    let mut ins_req = tx.prepare_cached(&format!(
        "INSERT INTO requests(msg_id, seat, session_id, agent_id, account_uuid, ts, model, requested_model, effort, fast, cwd,
                              input, cw5, cw1h, cread, output, thinking, web, rowid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
         ON CONFLICT(msg_id) DO UPDATE SET
           input = MAX(input, excluded.input), cw5 = MAX(cw5, excluded.cw5), cw1h = MAX(cw1h, excluded.cw1h),
           cread = MAX(cread, excluded.cread), output = MAX(output, excluded.output), thinking = MAX(thinking, excluded.thinking),
           web = MAX(web, excluded.web),
           account_uuid = COALESCE(account_uuid, excluded.account_uuid), {}
         RETURNING rowid",
        schema::unpriced_sql()
    ))?;
    let mut ins_tool = tx.prepare_cached(
        "INSERT INTO tool_calls(id, msg_id, seat, session_id, agent_id, ts, name, summary, target) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO NOTHING",
    )?;
    let mut upd_tool =
        tx.prepare_cached("UPDATE tool_calls SET done_ts = ?1, is_error = ?2 WHERE id = ?3")?;
    let mut count_tools = tx.prepare_cached(
        "UPDATE requests SET tools = (SELECT COUNT(*) FROM tool_calls WHERE msg_id = ?1) WHERE msg_id = ?1",
    )?;
    // What the read says of the agent's end: stopped when the interruption is the last thing in
    // it, nothing when it holds no event at all (what the index has then stands).
    let stopped = matches!(chunk.events.last(), Some(Event::Interrupted { .. }));
    let quiet = chunk.events.is_empty();
    for ev in chunk.events {
        match ev {
            Event::Owner { account: owner_id } => {
                tx.prepare_cached("INSERT INTO owners(seat, session_id, ts, account_uuid) VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING")?.execute(params![f.seat, f.session_id, last_ts, owner_id.as_str()])?;
                account = Some(owner_id.to_string());
            }
            Event::Request(r) => {
                let t = ms(r.ts);
                last_ts = last_ts.max(t);
                agg.first_ts = Some(agg.first_ts.map_or(t, |x| x.min(t)));
                agg.last_ts = Some(agg.last_ts.map_or(t, |x| x.max(t)));
                if let Some(b) = r.git_branch.as_deref().filter(|b| !b.is_empty())
                    && branch.as_deref() != Some(b)
                {
                    record_branch(tx, f, t, b)?;
                    branch = Some(b.to_string());
                }
                if r.cwd.is_some() {
                    agg.cwd = r.cwd.clone();
                    if let Some(c) = &r.cwd
                        && !cwds.contains(c)
                    {
                        cwds.insert(c.clone());
                    }
                }
                // One response is several records (one per content block), each with its own
                // time; its raw row carries the first one's, and the compactor rolled it up by
                // that time. So the response is skipped or written whole, as its first record
                // decides, never block by block (a later block past `rolled_until` would come
                // back as a new row).
                let gone = marked && *first_seen.entry(r.msg_id.clone()).or_insert(rolled(t));
                if gone {
                    skipped += 1;
                    // Tool calls are rolled up by their own time: one at or after
                    // `rolled_until` is still raw (or gone with a cache rebuild) and is kept.
                    if !rolled(t) {
                        insert_tools(&mut ins_tool, f, &r, t)?;
                    }
                    continue;
                }
                let u = r.usage;
                // A count SQLite cannot hold (beyond i64) is no real usage: the record is refused
                // and counted, never wrapped to a negative that fails the batch on every run.
                let tok = [
                    u.input,
                    u.cache_write_5m,
                    u.cache_write_1h,
                    u.cache_read,
                    u.output,
                    u.thinking,
                    u.web_search,
                ]
                .map(i64::try_from);
                let [
                    Ok(input),
                    Ok(cw5),
                    Ok(cw1h),
                    Ok(cread),
                    Ok(output),
                    Ok(thinking),
                    Ok(web),
                ] = tok
                else {
                    refused += 1;
                    continue;
                };
                late |= t < at.settled_before;
                // Numbered above every rowid ever given out (`compact::requests_rowid_high`); a
                // merge into an existing row leaves the number unused.
                *rowid += 1;
                let account_here = if main { account.clone() } else { None };
                let row: i64 = ins_req.query_row(
                    params![
                        r.msg_id,
                        f.seat,
                        f.session_id,
                        f.agent_id,
                        account_here,
                        t,
                        r.model,
                        r.requested_model,
                        r.effort,
                        r.fast as i64,
                        r.cwd,
                        input,
                        cw5,
                        cw1h,
                        cread,
                        output,
                        thinking,
                        web,
                        *rowid,
                    ],
                    |x| x.get(0),
                )?;
                // Merged into a row another file wrote first: a copy, which may be this
                // session's to take (with its tool calls; the account follows the session, or
                // the next pass gives it the session's owner).
                if row != *rowid
                    && !mine.contains(&r.msg_id)
                    && let Some(h) = held(tx, REQUEST_HELD, &r.msg_id)?
                    && takes_over(span, &f.session_id, &h)
                {
                    tx.prepare_cached("UPDATE requests SET seat = ?2, session_id = ?3, agent_id = ?4, account_uuid = ?5, cwd = COALESCE(?6, cwd) WHERE msg_id = ?1")?
                        .execute(params![r.msg_id, f.seat, f.session_id, f.agent_id, account_here, r.cwd])?;
                    tx.prepare_cached("UPDATE tool_calls SET seat = ?2, session_id = ?3, agent_id = ?4 WHERE msg_id = ?1")?
                        .execute(params![r.msg_id, f.seat, f.session_id, f.agent_id])?;
                    taken_from.push((SeatId::from_stored(h.seat), h.session, h.agent));
                }
                mine.insert(r.msg_id.clone());
                insert_tools(&mut ins_tool, f, &r, t)?;
                // A response's tool calls are counted, not added up: its records may be read
                // more than once, in any order.
                if !r.tool_uses.is_empty() {
                    count_tools.execute([&r.msg_id])?;
                }
            }
            Event::ToolResult {
                tool_use_id,
                ts,
                is_error,
            } => {
                if let Some(t) = ts {
                    last_ts = last_ts.max(ms(t));
                }
                upd_tool.execute(params![ts.map(ms), is_error as i64, tool_use_id])?;
            }
            Event::Prompt { ts, text, id } => {
                if let Some(t) = ts {
                    last_ts = last_ts.max(ms(t));
                    if main && rolled(ms(t)) {
                        skipped += 1;
                    } else if main {
                        // A record id already counted (a fork, a moved config dir) is not counted again
                        // (review B-M8): `ON CONFLICT` covers the unique `record_id` too.
                        let n = tx.prepare_cached("INSERT INTO prompts(seat, session_id, ts, chars, record_id) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING")?.execute(params![f.seat, f.session_id, ms(t), text.chars().count() as i64, id])?;
                        if n == 0
                            && let Some(id) = &id
                            && let Some(h) = held(tx, PROMPT_HELD, id)?
                            && takes_over(span, &f.session_id, &h)
                        {
                            tx.prepare_cached("UPDATE OR IGNORE prompts SET seat = ?2, session_id = ?3 WHERE record_id = ?1")?
                                .execute(params![id, f.seat, f.session_id])?;
                            taken_from.push((
                                SeatId::from_stored(h.seat),
                                h.session,
                                f.agent_id.clone(),
                            ));
                        }
                    }
                }
            }
            Event::Title { kind, text } => match kind {
                TitleKind::Custom => agg.title = Some(text),
                TitleKind::Ai => agg.ai_title = Some(text),
                TitleKind::AgentName => {
                    agg.title.get_or_insert(text);
                }
            },
            Event::TurnEnd { ts, .. } => {
                if let Some(t) = ts {
                    last_ts = last_ts.max(ms(t));
                }
            }
            Event::Compact { ts } => {
                if main {
                    let t = ts.map(ms).unwrap_or(last_ts);
                    tx.prepare_cached("INSERT INTO compact_events(seat, session_id, ts) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING")?.execute(params![f.seat, f.session_id, t])?;
                }
            }
            Event::AgentLaunch { .. } | Event::Interrupted { .. } => {}
            Event::CostState { total_usd, models } => {
                // Cumulative per process; a resumed session starts a new count, so keep the largest.
                // A map that cannot be written is an error, never an empty text in the column
                // (review A-M8).
                let models = serde_json::to_string(&models)?;
                tx.prepare_cached("INSERT INTO cost_states(seat, session_id, total_usd, models) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(seat, session_id) DO UPDATE SET total_usd = excluded.total_usd, models = excluded.models
                     WHERE excluded.total_usd >= cost_states.total_usd")?.execute(params![f.seat, f.session_id, total_usd, models])?;
            }
            Event::ApiError { ts, kind, text, id } => {
                let t = ts.map(ms).unwrap_or(last_ts);
                if rolled(t) {
                    skipped += 1;
                    continue;
                }
                let n = tx.prepare_cached("INSERT INTO api_errors(seat, session_id, agent_id, ts, kind, text, record_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT DO NOTHING")?.execute(params![f.seat, f.session_id, f.agent_id, t, kind, text, id])?;
                if n == 0
                    && let Some(id) = &id
                    && let Some(h) = held(tx, ERROR_HELD, id)?
                    && takes_over(span, &f.session_id, &h)
                {
                    tx.prepare_cached("UPDATE OR IGNORE api_errors SET seat = ?2, session_id = ?3, agent_id = ?4 WHERE record_id = ?1")?
                        .execute(params![id, f.seat, f.session_id, f.agent_id])?;
                    taken_from.push((SeatId::from_stored(h.seat), h.session, h.agent));
                }
            }
        }
    }

    if refused > 0 {
        tx.prepare_cached(
            "INSERT INTO record_types(path, type, count) VALUES (?1, ?2, ?3)
             ON CONFLICT(path, type) DO UPDATE SET count = count + excluded.count",
        )?
        .execute(params![
            f.path.to_string_lossy(),
            transcript::MALFORMED,
            refused
        ])?;
    }
    if main {
        if let Some(t) = side.title {
            agg.title = Some(t);
        }
        if agg.title.is_none() && f.session_title.is_some() {
            agg.title = f.session_title.clone();
        }
        if agg.cwd.is_none() && f.default_cwd.is_some() {
            agg.cwd = f.default_cwd.clone();
            if let Some(c) = &f.default_cwd
                && !cwds.contains(c)
            {
                cwds.insert(c.clone());
            }
        }
        tx.prepare_cached("INSERT INTO sessions(seat, session_id, title, ai_title, cwd, first_ts, last_ts, compactions)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, (SELECT COUNT(*) FROM compact_events WHERE seat = ?1 AND session_id = ?2))
             ON CONFLICT(seat, session_id) DO UPDATE SET
               title = COALESCE(excluded.title, title), ai_title = COALESCE(excluded.ai_title, ai_title), cwd = COALESCE(excluded.cwd, cwd),
               first_ts = MIN(COALESCE(first_ts, excluded.first_ts), COALESCE(excluded.first_ts, first_ts)),
               last_ts = MAX(COALESCE(last_ts, excluded.last_ts), COALESCE(excluded.last_ts, last_ts)),
               compactions = excluded.compactions")?.execute(params![f.seat, f.session_id, agg.title, agg.ai_title, agg.cwd, agg.first_ts, agg.last_ts])?;
    }
    let meta = side.meta;
    let workflow_run = match &f.kind {
        FileKind::Workflow(r) => Some(r.clone()),
        FileKind::Main | FileKind::Subagent => None,
    };
    tx.prepare_cached("INSERT INTO agents(seat, session_id, agent_id, kind, workflow_run, agent_type, description, tool_use_id, parent_agent_id, depth,
                            requested_model, effort, finished, first_ts, last_ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(seat, session_id, agent_id) DO UPDATE SET
           agent_type = COALESCE(excluded.agent_type, agent_type), description = COALESCE(excluded.description, description),
           tool_use_id = COALESCE(excluded.tool_use_id, tool_use_id), parent_agent_id = COALESCE(excluded.parent_agent_id, parent_agent_id),
           depth = COALESCE(excluded.depth, depth), requested_model = COALESCE(excluded.requested_model, requested_model),
           effort = COALESCE(excluded.effort, effort),
           finished = CASE WHEN ?16 THEN MAX(finished, excluded.finished) ELSE excluded.finished END,
           first_ts = MIN(COALESCE(first_ts, excluded.first_ts), COALESCE(excluded.first_ts, first_ts)),
           last_ts = MAX(COALESCE(last_ts, excluded.last_ts), COALESCE(excluded.last_ts, last_ts))")?.execute(params![
            f.seat,
            f.session_id,
            f.agent_id,
            f.kind.tag(),
            workflow_run,
            meta.agent_type,
            meta.description,
            meta.tool_use_id,
            meta.parent_agent_id,
            meta.spawn_depth.map(|d| d as i64).or(if main { Some(0) } else { None }),
            meta.model,
            meta.effort,
            (meta.finished || main || stopped) as i64,
            agg.first_ts,
            agg.last_ts,
            quiet,
        ])?;
    tx.prepare_cached("INSERT INTO files(path, seat, session_id, agent_id, kind, ino, size, mtime, offset, last_ts, account_uuid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(path) DO UPDATE SET ino = excluded.ino, size = excluded.size, mtime = excluded.mtime, offset = excluded.offset,
           last_ts = excluded.last_ts, account_uuid = excluded.account_uuid")?.execute(params![
            f.path.to_string_lossy(),
            f.seat,
            f.session_id,
            f.agent_id,
            f.kind.tag(),
            w.ino as i64,
            w.size as i64,
            w.mtime,
            chunk.end as i64,
            last_ts,
            account,
        ])?;
    Ok(Written {
        late,
        skipped,
        taken_from,
    })
}

/// The tool calls of one record of response `r` (at `t`, the record's time). A tool id is written
/// once; a record read again adds nothing.
fn insert_tools(
    ins_tool: &mut rusqlite::CachedStatement,
    f: &crate::claude::files::TranscriptFile,
    r: &transcript::Request,
    t: i64,
) -> rusqlite::Result<()> {
    for tu in r.tool_uses.iter().filter(|tu| !tu.id.is_empty()) {
        ins_tool.execute(params![
            tu.id,
            r.msg_id,
            f.seat,
            f.session_id,
            f.agent_id,
            t,
            tu.name,
            tu.summary,
            tu.target
        ])?;
    }
    Ok(())
}

/// Records that the agent of `f` worked on `branch` at `t`, unless the branch in force then is
/// that one already. Rows are only added: a file read again in any order gives the same answer
/// for every request (the latest row at or before it).
fn record_branch(
    tx: &rusqlite::Transaction,
    f: &files::TranscriptFile,
    t: i64,
    branch: &str,
) -> rusqlite::Result<()> {
    use rusqlite::OptionalExtension;
    let now: Option<String> = tx
        .prepare_cached(
            "SELECT branch FROM branches WHERE seat = ?1 AND session_id = ?2 AND agent_id = ?3
             AND from_ts <= ?4 ORDER BY from_ts DESC LIMIT 1",
        )?
        .query_row(params![f.seat, f.session_id, f.agent_id, t], |r| r.get(0))
        .optional()?;
    if now.as_deref() != Some(branch) {
        tx.prepare_cached(
            "INSERT OR IGNORE INTO branches(seat, session_id, agent_id, from_ts, branch)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![f.seat, f.session_id, f.agent_id, t, branch])?;
    }
    Ok(())
}
