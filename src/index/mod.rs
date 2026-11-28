//! The local index: an SQLite cache of every transcript, filled incrementally, next to the facts
//! swarmy observed over time (logins per seat, limit readings, price versions; see `schema`).
//!
//! Transcripts are append-only, so each file is read from where the previous run stopped
//! (tracked by inode, size and offset). A file that shrank or was replaced is read again from the
//! start. The first run reads everything in parallel.

// A new variant of a domain enum (a harness, a window kind, a dim) must be handled where it
// is matched, not fall into a catch-all (reviews A-H7, B-M24, B-L3).
#![deny(clippy::wildcard_enum_match_arm)]
// No result ignored, no string cut by bytes, and (where there are none) no silent numeric
// casts: the strict lints switched on, in production code, where they hold already (reviews
// A-H7, B-M24).
#![cfg_attr(not(test), deny(clippy::let_underscore_must_use))]
#![cfg_attr(not(test), deny(clippy::string_slice))]

#[cfg(test)]
mod account_scenarios;
mod accounts;
#[cfg(test)]
mod agy_scenarios;
pub mod archive;
#[cfg(test)]
pub(crate) mod archive_scenarios;
#[cfg(test)]
mod checkout_scenarios;
#[cfg(test)]
mod codex_scenarios;
pub mod compact;
#[cfg(test)]
mod compact_scenarios;
pub mod insights;
pub mod live;
mod marks;
mod prices;
mod projects;
pub mod query;
mod records;
#[cfg(test)]
pub(crate) mod scenarios;
pub mod schema;
#[cfg(test)]
mod seat_scenarios;
pub(crate) mod seats;
mod settled;
mod source;
pub mod storage;
mod write;

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use chrono::{DateTime, TimeZone, Utc};
use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::claude::config_json::{Account, Utilization};
use crate::claude::files::TranscriptFile;
use crate::claude::project::Resolver;
use crate::claude::seats::{Seat, SeatId};
use crate::claude::transcript;
use crate::core::harness::Harness;
use crate::core::pricing::PriceBook;

/// The account of a request (review H1). The transcripts say which account a request used:
/// the session's `session-owner` record in force at the request (else its first one) decides.
/// The seat's login history (`seat_accounts`) only fills in for sessions with no owner record:
/// the login in force at the request, else the seat's earliest known one.
///
/// Except after a change of the seat's login (a row naming another account than the row before
/// it; the seat's first row is no change) for a session that began before it: Claude Code reads
/// `.credentials.json` again when it changes, so its requests go out under the new login at
/// once, while its `session-owner` records keep naming the account it started with (verified
/// on 2.1.332: a vault switch under a running session). From such a change on, the seat's login
/// decides for that session.
const SEAT_ACCOUNT_AT: &str = "COALESCE(
  (SELECT sa.account_uuid FROM seat_accounts sa WHERE sa.seat = requests.seat AND sa.ts <= requests.ts
     AND sa.ts > (SELECT s.first_ts FROM sessions s WHERE s.seat = requests.seat AND s.session_id = requests.session_id)
     AND (SELECT p.account_uuid FROM seat_accounts p WHERE p.seat = sa.seat AND p.ts < sa.ts ORDER BY p.ts DESC LIMIT 1) <> sa.account_uuid
     ORDER BY sa.ts DESC LIMIT 1),
  (SELECT o.account_uuid FROM owners o WHERE o.seat = requests.seat AND o.session_id = requests.session_id AND o.ts <= requests.ts ORDER BY o.ts DESC LIMIT 1),
  (SELECT o.account_uuid FROM owners o WHERE o.seat = requests.seat AND o.session_id = requests.session_id ORDER BY o.ts ASC LIMIT 1),
  (SELECT sa.account_uuid FROM seat_accounts sa WHERE sa.seat = requests.seat AND sa.ts <= requests.ts ORDER BY sa.ts DESC LIMIT 1),
  (SELECT sa.account_uuid FROM seat_accounts sa WHERE sa.seat = requests.seat ORDER BY sa.ts ASC LIMIT 1),
  requests.account_uuid)";

/// Revision of the rule that picks a request's account (`SEAT_ACCOUNT_AT`); a change
/// re-attributes every owner-backed request once (`rule_owners`). 2: a login change of the seat
/// decides for sessions that began before it.
const OWNER_RULE_REV: &str = "2";

/// When the index was attributed under an older `OWNER_RULE_REV`, the next refresh attributes
/// the requests with owner records again (`accounts::OWNERS_PENDING`). An index with no requests
/// yet has nothing to move.
fn rule_owners(conn: &Connection) -> rusqlite::Result<()> {
    if schema::meta(conn, "owner_rule_rev")?.as_deref() == Some(OWNER_RULE_REV) {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    let any: bool = tx.query_row("SELECT EXISTS (SELECT 1 FROM requests)", [], |r| r.get(0))?;
    if any {
        schema::set_meta(&tx, accounts::OWNERS_PENDING, "1")?;
    }
    schema::set_meta(&tx, "owner_rule_rev", OWNER_RULE_REV)?;
    tx.commit()
}

/// Revision of the cwd → project rules; a change recomputes the `projects` table.
const PROJECTS_REV: &str = "5";

pub struct Index {
    conn: Connection,
    /// Session dirs listed by the hot refresh, with their modification time: a dir is listed
    /// again only when a file was added to it.
    session_dirs: HashMap<(SeatId, String), source::ListedDirs>,
    /// The last price book dated by `dated_prices`, by the fingerprint of the undated one.
    dated: Option<(String, PriceBook)>,
    /// Settled spend already summed (`query::settled_total`).
    settled: std::cell::RefCell<HashMap<SettledKey, f64>>,
    /// The seats (id, label, harness) last named in the `seats` table (`seats::adopt_seats`).
    adopted: Option<Vec<(SeatId, String, Harness)>>,
    /// The (seat, account) pairs and known accounts last checked for legacy bare-uuid migration
    /// (`accounts::adopt_accounts`).
    adopted_accounts: Option<AdoptedAccounts>,
    /// Organization-scoped accounts known beyond the seats (`Index::set_known_accounts`).
    known_accounts: Vec<crate::core::ids::AccountId>,
    /// Which agents this connection wrote, for readers that keep per-agent figures between
    /// refreshes (`agent_seq`).
    changes: Changes,
    /// Settled parts of recurring sums (`settled`).
    parts: settled::Parts,
    /// The instant of the current pass, given by the caller (`Index::set_now`): which rows
    /// are settled, inside the raw window or recent is decided by it, not by the wall clock
    /// read deep inside (reviews A-M6, B-M17). Without it, the wall clock.
    clock: Option<DateTime<Utc>>,
}

/// An agent by (seat, session id, agent id), as its transcript file names it.
type AgentOf = (SeatId, String, String);

/// A sequence number bumped by every write batch: per agent, the batch that last wrote one of
/// its files; `all`, the last change that may touch any agent (another process committed).
#[derive(Default)]
struct Changes {
    seq: u64,
    by_agent: HashMap<AgentOf, u64>,
    /// The same per session (seat, session id): the batch that last wrote any of its agents.
    by_session: HashMap<(SeatId, String), u64>,
    all: u64,
    /// SQLite's `data_version` when last looked at: it moves when another connection commits.
    data_version: Option<i64>,
}

impl Changes {
    fn wrote(&mut self, agents: Vec<AgentOf>) {
        if agents.is_empty() {
            return;
        }
        self.seq += 1;
        for a in agents {
            self.by_session.insert((a.0.clone(), a.1.clone()), self.seq);
            self.by_agent.insert(a, self.seq);
        }
    }
}

/// A settled total: (settled generation, price fingerprint, account, span start, span end).
type AdoptedAccounts = (
    Vec<(SeatId, crate::core::ids::AccountId)>,
    Vec<crate::core::ids::AccountId>,
);

type SettledKey = (Option<String>, String, String, i64, i64);

/// How long a write waits for another process's write (a compaction chunk, a VACUUM).
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// The WAL file is cut back to this many bytes after a checkpoint.
const JOURNAL_SIZE_LIMIT: i64 = 32 << 20;

/// Points SQLite's temporary files (the copy a `VACUUM` writes, large sorts) at `dir`, inside
/// swarmy's data dir, instead of the system's temp dir, which is often in RAM. Process-wide, set
/// once (the first call wins).
pub fn use_temp_dir(dir: &Path) -> anyhow::Result<()> {
    static SET: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if SET.get().is_some() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Connection::open_in_memory()?.pragma_update(
        None,
        "temp_store_directory",
        dir.to_string_lossy().as_ref(),
    )?;
    let _first = SET.set(());
    Ok(())
}

/// Spend older than this no longer changes on its own: requests of the last minutes are still
/// being written. Older spend changes only when the index says so, by bumping the settled
/// generation (`bump_settled`): a reprice, a re-attribution to another account, a request indexed
/// late (a file found by the full scan, a file read again).
pub const SETTLED_AFTER_MS: i64 = 5 * 60 * 1000;

/// A reading equal to a stored one (account, window, value) at most this far away is not stored.
const READING_REPEAT_MS: i64 = 5 * 60 * 1000;

/// Marks a change to settled spend (see [`SETTLED_AFTER_MS`]), inside the writing transaction.
fn bump_settled(tx: &Connection) -> rusqlite::Result<()> {
    tx.prepare_cached(
        "INSERT INTO meta(key, value) VALUES ('settled_gen', '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
    )?
    .execute([])?;
    Ok(())
}

/// A file as the index has it: inode, size, offset, account, last record time.
type Known = (u64, u64, u64, Option<String>, i64, bool);

/// `Known`'s last field: whether the file's agent is held as unfinished.
const UNFINISHED: &str = "COALESCE((SELECT a.finished = 0 FROM agents a WHERE a.seat = files.seat
     AND a.session_id = files.session_id AND a.agent_id = files.agent_id), 0)";

/// Up to this many files (the hot refresh), `plan` looks each one up by its path instead of
/// loading the whole `files` table.
const KNOWN_BY_PATH: usize = 512;

// Seat ids are stored as their text (`core` knows nothing of SQLite).
impl rusqlite::ToSql for SeatId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::Borrowed(
            rusqlite::types::ValueRef::Text(self.as_str().as_bytes()),
        ))
    }
}

impl rusqlite::types::FromSql for SeatId {
    fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        String::column_result(v).map(SeatId::from_stored)
    }
}

impl rusqlite::ToSql for crate::core::ids::SessionId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::Borrowed(
            rusqlite::types::ValueRef::Text(self.as_str().as_bytes()),
        ))
    }
}

impl rusqlite::types::FromSql for crate::core::ids::SessionId {
    fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        String::column_result(v).map(crate::core::ids::SessionId::new)
    }
}

/// What `.claude.json` of a seat says right now.
#[derive(Debug, Clone, Default)]
pub struct SeatInfo {
    pub seat: SeatId,
    /// For display only.
    pub label: String,
    pub account: Option<Account>,
    pub utilization: Option<Utilization>,
    /// When the vault last switched the seat's login: the moment a change of its account
    /// happened, which swarmy would otherwise only date when it sees it.
    pub switched_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Progress {
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

#[derive(Debug, Clone, Default)]
pub struct RefreshStats {
    pub files_total: usize,
    pub files_read: usize,
    pub bytes_read: u64,
    pub requests_new: u64,
    /// Records read again that were rolled up already, and skipped (`marks`).
    pub records_skipped: u64,
    /// Transcripts that could not be read (path, error): nothing of them was written, and they
    /// are tried again on the next refresh (`Index::unreadable_files`).
    pub unreadable: Vec<(String, String)>,
    pub elapsed: Duration,
}

struct Work {
    file: TranscriptFile,
    /// The file's `(ino, offset)` in the index when the work was planned (`None`: not indexed).
    /// It is checked again inside the write transaction: when another process advanced the file
    /// meanwhile, this work is dropped instead of adding the same bytes twice.
    expected: Option<(u64, u64)>,
    start: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    account: Option<String>,
    last_ts: i64,
    reset: bool,
    /// Whether records older than `rolled_until` are skipped: what the file may repeat of the
    /// rolled-up records (`marks::Marks::of`).
    mark: marks::Mark,
}

/// Records that the transcript at `path` could not be read (`unreadable_files`).
fn note_unreadable(tx: &Connection, path: &str, e: &std::io::Error) -> rusqlite::Result<()> {
    tx.prepare_cached(
        "INSERT INTO unreadable_files(path, error, since) VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET error = excluded.error WHERE error IS NOT excluded.error",
    )?
    .execute(params![path, e.to_string(), ms(Utc::now())])?;
    Ok(())
}

/// The index's failures that a user can act on, by kind (`IndexError`, coded): SQLite's busy,
/// damaged and disk-full results anywhere in `e`'s chain. Anything else is left as it is.
fn typed(e: anyhow::Error) -> anyhow::Error {
    use rusqlite::ErrorCode as C;
    let code = e
        .chain()
        .find_map(|c| c.downcast_ref::<rusqlite::Error>())
        .and_then(rusqlite::Error::sqlite_error_code);
    let Some(code) = code else { return e };
    let msg = format!("{e:#}");
    // `ErrorCode` is non-exhaustive: every other code stays as it is.
    let kind = if matches!(code, C::DatabaseBusy | C::DatabaseLocked) {
        crate::errors::IndexError::Busy(msg)
    } else if matches!(code, C::DatabaseCorrupt | C::NotADatabase) {
        crate::errors::IndexError::Damaged(msg)
    } else if code == C::DiskFull {
        crate::errors::IndexError::NoSpace(msg)
    } else {
        return e;
    };
    kind.into()
}

pub fn ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

pub fn from_ms(t: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(t)
        .single()
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// A file identity as the index stores it: SQLite integers are signed and the column is
/// `CHECK (ino >= 0)`, so the top bit goes. Windows' file index mixed with the volume serial uses
/// all 64 bits (real NTFS volumes, not Wine's), which used to fail every insert there.
fn file_key(ino: u64) -> u64 {
    ino & i64::MAX.unsigned_abs()
}

impl Index {
    /// Sets the instant of the passes that follow (the dashboard's tick, a command's `now`).
    pub fn set_now(&mut self, now: DateTime<Utc>) {
        self.clock = Some(now);
    }

    /// The instant of this pass ([`Index::set_now`]), else the wall clock.
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.unwrap_or_else(Utc::now)
    }

    pub fn open(path: &Path) -> anyhow::Result<Index> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        Self::init(conn).map_err(typed)
    }

    fn init(conn: Connection) -> anyhow::Result<Index> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // The live view runs a couple of dozen distinct statements every second (`live.rs`);
        // keep them all prepared, with the writer's statements, instead of the default 16.
        conn.set_prepared_statement_cache_capacity(128);
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // The WAL shrinks back to this after a checkpoint (a compaction or a VACUUM grows it).
        conn.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT)?;
        // Facts migrate, the cache is rebuilt when its version changed; requests read again
        // then follow their seat's login history like any new request (`post_pass`).
        schema::migrate(&conn)?;
        rule_owners(&conn)?;
        Ok(Index {
            conn,
            session_dirs: HashMap::new(),
            dated: None,
            settled: Default::default(),
            adopted: None,
            adopted_accounts: None,
            known_accounts: Vec::new(),
            changes: Changes::default(),
            parts: settled::Parts::default(),
            clock: None,
        })
    }

    pub fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(schema::meta(&self.conn, key)?)
    }

    pub fn set_meta(&mut self, key: &str, value: &str) -> anyhow::Result<()> {
        Ok(schema::set_meta(&self.conn, key, value)?)
    }

    /// Changes whenever a refresh of this index wrote something of the agent's (its transcript
    /// was read further) or another process committed to the index since. Equal numbers mean
    /// the agent's rows read the same as before, except for what a reprice, a re-attribution or
    /// a compaction rewrote, which readers pick up on their slower cadence.
    pub fn agent_seq(&self, a: live::AgentKey) -> u64 {
        let key = (
            a.seat.clone(),
            a.session_id.as_str().to_string(),
            a.agent_id.to_string(),
        );
        let all = self.changes.all;
        self.changes
            .by_agent
            .get(&key)
            .map_or(all, |s| (*s).max(all))
    }

    /// `agent_seq` for a whole session: moves when any of its agents was written.
    pub fn session_seq(&self, seat: &SeatId, session_id: &crate::core::ids::SessionId) -> u64 {
        let key = (seat.clone(), session_id.as_str().to_string());
        let all = self.changes.all;
        self.changes
            .by_session
            .get(&key)
            .map_or(all, |s| (*s).max(all))
    }

    /// Notes a commit by another connection (a second dashboard, a command's refresh): every
    /// agent counts as changed then.
    fn note_other_writers(&mut self) -> anyhow::Result<()> {
        let v: i64 = self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?;
        if self.changes.data_version.is_some_and(|old| old != v) {
            self.changes.seq += 1;
            self.changes.all = self.changes.seq;
        }
        self.changes.data_version = Some(v);
        Ok(())
    }

    /// Brings the index up to date with every seat's transcripts.
    pub fn refresh(
        &mut self,
        seats: &[Seat],
        seat_info: &[SeatInfo],
        resolver: &mut Resolver,
        prices: &PriceBook,
        progress: &mut dyn FnMut(Progress),
    ) -> anyhow::Result<RefreshStats> {
        let all: Vec<TranscriptFile> = seats.iter().flat_map(source::list_seat_files).collect();
        self.refresh_files(all, seats, seat_info, resolver, prices, progress)
    }

    /// A cheap refresh for the live view: only files touched in the last 30 minutes and the files
    /// of running sessions (including subagents spawned since the last full scan).
    pub fn refresh_hot(
        &mut self,
        seats: &[Seat],
        live: &[(SeatId, String)],
        seat_info: &[SeatInfo],
        resolver: &mut Resolver,
        prices: &PriceBook,
    ) -> anyhow::Result<RefreshStats> {
        let since = self.now().timestamp() - 1800;
        let mut out = source::recent_files(&self.conn, seats, since)?;
        for (seat_id, sid) in live {
            let Some(seat) = seats.iter().find(|s| &s.id == seat_id) else {
                continue;
            };
            out.extend(source::session_files(&mut self.session_dirs, seat, sid));
        }
        let mut seen = std::collections::HashSet::new();
        out.retain(|f| seen.insert(f.path.clone()));
        let mut quiet = |_p: Progress| {};
        self.refresh_files(out, seats, seat_info, resolver, prices, &mut quiet)
    }

    fn refresh_files(
        &mut self,
        all: Vec<TranscriptFile>,
        seats: &[Seat],
        seat_info: &[SeatInfo],
        resolver: &mut Resolver,
        prices: &PriceBook,
        progress: &mut dyn FnMut(Progress),
    ) -> anyhow::Result<RefreshStats> {
        self.refresh_files_untyped(all, seats, seat_info, resolver, prices, progress)
            .map_err(typed)
    }

    fn refresh_files_untyped(
        &mut self,
        all: Vec<TranscriptFile>,
        seats: &[Seat],
        seat_info: &[SeatInfo],
        resolver: &mut Resolver,
        prices: &PriceBook,
        progress: &mut dyn FnMut(Progress),
    ) -> anyhow::Result<RefreshStats> {
        let started = Instant::now();
        self.note_other_writers()?;
        let files_total = all.len();
        // Rows an older swarmy keyed by label move to their seat's id before anything is written;
        // legacy bare account uuids move to `<account>/<org>` before planning so multi-org files
        // re-read their `session-owner` records in the same pass.
        self.adopt_seats(seats)?;
        let reattribute_owners = self.adopt_accounts(seat_info)?;
        let (work, finished) = self.plan(&all, seats)?;
        let from_start = work.iter().any(|w| w.start == 0);
        let mut stats = self.ingest(
            work,
            seat_info,
            resolver,
            prices,
            reattribute_owners,
            progress,
        )?;
        write::finish(&mut self.conn, &finished)?;
        self.changes.wrote(finished);
        if from_start {
            // A seat found after a compaction: its old records go straight into the archive.
            self.roll_late(prices, seats)?;
        }
        stats.files_total = files_total;
        stats.elapsed = started.elapsed();
        Ok(stats)
    }

    /// What to read of each file: from the stored offset when the file only grew, from the start
    /// when it was replaced or shrank (`reset`), nothing when it is unchanged. A file read from
    /// its start gets its read marks here, before anything of this refresh is written. With it,
    /// the agents of unchanged files that finished since they were read (`finished_late`).
    fn plan(
        &self,
        all: &[TranscriptFile],
        seats: &[Seat],
    ) -> anyhow::Result<(Vec<Work>, Vec<AgentOf>)> {
        let mut known: HashMap<String, Known> = HashMap::new();
        let known_row = |r: &rusqlite::Row| -> rusqlite::Result<Known> {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        };
        if all.len() <= KNOWN_BY_PATH {
            // The hot refresh, every second: a few dozen files, each found by its key, instead
            // of loading every file the index has ever seen.
            let mut st = self.conn.prepare_cached(&format!(
                "SELECT ino, size, offset, account_uuid, last_ts, {UNFINISHED} FROM files WHERE path = ?1"
            ))?;
            for f in all {
                let key = f.path.to_string_lossy().into_owned();
                if let Some(k) = st.query_row([&key], known_row).optional()? {
                    known.insert(key, k);
                }
            }
        } else {
            let mut st = self.conn.prepare(&format!(
                "SELECT ino, size, offset, account_uuid, last_ts, {UNFINISHED}, path FROM files"
            ))?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(6)?, known_row(r)?)))?;
            for row in rows {
                let (k, v) = row?;
                known.insert(k, v);
            }
        }

        let mut work = Vec::new();
        let mut finished = Vec::new();
        let mut marks: Option<marks::Marks> = None;
        for f in all.iter() {
            let Ok(m) = std::fs::metadata(&f.path) else {
                continue;
            };
            let (ino, size) = (file_key(crate::fsutil::file_id(&f.path, &m)), m.len());
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let key = f.path.to_string_lossy().into_owned();
            let expected = known.get(&key).map(|(kino, _, koff, ..)| (*kino, *koff));
            let (start, account, last_ts, reset) = match known.get(&key) {
                // Scheduled for a re-read from the start (`accounts`: `offset > size`), however
                // the file changed since.
                Some((_, ksize, koff, ..)) if *koff > *ksize => (0, None, 0, true),
                Some((kino, ksize, .., unfinished)) if *kino == ino && *ksize == size => {
                    if *unfinished && source::finished_late(f, &m) {
                        finished.push((f.seat.clone(), f.session_id.clone(), f.agent_id.clone()));
                    }
                    continue;
                }
                Some((kino, _, koff, kacc, kts, _)) if *kino == ino && size >= *koff => {
                    (*koff, kacc.clone(), *kts, false)
                }
                Some(_) => (0, None, 0, true),
                None => (0, None, 0, false),
            };
            if start == 0 && marks.is_none() {
                marks = Some(marks::Marks::load(&self.conn, seats)?);
            }
            let mark = match &marks {
                Some(m) => m.of(&self.conn, f, start)?,
                None => marks::Mark::New,
            };
            work.push(Work {
                file: f.clone(),
                expected,
                start,
                ino,
                size,
                mtime,
                account,
                last_ts,
                reset,
                mark,
            });
        }
        Ok((work, finished))
    }

    /// Reads the planned work and writes it, one immediate transaction per batch: the write lock
    /// is taken before the offsets are checked again, so two processes (two dashboards, a CLI run)
    /// never both add the same bytes.
    fn ingest(
        &mut self,
        mut work: Vec<Work>,
        seat_info: &[SeatInfo],
        resolver: &mut Resolver,
        prices: &PriceBook,
        reattribute_owners: bool,
        progress: &mut dyn FnMut(Progress),
    ) -> anyhow::Result<RefreshStats> {
        let bytes_total: u64 = work.iter().map(|w| w.size - w.start).sum();
        let files_total = work.len();
        let mut stats = RefreshStats {
            files_read: files_total,
            ..Default::default()
        };
        let mut done = Progress {
            files_done: 0,
            files_total,
            bytes_done: 0,
            bytes_total,
        };
        progress(done);

        // Big files first so the parallel batches stay balanced.
        work.sort_by_key(|w| std::cmp::Reverse(w.size - w.start));
        // Rows added by this refresh have a larger rowid: rowids are never given out twice, even
        // after the compactor deleted rows (`compact::requests_rowid_high`).
        let last_rowid = compact::requests_rowid_high(&self.conn)?;
        let mut cwds: std::collections::HashSet<String> = std::collections::HashSet::new();
        let settled_before = ms(self.now()) - SETTLED_AFTER_MS;
        for batch in work.chunks(48) {
            // A few small files parse faster on this thread than by waking the whole pool.
            type Parsed<'w> = (&'w Work, std::io::Result<transcript::Chunk>, write::Side);
            fn read(w: &Work) -> Parsed<'_> {
                let chunk = source::read_transcript_chunk(&w.file, w.start);
                (w, chunk, write::read_side(&w.file))
            }
            let mut parsed: Vec<Parsed> = if batch.len() < 8 {
                batch.iter().map(read).collect()
            } else {
                batch.par_iter().map(read).collect()
            };
            // Owner records without an organization take the one they must be in.
            for (w, chunk, _) in &mut parsed {
                if let Ok(c) = chunk {
                    self.complete_owners(&w.file.seat, &mut c.events, seat_info);
                }
            }
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut late = false;
            let mut skipped = 0;
            let mut rowid = compact::requests_rowid_high(&tx)?;
            // Read in the write transaction: a compaction committed since the plan counts.
            let rolled_until = archive::rolled_until(&tx)?;
            let mut written: Vec<AgentOf> = Vec::new();
            for (w, chunk, side) in parsed {
                let path = w.file.path.to_string_lossy();
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        // Never dropped silently: recorded (`doctor`, ATTENTION), returned, and
                        // read again next time (its offset did not move).
                        note_unreadable(&tx, &path, &e)?;
                        stats.unreadable.push((path.into_owned(), e.to_string()));
                        continue;
                    }
                };
                tx.prepare_cached("DELETE FROM unreadable_files WHERE path = ?1")?
                    .execute([&path])?;
                let now: Option<(u64, u64)> = tx
                    .prepare_cached("SELECT ino, offset FROM files WHERE path = ?1")?
                    .query_row([w.file.path.to_string_lossy()], |r| {
                        Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
                    })
                    .optional()?;
                if now != w.expected {
                    // Another process read this file meanwhile; what it wrote stands.
                    continue;
                }
                done.bytes_done += chunk.end - w.start;
                let at = write::At {
                    settled_before,
                    rolled_until,
                };
                let wrote = write::write_file(&tx, w, chunk, side, &mut cwds, at, &mut rowid)?;
                written.push((
                    w.file.seat.clone(),
                    w.file.session_id.clone(),
                    w.file.agent_id.clone(),
                ));
                // The agents a copy was taken from read their totals again too.
                written.extend(wrote.taken_from);
                late |= wrote.late;
                skipped += wrote.skipped;
            }
            if late {
                bump_settled(&tx)?;
            }
            if skipped > 0 {
                // A running count for `doctor`: records found again after they were rolled up.
                tx.prepare_cached("INSERT INTO meta(key, value) VALUES ('skipped_records', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + excluded.value")?.execute([skipped as i64])?;
                stats.records_skipped += skipped;
            }
            tx.commit()?;
            self.changes.wrote(written);
            done.files_done += batch.len();
            progress(done);
        }
        stats.bytes_read = done.bytes_done;

        self.post_pass(
            seat_info,
            resolver,
            prices,
            &cwds,
            (files_total > 0).then_some(last_rowid),
            reattribute_owners,
        )?;
        // Counted by rowid, not by two `COUNT(*)` of the whole table on every hot refresh.
        let added: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM requests WHERE rowid > ?1",
            [last_rowid],
            |r| r.get(0),
        )?;
        stats.requests_new = added.max(0) as u64;
        // Only a refresh that read something commits: a commit makes every other process's
        // per-agent caches read again (`note_other_writers`).
        if done.files_done > 0 {
            self.set_meta("refreshed_at", &self.now().timestamp_millis().to_string())?;
        }
        Ok(stats)
    }

    fn post_pass(
        &mut self,
        seat_info: &[SeatInfo],
        resolver: &mut Resolver,
        prices: &PriceBook,
        new_cwds: &std::collections::HashSet<String>,
        new_after_rowid: Option<i64>,
        reattribute_owners: bool,
    ) -> anyhow::Result<()> {
        let now = ms(self.now());
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reattribute = accounts::record_seat_facts(&tx, seat_info, now)?;
        // Requests of the seat from the change on follow it, unless their transcript names the
        // owner (`SEAT_ACCOUNT_AT`). Rollups are never re-keyed: they were attributed while their
        // raw rows, owners included, were still there, and the archive cannot tell an owner's
        // attribution from the seat's; a change dated at the moment it is seen is newer than
        // `rolled_until` anyway. So no rollup is ever moved on weaker evidence than it was made.
        let mut moved = 0;
        for (seat, ts) in &reattribute {
            moved += tx.execute(
                &format!("UPDATE requests SET account_uuid = {SEAT_ACCOUNT_AT} WHERE seat = ?1 AND ts >= ?2"),
                params![seat, ts],
            )?;
        }
        if reattribute_owners {
            moved += tx.execute(
                &format!(
                    "UPDATE requests SET account_uuid = {SEAT_ACCOUNT_AT}
                     WHERE EXISTS (SELECT 1 FROM owners o WHERE o.seat = requests.seat AND o.session_id = requests.session_id)"
                ),
                [],
            )?;
            // Until every transcript scheduled for a re-read was read again, the next pass does
            // this again (`accounts::OWNERS_PENDING`).
            accounts::finish_reattribution(&tx)?;
        }
        if moved > 0 {
            bump_settled(&tx)?;
        }
        let rev_ok = schema::meta(&tx, "projects_rev")?.as_deref() == Some(PROJECTS_REV);
        let Some(new_after_rowid) = new_after_rowid.or((!rev_ok).then_some(i64::MAX)) else {
            tx.commit()?;
            return self.price_all(prices);
        };
        // New requests follow the same rule: requests read for the first time, which after a
        // cache rebuild is all of them.
        tx.execute(
            &format!(
                "UPDATE requests SET account_uuid = {SEAT_ACCOUNT_AT}
                 WHERE rowid > ?1 AND EXISTS (SELECT 1 FROM seat_accounts sa WHERE sa.seat = requests.seat)"
            ),
            params![new_after_rowid],
        )?;
        // Settled requests that get their account below change settled spend (`bump_settled`).
        let unattributed = |tx: &Connection| -> rusqlite::Result<i64> {
            tx.prepare_cached(
                "SELECT COUNT(*) FROM requests WHERE account_uuid IS NULL AND ts < ?1",
            )?
            .query_row([now - SETTLED_AFTER_MS], |r| r.get(0))
        };
        let unattributed_before = unattributed(&tx)?;
        // The account of subagent requests comes from the session's owner at that time.
        tx.execute_batch(
            "UPDATE requests SET account_uuid = (SELECT o.account_uuid FROM owners o WHERE o.seat = requests.seat
               AND o.session_id = requests.session_id AND o.ts <= requests.ts ORDER BY o.ts DESC LIMIT 1)
             WHERE account_uuid IS NULL;
             UPDATE requests SET account_uuid = (SELECT o.account_uuid FROM owners o WHERE o.seat = requests.seat
               AND o.session_id = requests.session_id ORDER BY o.ts ASC LIMIT 1)
             WHERE account_uuid IS NULL;",
        )?;
        // Transcripts written before Claude Code recorded owners: the seat's current account.
        for si in seat_info {
            if let Some(a) = &si.account {
                tx.prepare_cached("UPDATE requests SET account_uuid = ?1 WHERE account_uuid IS NULL AND seat = ?2")?.execute(params![a.id().as_str(), si.seat])?;
            }
        }
        if unattributed(&tx)? != unattributed_before {
            bump_settled(&tx)?;
        }
        // Projects of new working directories; a new rule revision recomputes every cwd, raw or
        // rolled up, never downgrading one whose dir is gone (`projects`).
        let cwds: Vec<String> = if rev_ok {
            let mut known_cwd = tx.prepare("SELECT 1 FROM projects WHERE cwd = ?1")?;
            new_cwds
                .iter()
                .filter(|c| !known_cwd.exists([c.as_str()]).unwrap_or(false))
                .cloned()
                .collect()
        } else {
            projects::every_cwd(&tx)?
        };
        projects::assign(&tx, resolver, &cwds)?;
        if !rev_ok {
            schema::set_meta(&tx, "projects_rev", PROJECTS_REV)?;
        }
        tx.commit()?;
        self.price_all(prices)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::query::Counted;

    #[test]
    fn a_fresh_index_opens_and_reopens() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("i.sqlite");
        let i = Index::open(&p).unwrap();
        assert_eq!(i.count(Counted::Requests).unwrap(), 0);
        drop(i);
        assert!(Index::open(&p).is_ok());
    }

    /// Review M12: a failure the user can act on reaches the front ends as a coded kind, not a
    /// bare SQLite message: the write lock held by another process (a compaction, a VACUUM),
    /// a damaged file.
    #[test]
    fn index_failures_are_typed() {
        use crate::index::scenarios::{World, assistant, owner};
        let w = World::new(&["work"]);
        w.append(
            "work",
            "s1",
            &[
                owner("acc-a"),
                assistant(
                    "m1",
                    "2026-11-01T10:00:00Z",
                    "claude-sonnet-5-5",
                    "/w",
                    (10, 5),
                    &[],
                ),
            ],
        );
        let path = w.dir.path().join("i.sqlite");
        let mut idx = Index::open(&path).unwrap();
        idx.conn.busy_timeout(Duration::from_millis(50)).unwrap();
        let other = Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let mut resolver = Resolver::new(Vec::new());
        let e = idx
            .refresh(
                &w.seats,
                &[],
                &mut resolver,
                &PriceBook::builtin(),
                &mut |_| {},
            )
            .unwrap_err();
        assert_eq!(crate::errors::code_of(&e), Some("index-busy"), "{e:#}");
        other.execute_batch("ROLLBACK;").unwrap();
        idx.refresh(
            &w.seats,
            &[],
            &mut resolver,
            &PriceBook::builtin(),
            &mut |_| {},
        )
        .unwrap();
        let bad = w.dir.path().join("bad.sqlite");
        std::fs::write(&bad, "not a database ".repeat(40)).unwrap();
        let e = Index::open(&bad).err().unwrap();
        assert_eq!(crate::errors::code_of(&e), Some("index-damaged"), "{e:#}");
    }

    /// `agent_seq` moves for the agent whose transcript was read further, not for the others,
    /// and for every agent when another connection wrote to the index.
    #[test]
    fn agent_seq_follows_writes() {
        use crate::core::ids::SessionId;
        use crate::index::scenarios::{World, assistant, owner};
        let w = World::new(&["work"]);
        let line =
            |id: &str, ts: &str| assistant(id, ts, "claude-sonnet-5-5", "/w/shop", (10, 5), &[]);
        w.append(
            "work",
            "s1",
            &[owner("acc-a"), line("m1", "2026-11-01T10:00:00Z")],
        );
        w.write_subagent("work", "s1", "a1", &[line("m2", "2026-11-01T10:01:00Z")]);
        let book = PriceBook::builtin();
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &book);
        let seat = w.id("work");
        let sid = SessionId::new("s1");
        let seq = |idx: &Index, agent: &str| {
            idx.agent_seq(live::AgentKey {
                seat: &seat,
                session_id: &sid,
                agent_id: agent,
            })
        };
        let (main0, sub0) = (seq(&idx, ""), seq(&idx, "a1"));
        w.append("work", "s1", &[line("m3", "2026-11-01T10:02:00Z")]);
        w.refresh(&mut idx, &[], &book);
        assert_ne!(seq(&idx, ""), main0, "the main thread was read further");
        assert_eq!(seq(&idx, "a1"), sub0, "the subagent was not");
        let session0 = idx.session_seq(&seat, &sid);
        w.write_subagent(
            "work",
            "s1",
            "a1",
            &[
                line("m2", "2026-11-01T10:01:00Z"),
                line("m4", "2026-11-01T10:03:00Z"),
            ],
        );
        w.refresh(&mut idx, &[], &book);
        assert_ne!(
            idx.session_seq(&seat, &sid),
            session0,
            "a subagent moves its session"
        );
        // Nothing new: nothing moves.
        let (main1, sub1) = (seq(&idx, ""), seq(&idx, "a1"));
        w.refresh(&mut idx, &[], &book);
        assert_eq!((seq(&idx, ""), seq(&idx, "a1")), (main1, sub1));
        // Another process's refresh that finds nothing new (the same login too) commits
        // nothing: nothing moves.
        let info = [SeatInfo {
            seat: Default::default(),
            label: "work".into(),
            switched_at: None,
            account: Some(crate::index::scenarios::account(
                "acc-a",
                "2026-10-01T00:00:00Z",
            )),
            utilization: None,
        }];
        let mut second = w.index("i.sqlite");
        w.refresh(&mut second, &info, &book);
        w.refresh(&mut idx, &info, &book);
        let (main2, sub2) = (seq(&idx, ""), seq(&idx, "a1"));
        w.refresh(&mut second, &info, &book);
        w.refresh(&mut idx, &info, &book);
        assert_eq!(
            (seq(&idx, ""), seq(&idx, "a1")),
            (main2, sub2),
            "a second reader"
        );
        // Another process commits: everything counts as changed.
        let mut other = w.index("i.sqlite");
        other.set_meta("refreshed_at", "1").unwrap();
        w.refresh(&mut idx, &[], &book);
        assert_ne!(seq(&idx, "a1"), sub2);
    }
}

#[cfg(test)]
mod file_key_tests {
    #[test]
    fn a_64_bit_file_identity_fits_the_signed_column() {
        assert_eq!(super::file_key(u64::MAX), i64::MAX.unsigned_abs());
        assert!(i64::try_from(super::file_key(0xF000_0000_0000_0001)).is_ok());
        assert_eq!(super::file_key(42), 42);
    }
}
