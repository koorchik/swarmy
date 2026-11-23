//! The worker thread: owns the index, refreshes it every tick, builds the live snapshot and, on
//! the cadences of `jobs::JOBS`, the heavier analytics. It sends everything to the UI thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Utc;

use super::analytics::{self, SlowParts};
use super::jobs::{
    COMPACT_CATCH_UP, COMPACT_CATCH_UP_ECO, COMPACT_CHUNKS_PER_RUN, Job, Schedule, TICK_STEP,
};
use super::{Msg, SystemView, Wants, debug, lock};
use crate::app::context::Ctx;
use crate::claude::procs::ProcTable;
use crate::claude::registry::{self, LiveSession};
use crate::core::period::Period;
use crate::index::compact::{self, CompactOpts};
use crate::index::query::{Dim, Filter};
use crate::index::{Index, Progress};
use crate::limits;
use crate::live;

pub(super) fn worker(
    mut ctx: Ctx,
    tx: Sender<Msg>,
    wants: Arc<Mutex<Wants>>,
    eco: Arc<AtomicBool>,
    force_eco: bool,
) {
    let send = |m: Msg| tx.send(m).is_ok();
    let mut idx = match Index::open(&ctx.dirs.index_file()) {
        Ok(i) => i,
        Err(e) => {
            send(Msg::Error(format!("{e:#}")));
            return;
        }
    };
    let mut resolver = crate::claude::project::Resolver::new(ctx.config.project.clone());
    let mut progress = |p: Progress| {
        let _ = tx.send(Msg::Progress(p));
    };
    match idx.refresh(
        &ctx.seats,
        &ctx.seat_info,
        &mut resolver,
        &ctx.prices,
        &mut progress,
    ) {
        Ok(s) => {
            let accounts = ctx
                .seat_info
                .iter()
                .filter_map(|s| s.account.as_ref().map(|a| a.account_uuid.clone()))
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            if !send(Msg::Ready {
                seats: ctx.seats.len(),
                accounts,
                files: s.files_total,
            }) {
                return;
            }
        }
        Err(e) => {
            send(Msg::Error(format!("{e:#}")));
            return;
        }
    }
    let mut sys = sysinfo::System::new().ok();
    let opts = live::Options {
        stall_minutes: ctx.config.stall_minutes() as i64,
        ..Default::default()
    };
    let mut jobs = Schedule::new(eco.load(Ordering::Relaxed));
    if debug() {
        super::jobs::print_table();
    }
    let mut disk_cache: Vec<(String, u64)> = Vec::new();
    let mut storage_cache: Option<crate::core::storage::Outlook> = None;
    let mut login_stamp = login_files_stamp(&ctx);
    let mut seats_changed = false;
    let mut sl_stamp = statusline_stamp(&ctx);
    let mut past_sent = false;
    let mut prev_snap: Option<live::Snapshot> = None;
    let mut reads = live::Reads::default();
    // The selected agent's timeline as last sent, by (agent key, `Index::agent_seq`).
    let mut timeline_sent: Option<(String, u64)> = None;
    let mut prev_analytics: Option<super::Analytics> = None;
    let mut fleet_last: Option<crate::fleet::Fleet> = None;
    let mut slow: Option<SlowParts> = None;
    loop {
        let tick_start = Instant::now();
        let mut marks: Vec<(&str, Instant)> = Vec::new();
        ctx.now = Utc::now();
        let tp = Instant::now();
        let want_system = lock(&wants).system;
        if let Some(s) = sys.as_mut() {
            if want_system && jobs.due(Job::SystemProcs) {
                ctx.procs = ProcTable::snapshot_with_cpu(s);
                jobs.done(Job::SystemProcs);
            } else if !want_system || jobs.due(Job::SystemProcs) {
                let pids: Vec<u32> = ctx
                    .seats
                    .iter()
                    .flat_map(|st| registry::session_pids(&st.sessions_dir()))
                    .collect();
                ctx.procs = ProcTable::for_pids(s, &pids);
            }
        }
        if debug() {
            eprintln!("procs {:?} ({} nodes)", tp.elapsed(), ctx.procs.nodes.len());
        }
        marks.push(("procs", Instant::now()));
        // A fresh status-line reading: limits are exact again, show them.
        let sl = statusline_stamp(&ctx);
        if sl != sl_stamp && jobs.due(Job::StatusLine) {
            sl_stamp = sl;
            jobs.done(Job::StatusLine);
            if let Err(e) = ctx.reload_seat_info() {
                send(Msg::Error(format!("{e:#}")));
            }
            jobs.done(Job::SeatInfo);
            seats_changed = true;
        }
        // A login file changed (a refresh, a /login, a copied backup): re-read at once.
        let stamp = login_files_stamp(&ctx);
        if stamp != login_stamp || jobs.due(Job::SeatInfo) {
            if stamp != login_stamp {
                // A login changed: refresh seats and limits now, not the period aggregates.
                seats_changed = true;
                login_stamp = stamp;
            }
            if let Err(e) = ctx.reload_seat_info() {
                send(Msg::Error(format!("{e:#}")));
            }
            jobs.done(Job::SeatInfo);
        }
        marks.push(("seatinfo", Instant::now()));
        let dbg = debug();
        let t0 = Instant::now();
        let live_sessions: Vec<LiveSession> = ctx
            .seats
            .iter()
            .flat_map(|s| registry::live_sessions(&s.id, &s.sessions_dir(), &ctx.procs))
            .collect();
        let full_refresh = jobs.due(Job::FullRefresh);
        let res = if full_refresh {
            jobs.done(Job::FullRefresh);
            let mut quiet = |_p: Progress| {};
            idx.refresh(
                &ctx.seats,
                &ctx.seat_info,
                &mut resolver,
                &ctx.prices,
                &mut quiet,
            )
        } else {
            let keys: Vec<(crate::core::ids::SeatId, String)> = live_sessions
                .iter()
                .map(|l| (l.seat.clone(), l.session_id.to_string()))
                .collect();
            idx.refresh_hot(
                &ctx.seats,
                &keys,
                &ctx.seat_info,
                &mut resolver,
                &ctx.prices,
            )
        };
        let refreshed = res.is_ok();
        if let Err(e) = res {
            send(Msg::Error(format!("{e:#}")));
        }
        // Right after a full refresh: prices and accounts are settled then.
        if full_refresh && refreshed && jobs.due(Job::Compact) {
            match compact(&ctx, &mut idx, jobs.eco) {
                Ok(true) => jobs.again_in(
                    Job::Compact,
                    if jobs.eco {
                        COMPACT_CATCH_UP_ECO
                    } else {
                        COMPACT_CATCH_UP
                    },
                ),
                Ok(false) => jobs.done(Job::Compact),
                Err(e) => {
                    jobs.done(Job::Compact);
                    if debug() {
                        eprintln!("compaction failed: {e:#}");
                    }
                    send(Msg::Error(format!("compaction: {e:#}")));
                }
            }
        }
        marks.push(("sessions+refresh", Instant::now()));
        let t1 = Instant::now();
        let heavy = jobs.due(Job::HeavyParts);
        let built = live::build(
            &idx,
            &live_sessions,
            &ctx.procs,
            &opts,
            ctx.now,
            if heavy { None } else { prev_snap.as_ref() },
            &mut reads,
        );
        if heavy {
            jobs.done(Job::HeavyParts);
        }
        if let Ok(s) = &built {
            prev_snap = Some(s.clone());
        }
        if dbg {
            eprintln!("refresh {:?} build {:?}", t1 - t0, t1.elapsed());
        }
        match built {
            Ok(snap) => {
                // The status line's summary: when something changed (at most every 2 s), and
                // at least every 5 s so it stays fresh.
                if jobs.due(Job::Fleet) {
                    let fl = fleet_of(
                        &ctx,
                        &snap,
                        prev_analytics
                            .as_ref()
                            .map(|a| a.limits.as_slice())
                            .unwrap_or(&[]),
                    );
                    let changed = Some(&fl) != fleet_last.as_ref();
                    if changed || jobs.due(Job::FleetKeepAlive) {
                        let mut f = fl.clone();
                        f.written_at = Utc::now();
                        // The status lines fall back to scanning the registries without it.
                        if let Err(e) = crate::fleet::write(&crate::fleet::path(&ctx.dirs.data), &f)
                        {
                            crate::diag::log("fleet", &format!("cannot write the summary: {e}"));
                        }
                        fleet_last = Some(fl);
                        jobs.done(Job::Fleet);
                        jobs.done(Job::FleetKeepAlive);
                    }
                }
                // Only the SYSTEM panel shows this; listing disks is not free.
                let sysview = if want_system {
                    system_view(&ctx, &live_sessions, sys.as_ref())
                } else {
                    SystemView::default()
                };
                if !past_sent {
                    past_sent = true;
                    if let Ok(p) =
                        live::past_events(&idx, ctx.now - chrono::Duration::hours(3), 200)
                    {
                        let _ = send(Msg::Past(p));
                    }
                }
                if !send(Msg::Snapshot(
                    Box::new(snap),
                    live_sessions.clone(),
                    Box::new(sysview),
                )) {
                    return;
                }
            }
            Err(e) => {
                send(Msg::Error(format!("{e:#}")));
            }
        }
        marks.push(("build+send", Instant::now()));
        let w = lock(&wants).clone();
        let tt = Instant::now();
        // Read again only when the agent changed or the selection moved: the UI keeps the last.
        let sent = w
            .agent
            .as_ref()
            .map(|a| (a.key(), idx.agent_seq(live::key_of(a))));
        if let Some(a) = &w.agent
            && sent != timeline_sent
            && let Ok(t) = live::tool_timeline(&idx, a, 200)
        {
            if !send(Msg::Timeline(
                a.key(),
                t,
                live::last_message(&idx, a),
                live::life(&idx, a).unwrap_or_default(),
            )) {
                return;
            }
            timeline_sent = sent;
        }
        marks.push(("timeline", Instant::now()));
        if dbg {
            eprintln!("timeline {:?}", tt.elapsed());
        }
        let seats_now = std::mem::take(&mut seats_changed);
        if jobs.due(Job::Analytics) || w.refresh_analytics || seats_now {
            if jobs.due(Job::DiskSizes) {
                disk_cache = ctx
                    .seats
                    .iter()
                    .map(|s| (s.label.clone(), dir_size(&s.projects_dir())))
                    .collect();
                match outlook(&ctx, &idx) {
                    Ok(o) => storage_cache = Some(o),
                    Err(e) => {
                        send(Msg::Error(format!("{e:#}")));
                    }
                }
                jobs.done(Job::DiskSizes);
            }
            if lock(&wants).refresh_analytics {
                // A login may have just changed: re-read the seats' accounts.
                if let Err(e) = ctx.reload_seat_info() {
                    send(Msg::Error(format!("{e:#}")));
                }
            }
            lock(&wants).refresh_analytics = false;
            let ta = Instant::now();
            if jobs.due(Job::DaysHeat)
                && let Some(parts) = days_and_heat(&ctx, &idx)
            {
                slow = Some(parts);
                jobs.done(Job::DaysHeat);
            }
            let full = w.refresh_analytics || jobs.due(Job::AnalyticsFull);
            let limits_due = full || seats_now || jobs.due(Job::Limits);
            let res = analytics::analytics(analytics::Inputs {
                ctx: &ctx,
                idx: &idx,
                wants: &w,
                jobs: &jobs,
                disk: &disk_cache,
                storage: storage_cache,
                slow: slow.clone(),
                reuse_period: if full { None } else { prev_analytics.as_ref() },
                reuse_limits: if limits_due {
                    None
                } else {
                    prev_analytics.as_ref().map(|p| p.limits.as_slice())
                },
                last: prev_analytics.as_ref(),
            });
            if limits_due && res.is_ok() {
                jobs.done(Job::Limits);
            }
            if full && res.is_ok() {
                jobs.done(Job::AnalyticsFull);
            }
            if dbg {
                eprintln!("analytics {:?}", ta.elapsed());
            }
            match res {
                Ok(a) => {
                    prev_analytics = Some(a.clone());
                    if !send(Msg::Analytics(Box::new(a))) {
                        return;
                    }
                }
                Err(e) => {
                    send(Msg::Error(format!("{e:#}")));
                }
            }
            jobs.done(Job::Analytics);
        }
        if jobs.due(Job::Power) {
            eco.store(force_eco || crate::power::on_battery(), Ordering::Relaxed);
            jobs.done(Job::Power);
        }
        if debug() {
            let mut prev = tick_start;
            let parts: Vec<String> = marks
                .iter()
                .map(|(n, t)| {
                    let d = *t - prev;
                    prev = *t;
                    format!("{n}={:.1}", d.as_secs_f64() * 1000.0)
                })
                .collect();
            eprintln!(
                "tick {:.1}ms {}",
                tick_start.elapsed().as_secs_f64() * 1000.0,
                parts.join(" ")
            );
        }
        // One tick (longer on battery), but answer UI requests sooner.
        jobs.eco = eco.load(Ordering::Relaxed);
        for _ in 0..jobs.tick_steps() {
            std::thread::sleep(TICK_STEP);
            if lock(&wants).refresh_analytics {
                break;
            }
        }
    }
}

/// The compaction job (`jobs::Job::Compact`): at most a day of chunks, then, once nothing is
/// left to roll up, the one-time switch to incremental auto-vacuum when it is due (never on
/// battery). Returns whether a backlog is left for another run soon.
fn compact(ctx: &Ctx, idx: &mut Index, on_battery: bool) -> anyhow::Result<bool> {
    if !ctx.config.storage.auto_compact {
        // `[storage] auto_compact = false`: only the day's size sample (growth, projection).
        idx.sample_storage(ctx.now.with_timezone(&chrono::Local).date_naive())?;
        return Ok(false);
    }
    let t = Instant::now();
    let r = idx.compact(&CompactOpts {
        keep_raw_days: ctx.keep_raw_days(None)?,
        now: ctx.now,
        prices: &ctx.prices,
        seats: &ctx.seats,
        max_chunks: Some(COMPACT_CHUNKS_PER_RUN),
    })?;
    let backlog = !r.done && r.waiting.is_none();
    if !backlog && r.rolled_until > 0 {
        let vacuum =
            idx.vacuum_to_incremental(on_battery, compact::free_disk(&ctx.dirs.data), false)?;
        if debug() {
            eprintln!("compaction: one-time VACUUM {vacuum:?}");
        }
    }
    if debug() {
        eprintln!(
            "compaction {:?}: {} chunks, {} requests, {} tool calls, freed {} B, backlog {backlog}",
            t.elapsed(),
            r.chunks,
            r.requests,
            r.tool_calls,
            r.freed_bytes
        );
    }
    Ok(backlog)
}

/// The index's size, growth and 12-month projection for SYSTEM.
fn outlook(ctx: &Ctx, idx: &Index) -> anyhow::Result<crate::core::storage::Outlook> {
    Ok(idx
        .storage_report()?
        .outlook(ctx.now, ctx.keep_raw_days(None)?))
}

/// Day bars (14 days), the weekday × hour heat map (28 days) and day × model spend.
fn days_and_heat(ctx: &Ctx, idx: &Index) -> Option<SlowParts> {
    let f = Filter::default();
    // Rolling periods: they start on a quarter hour (`Period::span`).
    let days_span = Period::Days(14).span(ctx.now, None);
    let heat_span = Period::Days(28).span(ctx.now, None);
    let (Ok(d), Ok(h), Ok(dm)) = (
        idx.spend_by(Dim::Day, days_span, &f),
        idx.matrix(Dim::Weekday, Dim::Hour, heat_span, &f),
        idx.days_by_model(days_span),
    ) else {
        return None;
    };
    Some((d, h, dm))
}

/// Modification times of every seat's credentials file (`.claude.json` changes every few seconds
/// and is re-read on a timer instead).
fn login_files_stamp(ctx: &Ctx) -> Vec<Option<std::time::SystemTime>> {
    ctx.seats
        .iter()
        .map(|s| s.credentials_file())
        .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect()
}

/// The fleet summary for the status line (`fleet.rs`); `written_at` is set by the caller.
fn fleet_of(
    ctx: &Ctx,
    snap: &live::Snapshot,
    limits: &[limits::AccountReport],
) -> crate::fleet::Fleet {
    use crate::fleet::{Fleet, NeedsYou};
    let mut f = Fleet {
        v: crate::fleet::VERSION,
        writer_pid: std::process::id(),
        ..Default::default()
    };
    for a in &snap.agents {
        let main = snap
            .agents
            .iter()
            .find(|m| m.is_main() && m.seat == a.seat && m.session_id == a.session_id);
        if a.is_main() {
            if a.state != live::State::Gone {
                f.counts.sessions += 1;
            }
            f.sessions
                .entry(a.session_id.clone())
                .or_default()
                .burn_usd_h = a.session_burn;
        } else if a.state.is_active()
            || matches!(
                a.state,
                live::State::NeedsYou(_) | live::State::Stalled | live::State::WaitingAgents(_)
            )
        {
            f.counts.agents += 1;
            f.sessions.entry(a.session_id.clone()).or_default().agents += 1;
        }
        let sc = f.sessions.entry(a.session_id.clone()).or_default();
        let (w, t, wt, st) = match &a.state {
            live::State::Tool { .. } => (1, 0, 0, 0),
            live::State::Thinking => (0, 1, 0, 0),
            live::State::WaitingAgents(_) => (0, 0, 1, 0),
            live::State::Stalled => (0, 0, 0, 1),
            _ => (0, 0, 0, 0),
        };
        if !a.is_main() {
            sc.working += w;
            sc.thinking += t;
            sc.waiting += wt;
            sc.stalled += st;
        }
        f.counts.working += w;
        f.counts.thinking += t;
        f.counts.waiting += wt;
        f.counts.stalled += st;
        if let live::State::NeedsYou(what) = &a.state {
            let name = main
                .map(|m| m.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| a.project.clone());
            f.needs_you.push(NeedsYou {
                session_id: a.session_id.clone(),
                seat: a.seat.clone(),
                name,
                what: what.clone(),
                since: Some(a.since),
                pane: main.and_then(|m| m.pane.clone()).or_else(|| a.pane.clone()),
            });
        }
    }
    f.needs_you.sort_by_key(|n| n.since);
    for (s, si) in ctx.seats.iter().zip(&ctx.seat_info) {
        let Some(acct) = si.account.as_ref().map(|a| a.account_uuid.clone()) else {
            continue;
        };
        let week = limits
            .iter()
            .find(|r| r.account_uuid == acct)
            .and_then(|r| {
                r.windows.iter().find(|w| {
                    w.reading.kind == crate::core::limits::WindowKind::SevenDay && !w.reset_passed
                })
            });
        f.seats.insert(
            s.id.clone(),
            seat_limit(&s.label, ctx.account_name(&acct), week),
        );
    }
    f
}

/// A seat's week in the fleet summary, from the last limits pass only: nothing here moves with
/// the clock, so an unchanged pass gives an equal summary and `fleet.json` is written only on
/// the keep-alive (review L2: `now + eta` each tick drifted and rewrote it every second).
fn seat_limit(
    label: &str,
    account_name: String,
    week: Option<&limits::WindowReport>,
) -> crate::fleet::SeatLimit {
    crate::fleet::SeatLimit {
        label: label.to_string(),
        account_name,
        week_used: week.map(|w| w.estimate_now.get()),
        runs_out_at: week.and_then(|w| w.runs_out_at),
        resets_at: week.map(|w| w.reading.resets_at),
    }
}

/// Modification times of the seats' status-line readings (one `stat` per seat).
fn statusline_stamp(ctx: &Ctx) -> Vec<Option<std::time::SystemTime>> {
    ctx.seats
        .iter()
        .map(|s| crate::statusline::reading::file_of(&ctx.dirs.data, &s.id))
        .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect()
}

fn system_view(ctx: &Ctx, live: &[LiveSession], sys: Option<&sysinfo::System>) -> SystemView {
    let mut v = SystemView::default();
    for l in live {
        let (cpu, mem) = ctx.procs.tree_usage(l.pid);
        let mut kids: Vec<(String, f32, u64)> = ctx
            .procs
            .descendants(l.pid)
            .into_iter()
            .map(|n| {
                let cmd = if n.cmd.is_empty() {
                    n.name.clone()
                } else {
                    n.cmd.clone()
                };
                (cmd, n.cpu, n.memory)
            })
            .collect();
        kids.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.cmp(&a.2)));
        kids.truncate(6);
        let seat = ctx
            .seats
            .iter()
            .find(|s| s.id == l.seat)
            .map_or_else(|| l.seat.to_string(), |s| s.label.clone());
        v.instances.push((
            seat,
            l.name
                .clone()
                .unwrap_or_else(|| l.session_id.short().to_string()),
            l.pid,
            cpu,
            mem,
            kids,
        ));
    }
    if let Some(s) = sys {
        v.mem_total = s.total_memory();
        v.mem_available = s.available_memory();
    }
    v.disk_free = compact::free_disk(&ctx.dirs.home);
    v
}

fn dir_size(p: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::limits::{Reading, WindowKind};
    use crate::core::units::{Pct, Usd};

    /// Review L2: the fleet's week comes from the last limits pass only. Two ticks a few
    /// seconds apart with the same pass give an equal summary (so `fleet.json` waits for the
    /// keep-alive), and the run-out time is the instant estimated then, not `now + eta` again.
    #[test]
    fn the_fleet_week_does_not_move_with_the_clock() {
        let pass = "2026-11-22T10:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        let reading = Reading {
            kind: WindowKind::SevenDay,
            utilization: 70.0,
            resets_at: pass + chrono::Duration::days(2),
            fetched_at: pass,
        };
        let week = limits::WindowReport {
            reading,
            reset_passed: false,
            window: reading.window(),
            manual_reset: None,
            own_usd_per_point: None,
            usd_per_point: None,
            reference: None,
            cost_in_window: Usd::ZERO,
            estimate_now: Pct::new(70.0),
            visible_points: None,
            outside_points: None,
            burn_usd_per_hour: Usd::ZERO,
            points_per_hour: None,
            eta: Some(chrono::Duration::hours(7)),
            runs_out_at: Some(pass + chrono::Duration::hours(7)),
            eta_seconds: Some(7 * 3600),
            shares: Vec::new(),
            window_usd: None,
            curve: Vec::new(),
            curve_step: chrono::Duration::hours(1),
            marks: Vec::new(),
        };
        let tick = || seat_limit("work", "alice".into(), Some(&week));
        let first = tick();
        assert_eq!(first.runs_out_at, Some(pass + chrono::Duration::hours(7)));
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(tick(), first);
        assert_eq!(first.week_used, Some(70.0));
    }
}
