//! What needs a look and what is worth a line in the event log: the ATTENTION items, and the
//! notices for agent and limit changes. Pure functions over the snapshot and the analytics, with
//! the time as an argument, so every rule is tested with fixed numbers; `App` only applies them.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use super::SeatRow;
use crate::core::limits::{self, Level};
use crate::core::models::{context_window, short_name};
#[cfg(test)]
use crate::core::units::{Pct, Usd};
use crate::limits::AccountReport;
use crate::live::{PastKind, Snapshot, State};
use crate::ui::fmt;
use crate::vault::Login;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Good,
    Warn,
    Bad,
    Accent,
}

/// Something that needs a look, in order of urgency.
#[derive(Debug, Clone)]
pub struct Attn {
    pub severity: u8,
    pub glyph: &'static str,
    pub tone: Tone,
    pub text: String,
    pub since: Option<DateTime<Utc>>,
    pub target: AttnTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttnTarget {
    Agent(String),
    Limits,
    None,
}

/// Order of the ATTENTION list: lower comes first.
pub mod severity {
    pub const NEEDS_YOU: u8 = 0;
    /// A limit reached on a seated login, or running out within `RUNS_OUT_SOON_HOURS`.
    pub const LIMIT_NOW: u8 = 1;
    pub const STALLED: u8 = 2;
    /// API errors in the last hour; two agents editing one file.
    pub const ERRORS: u8 = 3;
    pub const LONG_TOOL: u8 = 4;
    /// A context nearly full; a seat whose new login's account is not known yet.
    pub const CONTEXT_OR_UNVERIFIED: u8 = 5;
    /// A limit window nearly full; a saved login about to expire.
    pub const LIMIT_HIGH: u8 = 6;
    pub const UNSAVED_LOGIN: u8 = 7;
    /// A limit reached on a login parked in the vault: nothing runs on it.
    pub const PARKED_LIMIT: u8 = 8;
}

/// A tool running at least this long is worth a look.
const LONG_TOOL_MINUTES: i64 = 10;
/// Share of the context window that counts as nearly full.
const CONTEXT_FULL: f64 = 0.8;
/// A limit running out sooner than this is as urgent as a reached one.
const RUNS_OUT_SOON_HOURS: i64 = 6;
/// A saved login whose refresh token expires sooner than this needs a new sign-in.
const LOGIN_EXPIRES_DAYS: i64 = 7;
/// Limit levels announced in the log when crossed upwards; from `RING_AT` on the bell rings.
const LIMIT_CROSSINGS: [f64; 3] = [80.0, 90.0, 100.0];
const RING_AT: f64 = 90.0;
/// A reset is logged when the window had been used at least this much (smaller ones are noise).
const RESET_WORTH_ANNOUNCING: f64 = 50.0;

/// What ATTENTION looks at.
pub struct Scene<'a> {
    pub snapshot: &'a Snapshot,
    pub limits: &'a [AccountReport],
    pub seats: &'a [SeatRow],
    /// Saved logins (the vault as the LIMITS · SEATS panel shows it).
    pub logins: &'a [Login],
    /// What could not be read (a damaged `.claude.json`, the vault): numbers may be off.
    pub problems: &'a [String],
    /// `--mask`: file paths are blanked.
    pub mask: bool,
    pub now: DateTime<Utc>,
}

/// Everything that needs a look, most urgent first.
pub fn attention(s: &Scene) -> Vec<Attn> {
    let now = s.now;
    let mut out: Vec<Attn> = s
        .problems
        .iter()
        .map(|p| Attn {
            severity: severity::ERRORS,
            glyph: "✖",
            tone: Tone::Bad,
            text: p.clone(),
            since: None,
            target: AttnTarget::Limits,
        })
        .collect();
    for a in &s.snapshot.agents {
        let name = if a.is_main() {
            format!("{} · {}", a.name, a.project)
        } else {
            a.name.clone()
        };
        match &a.state {
            State::NeedsYou(w) => out.push(Attn {
                severity: severity::NEEDS_YOU,
                glyph: "▲",
                tone: Tone::Warn,
                text: format!("{name}: {w}"),
                since: Some(a.since),
                target: AttnTarget::Agent(a.key()),
            }),
            State::Stalled => out.push(Attn {
                severity: severity::STALLED,
                glyph: "✖",
                tone: Tone::Bad,
                text: format!("{name}: stalled"),
                since: Some(a.last_activity),
                target: AttnTarget::Agent(a.key()),
            }),
            State::Tool { name: tool, .. }
                if (now - a.since).num_minutes() >= LONG_TOOL_MINUTES =>
            {
                out.push(Attn {
                    severity: severity::LONG_TOOL,
                    glyph: "◷",
                    tone: Tone::Warn,
                    text: format!("{name}: {tool} running {}", fmt::duration(now - a.since)),
                    since: Some(a.since),
                    target: AttnTarget::Agent(a.key()),
                })
            }
            State::Tool { .. }
            | State::Thinking
            | State::WaitingAgents(_)
            | State::Idle
            | State::Done
            | State::Gone => {}
        }
        if let Some(m) = &a.model {
            let frac = a.context_tokens as f64 / context_window(m) as f64;
            if frac >= CONTEXT_FULL && a.state.is_active() {
                out.push(Attn {
                    severity: severity::CONTEXT_OR_UNVERIFIED,
                    glyph: "◔",
                    tone: Tone::Warn,
                    text: format!("{name}: context {:.0}% full", frac * 100.0),
                    since: None,
                    target: AttnTarget::Agent(a.key()),
                });
            }
        }
    }
    for (kind, n) in &s.snapshot.errors_last_hour {
        out.push(Attn {
            severity: severity::ERRORS,
            glyph: "!",
            tone: Tone::Bad,
            text: format!("API error {kind} ×{n} in the last hour"),
            since: None,
            target: AttnTarget::None,
        });
    }
    limit_items(s, &mut out);
    for seat in s.seats {
        if seat.state.unverified_since.is_some() {
            out.push(Attn {
                severity: severity::CONTEXT_OR_UNVERIFIED,
                glyph: "⚿",
                tone: Tone::Warn,
                text: format!(
                    "seat {}: new login, account not known yet (start Claude Code there once)",
                    seat.label
                ),
                since: seat.state.unverified_since,
                target: AttnTarget::Limits,
            });
        } else if seat.state.fingerprint.is_some() && seat.login.is_none() {
            out.push(Attn {
                severity: severity::UNSAVED_LOGIN,
                glyph: "⚿",
                tone: Tone::Info,
                text: format!(
                    "seat {}: save this login to switch back to it later (c)",
                    seat.label
                ),
                since: None,
                target: AttnTarget::Limits,
            });
        }
    }
    for l in s.logins {
        if let Some(t) = l.meta.refresh_expires_at
            && (t - now).num_days() < LOGIN_EXPIRES_DAYS
        {
            out.push(Attn {
                severity: severity::LIMIT_HIGH,
                glyph: "⚿",
                tone: Tone::Warn,
                text: format!("login {} expires in {}", l.label, fmt::duration(t - now)),
                since: None,
                target: AttnTarget::Limits,
            });
        }
    }
    // Two agents editing the same file within the last half hour.
    for (file, n) in &s.snapshot.conflicts {
        let short: Vec<&str> = file.rsplit('/').take(2).collect();
        let short = if s.mask {
            "•••".to_string()
        } else {
            short.into_iter().rev().collect::<Vec<_>>().join("/")
        };
        out.push(Attn {
            severity: severity::ERRORS,
            glyph: "⚠",
            tone: Tone::Warn,
            text: format!("{n} agents edited {short} in the last 30 min"),
            since: None,
            target: AttnTarget::None,
        });
    }
    out.sort_by_key(|a| a.severity);
    out
}

/// Limit windows: reached, running out soon, or nearly full.
fn limit_items(s: &Scene, out: &mut Vec<Attn>) {
    let now = s.now;
    let seated: Vec<&str> = s
        .seats
        .iter()
        .filter_map(|s| s.account.as_deref())
        .collect();
    for rep in s.limits {
        // A login parked in the vault is not urgent: nothing runs on it.
        let parked = !seated.is_empty() && !seated.contains(&rep.account_uuid.as_str());
        for w in &rep.windows {
            let label = format!("{} {}", rep.name, w.reading.kind.label());
            if w.level() == Level::Reached && !w.reset_passed {
                out.push(Attn {
                    severity: if parked {
                        severity::PARKED_LIMIT
                    } else {
                        severity::LIMIT_NOW
                    },
                    glyph: "⊘",
                    tone: if parked { Tone::Info } else { Tone::Bad },
                    text: format!(
                        "{label}: limit reached · resets in {}{}",
                        fmt::duration(w.window.end - now),
                        if parked {
                            " (saved login, not in a seat)"
                        } else {
                            " · S puts the best login in"
                        }
                    ),
                    since: None,
                    target: AttnTarget::Limits,
                });
            } else if let Some(eta) = w.eta.filter(|d| d.num_hours() < RUNS_OUT_SOON_HOURS) {
                out.push(Attn {
                    severity: severity::LIMIT_NOW,
                    glyph: "⊘",
                    tone: Tone::Warn,
                    text: format!(
                        "{label}: {} used · runs out in {} ({})",
                        fmt::pct(w.estimate_now),
                        fmt::duration(eta),
                        w.runs_out_at
                            .unwrap_or(now + eta)
                            .with_timezone(&chrono::Local)
                            .format("%a %H:%M")
                    ),
                    since: None,
                    target: AttnTarget::Limits,
                });
            } else if w.level() == Level::Bad {
                out.push(Attn {
                    severity: severity::LIMIT_HIGH,
                    glyph: "⊘",
                    tone: Tone::Warn,
                    text: format!("{label}: {} used", fmt::pct(w.estimate_now)),
                    since: None,
                    target: AttnTarget::Limits,
                });
            }
        }
    }
}

/// A line for the event log. `flash` is what to highlight for a moment: an agent key,
/// `limit:<account>` (a limit crossed) or `restored:<account>` (a limit reset).
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub glyph: &'static str,
    pub text: String,
    pub tone: Tone,
    pub flash: Option<String>,
}

impl Notice {
    fn new(glyph: &'static str, text: String, tone: Tone) -> Notice {
        Notice {
            glyph,
            text,
            tone,
            flash: None,
        }
    }

    fn flash(mut self, key: String) -> Notice {
        self.flash = Some(key);
        self
    }
}

/// Notices in the order they happened, and whether the bell should ring.
#[derive(Debug, Default)]
pub struct Events {
    pub notices: Vec<Notice>,
    pub ring: bool,
}

/// Each agent's state and cost at the last snapshot, by key.
pub type Seen = HashMap<String, (State, f64)>;

/// What changed between two snapshots: agents spawned, done, waiting for you, stalled, exited;
/// new API errors. On the first snapshot nothing is announced, only remembered.
pub fn agent_events(seen: &Seen, prev: &Snapshot, next: &Snapshot) -> (Events, Seen) {
    let first = seen.is_empty() && prev.taken_at.is_none();
    let mut ev = Events::default();
    let mut now_seen = HashMap::new();
    for a in &next.agents {
        let key = a.key();
        let name = if a.is_main() {
            format!("{} ({})", a.name, a.project)
        } else {
            a.name.clone()
        };
        if !first {
            match seen.get(&key) {
                None if !a.is_main() && a.state != State::Done => {
                    let model = a.model.as_deref().map(short_name).unwrap_or_default();
                    let effort = a
                        .effort
                        .as_deref()
                        .map(|e| format!("/{e}"))
                        .unwrap_or_default();
                    ev.notices.push(
                        Notice::new(
                            "+",
                            format!("{name} spawned · {model}{effort}"),
                            Tone::Accent,
                        )
                        .flash(key.clone()),
                    );
                }
                None if a.is_main() && a.state != State::Gone => {
                    ev.notices
                        .push(Notice::new("◆", format!("{name} came online"), Tone::Info))
                }
                Some((prev, cost)) if *prev != a.state => match &a.state {
                    State::Done => {
                        let took = fmt::duration(a.last_activity - a.first_seen);
                        ev.notices.push(
                            Notice::new(
                                "✓",
                                format!(
                                    "{name} done · {took} · {} (+{})",
                                    fmt::money(a.cost),
                                    fmt::money((a.cost - cost).max(0.0))
                                ),
                                Tone::Good,
                            )
                            .flash(key.clone()),
                        );
                    }
                    State::NeedsYou(w) => {
                        ev.notices.push(
                            Notice::new("▲", format!("{name} needs you: {w}"), Tone::Warn)
                                .flash(key.clone()),
                        );
                        ev.ring = true;
                    }
                    State::Stalled => {
                        ev.notices.push(Notice::new(
                            "✖",
                            format!("{name} stalled (no activity for a while)"),
                            Tone::Bad,
                        ));
                        ev.ring = true;
                    }
                    State::Gone if a.is_main() => {
                        ev.notices
                            .push(Notice::new("○", format!("{name} exited"), Tone::Info))
                    }
                    State::Gone
                    | State::Tool { .. }
                    | State::Thinking
                    | State::WaitingAgents(_)
                    | State::Idle => {}
                },
                None | Some(_) => {}
            }
        }
        now_seen.insert(key, (a.state.clone(), a.cost));
    }
    for (kind, n) in &next.errors_last_hour {
        let before: u64 = prev
            .errors_last_hour
            .iter()
            .filter(|(k, _)| k == kind)
            .map(|(_, n)| *n)
            .sum();
        if !first && *n > before {
            ev.notices.push(Notice::new(
                "!",
                format!("API error: {kind} ×{}", n - before),
                Tone::Bad,
            ));
        }
    }
    (ev, now_seen)
}

/// Limits crossing 80/90/100% between two analyses (the bell from 90%), and resets of windows
/// that had been at least half used.
pub fn limit_events(prev: &[AccountReport], next: &[AccountReport]) -> Events {
    let mut ev = Events::default();
    for rep in next {
        for w in &rep.windows {
            let before = prev
                .iter()
                .find(|r| r.account_uuid == rep.account_uuid)
                .and_then(|r| r.windows.iter().find(|x| x.reading.kind == w.reading.kind))
                .map(|x| (x.estimate_now.get(), x.window));
            let Some((p, prev_window)) = before else {
                continue;
            };
            for th in LIMIT_CROSSINGS {
                if p < th && w.estimate_now.get() >= th {
                    ev.notices.push(
                        Notice::new(
                            "▲",
                            format!(
                                "LIMIT {} {} crossed {th:.0}%",
                                rep.name,
                                w.reading.kind.label()
                            ),
                            Tone::Bad,
                        )
                        .flash(format!("limit:{}", rep.account_uuid)),
                    );
                    ev.ring |= th >= RING_AT;
                }
            }
            // The reset the analysis itself decided (roll-over or reset by hand), announced
            // when the window had been at least half used.
            if p >= RESET_WORTH_ANNOUNCING && limits::was_reset(prev_window, w.window) {
                ev.notices.push(
                    Notice::new(
                        "◆",
                        format!("LIMIT RESET · {} {}", rep.name, w.reading.kind.label()),
                        Tone::Good,
                    )
                    .flash(format!("restored:{}", rep.account_uuid)),
                );
            }
        }
    }
    ev
}

/// The log line of something that happened before launch.
pub fn past_notice(kind: PastKind) -> Notice {
    match kind {
        PastKind::Spawned {
            name,
            model,
            effort,
        } => {
            let effort = effort.map(|e| format!("/{e}")).unwrap_or_default();
            Notice::new(
                "+",
                format!("{name} spawned · {model}{effort}"),
                Tone::Accent,
            )
        }
        PastKind::Done { name, took, cost } => {
            let took = took.map(fmt::duration).unwrap_or_default();
            Notice::new(
                "✓",
                format!("{name} done · {took} · {}", fmt::money(cost)),
                Tone::Good,
            )
        }
        PastKind::ApiError { kind, text } => {
            let text = text.map(|t| format!(" · {t}")).unwrap_or_default();
            Notice::new("!", format!("API error: {kind}{text}"), Tone::Bad)
        }
        PastKind::Prompt { title, chars } => {
            let to = if title.is_empty() {
                String::new()
            } else {
                format!(" → {title}")
            };
            Notice::new("✎", format!("you{to} · {chars} chars"), Tone::Info)
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::core::limits::{Reading, WindowKind};
    use crate::core::period::Span;
    use crate::limits::WindowReport;
    use crate::live::Agent;
    use crate::vault::SeatState;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 4, 12, 0, 0).unwrap()
    }

    fn agent(session: &str, id: &str, state: State) -> Agent {
        Agent {
            seat: "work".into(),
            seat_label: "work".into(),
            session_id: session.into(),
            agent_id: id.into(),
            parent: None,
            depth: u32::from(!id.is_empty()),
            name: if id.is_empty() { "main" } else { id }.into(),
            agent_type: None,
            project: "shop".into(),
            model: Some("claude-opus-5-5".into()),
            effort: None,
            state,
            since: now() - Duration::minutes(1),
            last_activity: now() - Duration::minutes(1),
            cost: 1.0,
            requests: 3,
            context_tokens: 1_000,
            spark: Vec::new(),
            lane: Vec::new(),
            pane: None,
            pid: None,
            account: None,
            first_seen: now() - Duration::minutes(30),
            session_cost: 0.0,
            session_burn: 0.0,
            cpu: 0.0,
            memory: 0,
            last_request_ts: None,
        }
    }

    /// A weekly window of `account` at `used` percent, resetting in two days.
    fn report(account: &str, used: f64, eta_hours: Option<i64>) -> AccountReport {
        let resets_at = now() + Duration::days(2);
        let reading = Reading {
            kind: WindowKind::SevenDay,
            utilization: used,
            resets_at,
            fetched_at: now() - Duration::minutes(5),
        };
        AccountReport {
            account_uuid: account.into(),
            name: format!("{account}-login"),
            tier: None,
            windows: vec![WindowReport {
                reading,
                reset_passed: false,
                window: reading.window(),
                manual_reset: None,
                own_usd_per_point: None,
                usd_per_point: None,
                reference: None,
                cost_in_window: Usd::ZERO,
                estimate_now: Pct::new(used),
                visible_points: None,
                outside_points: None,
                burn_usd_per_hour: Usd::ZERO,
                points_per_hour: None,
                eta: eta_hours.map(Duration::hours),
                runs_out_at: eta_hours.map(|h| now() + Duration::hours(h)),
                eta_seconds: eta_hours.map(|h| h * 3600),
                shares: Vec::new(),
                window_usd: None,
                curve: Vec::new(),
                curve_step: Duration::hours(1),
                marks: Vec::new(),
            }],
        }
    }

    fn seat(label: &str, account: Option<&str>, login: Option<&str>) -> SeatRow {
        SeatRow {
            id: label.into(),
            label: label.into(),
            account: account.map(String::from),
            login: login.map(String::from),
            live: 0,
            state: SeatState {
                fingerprint: Some("f1".into()),
                ..Default::default()
            },
        }
    }

    fn scene<'a>(
        snap: &'a Snapshot,
        limits: &'a [AccountReport],
        seats: &'a [SeatRow],
        mask: bool,
    ) -> Scene<'a> {
        Scene {
            snapshot: snap,
            limits,
            seats,
            logins: &[],
            problems: &[],
            mask,
            now: now(),
        }
    }

    /// First review H5: a file that could not be read is said in ATTENTION, not shown as if
    /// the seat had no account.
    #[test]
    fn what_could_not_be_read_is_attention() {
        let snap = Snapshot::default();
        let problems = ["seat work: /x/.claude.json is damaged".to_string()];
        let items = attention(&Scene {
            problems: &problems,
            ..scene(&snap, &[], &[], false)
        });
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, severity::ERRORS);
        assert!(items[0].text.contains("damaged"));
    }

    #[test]
    fn a_parked_logins_reached_limit_is_low_and_a_seated_one_is_high() {
        let snap = Snapshot::default();
        let limits = [report("seated", 100.0, None), report("parked", 100.0, None)];
        let seats = [seat("work", Some("seated"), Some("w"))];
        let items = attention(&scene(&snap, &limits, &seats, false));
        assert_eq!(items.len(), 2);
        // Most urgent first: the seated account, then the parked one at the end.
        assert_eq!(items[0].severity, severity::LIMIT_NOW);
        assert_eq!(items[0].tone, Tone::Bad);
        assert_eq!(
            items[0].text,
            "seated-login weekly: limit reached · resets in 2d 0h · S puts the best login in"
        );
        assert_eq!(items[1].severity, severity::PARKED_LIMIT);
        assert_eq!(items[1].tone, Tone::Info);
        assert!(items[1].text.ends_with("(saved login, not in a seat)"));
        assert!(items.iter().all(|a| a.target == AttnTarget::Limits));
    }

    #[test]
    fn with_no_seat_known_every_reached_limit_is_urgent() {
        let snap = Snapshot::default();
        let limits = [report("a", 100.0, None)];
        let items = attention(&scene(&snap, &limits, &[], false));
        assert_eq!(items[0].severity, severity::LIMIT_NOW);
    }

    #[test]
    fn a_limit_running_out_within_six_hours_is_as_urgent_as_a_reached_one() {
        let snap = Snapshot::default();
        let seats = [seat("work", Some("a"), Some("w"))];
        // 5 h left: urgent; 6 h left: only "nearly full" (≥ 85%); 70% with 30 h: nothing.
        let soon = [report("a", 90.0, Some(5))];
        let items = attention(&scene(&snap, &soon, &seats, false));
        assert_eq!(
            (items[0].severity, items[0].tone),
            (severity::LIMIT_NOW, Tone::Warn)
        );
        assert!(
            items[0]
                .text
                .starts_with("a-login weekly: 90% used · runs out in 5h")
        );
        let later = [report("a", 90.0, Some(6))];
        let items = attention(&scene(&snap, &later, &seats, false));
        assert_eq!(items[0].severity, severity::LIMIT_HIGH);
        assert_eq!(items[0].text, "a-login weekly: 90% used");
        let calm = [report("a", 70.0, Some(30))];
        assert!(attention(&scene(&snap, &calm, &seats, false)).is_empty());
    }

    #[test]
    fn conflicts_produce_an_item_with_the_last_two_path_parts() {
        let snap = Snapshot {
            conflicts: vec![("/src/shop/cart/pricing.rs".into(), 3)],
            ..Default::default()
        };
        let items = attention(&scene(&snap, &[], &[], false));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, severity::ERRORS);
        assert_eq!(
            items[0].text,
            "3 agents edited cart/pricing.rs in the last 30 min"
        );
        assert_eq!(items[0].target, AttnTarget::None);
        let masked = attention(&scene(&snap, &[], &[], true));
        assert_eq!(masked[0].text, "3 agents edited ••• in the last 30 min");
    }

    #[test]
    fn agents_needing_you_come_first_and_long_tools_and_full_contexts_follow() {
        let mut long = agent(
            "s1",
            "a1",
            State::Tool {
                name: "Bash".into(),
                summary: String::new(),
            },
        );
        long.since = now() - Duration::minutes(12);
        let mut full = agent("s1", "a2", State::Thinking);
        // Opus 5.5: a 1M-token window; 850k tokens is 85% full.
        full.context_tokens = 850_000;
        let snap = Snapshot {
            agents: vec![
                agent("s1", "", State::Idle),
                long,
                full,
                agent("s2", "", State::NeedsYou("permission".into())),
                agent("s2", "a3", State::Stalled),
            ],
            errors_last_hour: vec![("overloaded".into(), 2)],
            ..Default::default()
        };
        let items = attention(&scene(&snap, &[], &[], false));
        let got: Vec<(u8, &str)> = items
            .iter()
            .map(|a| (a.severity, a.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (severity::NEEDS_YOU, "main · shop: permission"),
                (severity::STALLED, "a3: stalled"),
                (severity::ERRORS, "API error overloaded ×2 in the last hour"),
                (severity::LONG_TOOL, "a1: Bash running 12m"),
                (severity::CONTEXT_OR_UNVERIFIED, "a2: context 85% full"),
            ]
        );
        assert_eq!(items[0].target, AttnTarget::Agent("work/s2/".into()));
    }

    #[test]
    fn seats_without_a_saved_login_and_expiring_logins_are_listed() {
        let snap = Snapshot::default();
        let mut unverified = seat("new", None, None);
        unverified.state.unverified_since = Some(now() - Duration::hours(1));
        let seats = [seat("work", Some("a"), None), unverified];
        let login: Login = serde_json::from_value(serde_json::json!({
            "id": "l1", "label": "spare", "account_uuid": null, "email": null, "plan": null,
            "meta": {"fingerprint": "f2", "subscription_type": null, "rate_limit_tier": null,
                     "access_expires_at": null,
                     "refresh_expires_at": (now() + Duration::days(3)).to_rfc3339(), "scopes": []},
            "oauth_account": null, "captured_at": now().to_rfc3339(),
            "updated_at": now().to_rfc3339(), "mounted": null
        }))
        .unwrap();
        let logins = [login];
        let items = attention(&Scene {
            logins: &logins,
            ..scene(&snap, &[], &seats, false)
        });
        let got: Vec<(u8, &str)> = items
            .iter()
            .map(|a| (a.severity, a.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    severity::CONTEXT_OR_UNVERIFIED,
                    "seat new: new login, account not known yet (start Claude Code there once)"
                ),
                (severity::LIMIT_HIGH, "login spare expires in 3d 0h"),
                (
                    severity::UNSAVED_LOGIN,
                    "seat work: save this login to switch back to it later (c)"
                ),
            ]
        );
    }

    #[test]
    fn the_first_snapshot_is_only_remembered() {
        let snap = Snapshot {
            taken_at: Some(now()),
            agents: vec![agent("s1", "a1", State::Thinking)],
            errors_last_hour: vec![("overloaded".into(), 1)],
            ..Default::default()
        };
        let (ev, seen) = agent_events(&Seen::new(), &Snapshot::default(), &snap);
        assert!(ev.notices.is_empty() && !ev.ring);
        assert_eq!(seen["work/s1/a1"], (State::Thinking, 1.0));
    }

    #[test]
    fn agent_changes_are_announced_and_the_bell_rings_for_you_and_stalls() {
        let mut done = agent("s1", "a1", State::Done);
        done.cost = 2.5;
        let prev = Snapshot {
            taken_at: Some(now() - Duration::seconds(1)),
            errors_last_hour: vec![("overloaded".into(), 1)],
            ..Default::default()
        };
        let seen: Seen = [
            ("work/s1/a1".to_string(), (State::Thinking, 1.0)),
            ("work/s1/".to_string(), (State::Idle, 0.0)),
        ]
        .into();
        let next = Snapshot {
            taken_at: Some(now()),
            agents: vec![
                agent("s1", "", State::NeedsYou("a question".into())),
                done,
                agent("s1", "a2", State::Thinking),
            ],
            errors_last_hour: vec![("overloaded".into(), 3)],
            ..Default::default()
        };
        let (ev, _) = agent_events(&seen, &prev, &next);
        let got: Vec<(&str, &str, Option<&str>)> = ev
            .notices
            .iter()
            .map(|n| (n.glyph, n.text.as_str(), n.flash.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("▲", "main (shop) needs you: a question", Some("work/s1/")),
                // 29 min from first seen to last activity; $2.50 now, $1.00 seen before.
                ("✓", "a1 done · 29m · $2.50 (+$1.50)", Some("work/s1/a1")),
                ("+", "a2 spawned · opus-5.5", Some("work/s1/a2")),
                ("!", "API error: overloaded ×2", None),
            ]
        );
        assert!(ev.ring);
    }

    #[test]
    fn limits_crossing_levels_are_logged_and_ring_from_ninety() {
        let ev = limit_events(&[report("a", 75.0, None)], &[report("a", 85.0, None)]);
        let texts: Vec<&str> = ev.notices.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(texts, vec!["LIMIT a-login weekly crossed 80%"]);
        assert_eq!(ev.notices[0].flash.as_deref(), Some("limit:a"));
        assert!(!ev.ring);
        let ev = limit_events(&[report("a", 85.0, None)], &[report("a", 100.0, None)]);
        let texts: Vec<&str> = ev.notices.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "LIMIT a-login weekly crossed 90%",
                "LIMIT a-login weekly crossed 100%"
            ]
        );
        assert!(ev.ring);
        // An account seen for the first time is not a crossing.
        assert!(
            limit_events(&[], &[report("a", 100.0, None)])
                .notices
                .is_empty()
        );
    }

    #[test]
    fn a_reset_is_logged_when_the_window_had_been_at_least_half_used() {
        let rolled = |used_before: f64| {
            let before = report("a", used_before, None);
            let mut after = report("a", 3.0, None);
            let w = &mut after.windows[0];
            w.window = Span {
                start: w.window.start + Duration::days(7),
                end: w.window.end + Duration::days(7),
            };
            limit_events(&[before], &[after])
        };
        let ev = rolled(60.0);
        assert_eq!(ev.notices.len(), 1);
        assert_eq!(ev.notices[0].text, "LIMIT RESET · a-login weekly");
        assert_eq!(ev.notices[0].flash.as_deref(), Some("restored:a"));
        assert!(rolled(40.0).notices.is_empty());
    }
}
