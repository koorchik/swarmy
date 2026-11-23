//! Drawing the status line: segments in their forms from full to shortest, shortened in a fixed
//! order to the width Claude Code gives. Pure: everything comes in through `Input` and `Extra`.

use chrono::{DateTime, Utc};

use super::input::{Input, Window};
use crate::core::ids::SeatId;
use crate::core::limits::Level;

/// One piece of the line: its forms from full to shortest (text with colour, plain width).
struct Segment {
    key: &'static str,
    forms: Vec<(String, usize)>,
    form: usize,
    dropped: bool,
}

/// Everything the line is made from besides stdin.
pub struct Extra {
    pub fleet: Option<crate::fleet::Fleet>,
    /// Sessions waiting for the user when there is no fresh fleet summary:
    /// (session id, name, what, pane).
    pub waiting: Vec<(String, String, String, Option<crate::core::pane::Pane>)>,
    /// This seat's key in the fleet summary.
    pub seat_key: SeatId,
    pub mask: bool,
    pub color: bool,
    pub segments: Vec<String>,
    pub width: usize,
    pub lines: u8,
    /// The git branch of the session's directory (second line only).
    pub branch: Option<String>,
}

struct Paint(bool);

impl Paint {
    fn c(&self, s: &str, code: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

fn form(p: &Paint, parts: &[(&str, &str)]) -> (String, usize) {
    let text: String = parts
        .iter()
        .map(|(t, c)| {
            if c.is_empty() {
                t.to_string()
            } else {
                p.c(t, c)
            }
        })
        .collect();
    let width = parts
        .iter()
        .map(|(t, _)| unicode_width::UnicodeWidthStr::width(*t))
        .sum();
    (text, width)
}

/// A context window this full turns amber, then red (not a limit: its own thresholds).
const CONTEXT_WARN: f64 = 70.0;
const CONTEXT_BAD: f64 = 85.0;

/// The ANSI code of a limit's level (`core::limits::Level`), the same colours as the dashboard.
fn level(pct: f64) -> &'static str {
    match Level::of(pct) {
        Level::Ok => "32",
        Level::Warn => "33",
        Level::Bad | Level::Reached => "1;31",
    }
}

fn hm(d: chrono::Duration) -> String {
    let m = d.num_minutes().max(0);
    if m >= 24 * 60 {
        format!("{}d {}h", m / 1440, (m % 1440) / 60)
    } else if m >= 60 {
        format!("{}h {:02}m", m / 60, m % 60)
    } else {
        format!("{m}m")
    }
}

/// The status line: who needs you, this session's agents, the week, 5 hours, context, cost and
/// burn; shortened to `x.width` in a fixed order; optionally a second line.
pub fn render(input: &Input, x: &Extra, now: DateTime<Utc>) -> String {
    let p = Paint(x.color);
    let me = input.session_id.clone().unwrap_or_default();
    let seat = x.fleet.as_ref().and_then(|f| f.seats.get(&x.seat_key));
    let mut segs: Vec<Segment> = Vec::new();
    for key in &x.segments {
        let forms: Vec<(String, usize)> = match key.as_str() {
            "attention" => {
                let others: Vec<(String, String, Option<crate::core::pane::Pane>)> = match &x.fleet
                {
                    Some(f) => f
                        .needs_you
                        .iter()
                        .filter(|n| n.session_id.as_str() != me)
                        .map(|n| (n.name.clone(), n.what.clone(), n.pane.clone()))
                        .collect(),
                    None => x
                        .waiting
                        .iter()
                        .filter(|w| w.0 != me)
                        .map(|w| (w.1.clone(), w.2.clone(), w.3.clone()))
                        .collect(),
                };
                if others.is_empty() {
                    continue;
                }
                let n = others.len();
                let names: Vec<String> = others
                    .iter()
                    .map(|o| {
                        if x.mask {
                            "•••".into()
                        } else {
                            o.0.clone()
                        }
                    })
                    .collect();
                let full = if n == 1 {
                    format!("▲ {} needs you: {}", names[0], others[0].1)
                } else {
                    format!("▲ {n} need you: {}", names.join(", "))
                };
                let mut v = Vec::new();
                if !x.mask {
                    v.push(form(&p, &[(&full, "1;33"), (" · swarmy n J", "2")]));
                }
                v.push(form(
                    &p,
                    &[
                        (
                            &format!("▲ {n} need{} you", if n == 1 { "s" } else { "" }),
                            "1;33",
                        ),
                        (" · n J", "2"),
                    ],
                ));
                v.push(form(&p, &[(&format!("▲{n}"), "1;33")]));
                v
            }
            "agents" => {
                let Some(sc) = x
                    .fleet
                    .as_ref()
                    .and_then(|f| f.sessions.get(me.as_str()))
                    .filter(|s| s.agents > 0)
                else {
                    continue;
                };
                let mut parts: Vec<(String, &str)> = vec![(
                    format!(
                        "◐ {} agent{} ",
                        sc.agents,
                        if sc.agents == 1 { "" } else { "s" }
                    ),
                    "36",
                )];
                if sc.working > 0 {
                    parts.push((format!("●{} ", sc.working), "32"));
                }
                if sc.thinking > 0 {
                    parts.push((format!("◌{} ", sc.thinking), "36"));
                }
                if sc.waiting > 0 {
                    parts.push((format!("◐{} ", sc.waiting), "36"));
                }
                if sc.stalled > 0 {
                    parts.push((format!("✖{} ", sc.stalled), "1;31"));
                }
                let last = parts.len() - 1;
                parts[last].0 = parts[last].0.trim_end().to_string();
                let refs: Vec<(&str, &str)> = parts.iter().map(|(t, c)| (t.as_str(), *c)).collect();
                vec![
                    form(&p, &refs),
                    form(&p, &[(&format!("◐{}", sc.agents), "36")]),
                ]
            }
            "week" => {
                // Exact from stdin; else the dashboard's estimate (≈).
                let (pct, resets, exact) = match input
                    .rate_limits
                    .as_ref()
                    .and_then(|r| r.seven_day.as_ref())
                    .and_then(Window::pair)
                {
                    Some((p, t)) => (p, Some(t), true),
                    None => match seat.and_then(|s| s.week_used.map(|w| (w, s.resets_at))) {
                        Some((p, t)) => (p, t, false),
                        None => continue,
                    },
                };
                let approx = if exact { "" } else { "≈" };
                let head = format!("week used {approx}{pct:.0}%");
                let code = level(pct);
                let when = |t: DateTime<Utc>| {
                    t.with_timezone(&chrono::Local)
                        .format("%a %H:%M")
                        .to_string()
                };
                if Level::of(pct) == Level::Reached {
                    let r = resets
                        .map(|t| format!(" · resets {}", when(t)))
                        .unwrap_or_default();
                    vec![
                        form(
                            &p,
                            &[(&head, "1;31"), (" · limit reached", "1;31"), (&r, "2")],
                        ),
                        form(&p, &[(&head, "1;31"), (" · limit reached", "1;31")]),
                        form(&p, &[(&format!("week {approx}{pct:.0}%"), "1;31")]),
                    ]
                } else if let Some(out) = seat.and_then(|s| s.runs_out_at).filter(|t| *t > now) {
                    let before_reset = resets.is_none_or(|r| out < r);
                    let code = if before_reset && out - now < chrono::Duration::hours(24) {
                        "1;31"
                    } else {
                        code
                    };
                    vec![
                        form(
                            &p,
                            &[
                                (&head, code),
                                (
                                    &format!(" · runs out in {} ({})", hm(out - now), when(out)),
                                    code,
                                ),
                            ],
                        ),
                        form(
                            &p,
                            &[
                                (&head, code),
                                (&format!(" · runs out in {}", hm(out - now)), code),
                            ],
                        ),
                        form(&p, &[(&format!("week {approx}{pct:.0}%"), code)]),
                    ]
                } else {
                    let r = resets
                        .map(|t| format!(" · resets {}", when(t)))
                        .unwrap_or_default();
                    vec![
                        form(&p, &[(&head, code), (&r, "2")]),
                        form(&p, &[(&head, code)]),
                        form(&p, &[(&format!("week {approx}{pct:.0}%"), code)]),
                    ]
                }
            }
            "five_hour" => {
                let Some((pct, _)) = input
                    .rate_limits
                    .as_ref()
                    .and_then(|r| r.five_hour.as_ref())
                    .and_then(Window::pair)
                else {
                    continue;
                };
                vec![form(&p, &[(&format!("5h {pct:.0}%"), level(pct))])]
            }
            "context" => {
                let Some(c) = input
                    .context_window
                    .as_ref()
                    .and_then(|c| c.used_percentage)
                else {
                    continue;
                };
                let code = if c >= CONTEXT_BAD {
                    "1;31"
                } else if c >= CONTEXT_WARN {
                    "33"
                } else {
                    ""
                };
                vec![form(&p, &[(&format!("ctx {c:.0}%"), code)])]
            }
            "cost" => {
                let Some(c) = input.cost.as_ref().and_then(|c| c.total_cost_usd) else {
                    continue;
                };
                vec![form(&p, &[(&format!("${c:.2}"), "")])]
            }
            "burn" => {
                let Some(b) = x
                    .fleet
                    .as_ref()
                    .and_then(|f| f.sessions.get(me.as_str()))
                    .map(|s| s.burn_usd_h)
                    .filter(|b| *b > 0.0)
                else {
                    continue;
                };
                vec![form(&p, &[(&format!("${b:.1}/h"), "2")])]
            }
            _ => continue,
        };
        segs.push(Segment {
            key: static_key(key),
            forms,
            form: 0,
            dropped: false,
        });
    }
    // Shorten until it fits: drop or shorten pieces, least useful first; the alert and the week
    // never go.
    let sep = 3usize;
    let width = |segs: &[Segment]| -> usize {
        let live: Vec<&Segment> = segs.iter().filter(|s| !s.dropped).collect();
        live.iter().map(|s| s.forms[s.form].1).sum::<usize>() + sep * live.len().saturating_sub(1)
    };
    let pct_of = |k: &str| -> f64 {
        match k {
            "five_hour" => input
                .rate_limits
                .as_ref()
                .and_then(|r| r.five_hour.as_ref())
                .map(|w| w.used_percentage)
                .unwrap_or(0.0),
            "context" => input
                .context_window
                .as_ref()
                .and_then(|c| c.used_percentage)
                .unwrap_or(0.0),
            _ => 0.0,
        }
    };
    let steps: [(&str, bool); 10] = [
        ("burn", true),
        ("cost", true),
        ("five_hour", true),
        ("week", false),
        ("agents", false),
        ("attention", false),
        ("context", true),
        ("week", false),
        ("attention", false),
        ("agents", true),
    ];
    for (key, drop) in steps {
        if width(&segs) <= x.width {
            break;
        }
        if let Some(sg) = segs.iter_mut().find(|s| s.key == key && !s.dropped) {
            if drop {
                // A 5-hour limit from amber on and a context past 70% stay.
                let keep = (key == "five_hour" && Level::of(pct_of(key)) >= Level::Warn)
                    || (key == "context" && pct_of(key) >= CONTEXT_WARN);
                if !keep {
                    sg.dropped = true;
                }
            } else if sg.form + 1 < sg.forms.len() {
                sg.form += 1;
            }
        }
    }
    let line: Vec<String> = segs
        .iter()
        .filter(|s| !s.dropped)
        .map(|s| s.forms[s.form].0.clone())
        .collect();
    let mut out = if line.is_empty() {
        p.c("swarmy", "2")
    } else {
        line.join(&p.c(" │ ", "2"))
    };
    if x.lines >= 2 {
        let mut second: Vec<String> = Vec::new();
        if let Some(m) = input.model.as_ref().and_then(|m| m.display_name.clone()) {
            let e = input
                .effort
                .as_ref()
                .and_then(|e| e.level.clone())
                .map(|e| format!(" {e}"))
                .unwrap_or_default();
            second.push(format!("{m}{e}"));
        }
        if let Some(b) = x.branch.clone() {
            let wt = input
                .workspace
                .as_ref()
                .and_then(|w| w.git_worktree.as_ref())
                .is_some_and(|v| !v.is_null());
            second.push(format!("⎇ {b}{}", if wt { " (worktree)" } else { "" }));
        }
        if let Some(n) = input.session_name.clone().filter(|_| !x.mask) {
            second.push(n);
        }
        if input.prompt_cache.as_ref().and_then(|c| c.warm) == Some(false) {
            second.push("cache cold".into());
        }
        if !second.is_empty() {
            let l = truncate_to_width(&second.join(" · "), x.width);
            out.push('\n');
            out.push_str(&p.c(&l, "2"));
        }
    }
    out
}

/// The columns a terminal gives `s`: escape sequences (colours, OSC links) take none, wide
/// characters take two (review L3).
pub fn visible_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(strip_escapes(s).as_str())
}

/// `s` without its terminal escape sequences: CSI (`ESC [ … final byte`) and OSC
/// (`ESC ] … BEL` or `ESC \\`).
fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('[') => {
                for c in it.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = it.next() {
                    if c == '\x07' || (c == '\x1b' && it.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            Some(_) | None => {}
        }
    }
    out
}

/// The longest start of `s` that fits `width` columns, cut at a character, never inside a wide
/// one (review L3: a cut by character count made Chinese, Japanese or Korean text twice as wide).
pub fn truncate_to_width(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

fn static_key(k: &str) -> &'static str {
    [
        "attention",
        "agents",
        "week",
        "five_hour",
        "context",
        "cost",
        "burn",
    ]
    .into_iter()
    .find(|s| *s == k)
    .unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statusline::input::parse_input;
    use chrono::TimeZone;

    const INPUT: &str = r#"{"session_id":"s1","model":{"display_name":"Opus"},"cost":{"total_cost_usd":4.2},
        "context_window":{"used_percentage":42.0},
        "rate_limits":{"five_hour":{"used_percentage":12.5,"resets_at":1795400000},
                       "seven_day":{"used_percentage":3,"resets_at":1795960000}}}"#;

    fn extra(fleet: Option<crate::fleet::Fleet>, width: usize) -> Extra {
        Extra {
            fleet,
            waiting: Vec::new(),
            seat_key: SeatId::from_stored("0123456789abcdef"),
            mask: false,
            color: false,
            segments: crate::config::StatuslineConf::default().segments,
            width,
            lines: 1,
            branch: None,
        }
    }

    fn fleet(now: DateTime<Utc>) -> crate::fleet::Fleet {
        use crate::fleet::*;
        let mut f = Fleet {
            v: VERSION,
            written_at: now,
            ..Default::default()
        };
        f.needs_you = vec![
            NeedsYou {
                session_id: "other".into(),
                name: "shop-api".into(),
                what: "permission".into(),
                ..Default::default()
            },
            NeedsYou {
                session_id: "s1".into(),
                name: "me".into(),
                what: "question".into(),
                ..Default::default()
            },
            NeedsYou {
                session_id: "x".into(),
                name: "docs".into(),
                what: "question".into(),
                ..Default::default()
            },
        ];
        f.sessions.insert(
            "s1".into(),
            SessionCounts {
                agents: 3,
                working: 2,
                thinking: 1,
                burn_usd_h: 3.1,
                ..Default::default()
            },
        );
        f.seats.insert(
            SeatId::from_stored("0123456789abcdef"),
            SeatLimit {
                runs_out_at: Some(now + chrono::Duration::hours(7)),
                resets_at: Some(now + chrono::Duration::days(3)),
                ..Default::default()
            },
        );
        f
    }

    #[test]
    fn the_full_line_and_how_it_shortens() {
        let now = Utc.timestamp_opt(1_795_380_000, 0).unwrap();
        let input = parse_input(INPUT);
        let full = render(&input, &extra(Some(fleet(now)), 400), now);
        // The other two sessions need you (not this one), this session's agents, the week with
        // its runs-out time, 5 hours, context, cost, burn.
        assert!(full.starts_with("▲ 2 need you: shop-api, docs · swarmy n J │ ◐ 3 agents ●2 ◌1 │ week used 3% · runs out in 7h 00m ("), "{full}");
        assert!(
            full.ends_with(") │ 5h 12% │ ctx 42% │ $4.20 │ $3.1/h"),
            "{full}"
        );

        // Narrow: burn, cost, 5h go; then shorter forms; the alert and the week stay.
        let mid = render(&input, &extra(Some(fleet(now)), 80), now);
        assert!(mid.len() <= 80 * 3, "{mid}");
        assert!(mid.contains("▲") && mid.contains("week"), "{mid}");
        assert!(!mid.contains("$3.1/h") && !mid.contains("$4.20"), "{mid}");
        let tiny = render(&input, &extra(Some(fleet(now)), 30), now);
        assert!(tiny.starts_with("▲2 │ "), "{tiny}");
        assert!(tiny.contains("week 3%"), "{tiny}");
    }

    /// Review L3: the user's coloured status line is measured by what it shows, and the second
    /// line is cut to the width in columns, not characters.
    #[test]
    fn widths_count_columns_not_bytes_or_escapes() {
        let coloured = "\x1b[1;32mmain\x1b[0m \x1b]8;;https://example.com\x07link\x1b]8;;\x07 ok";
        assert_eq!(visible_width(coloured), "main link ok".len());
        assert_eq!(visible_width("項目の説明"), 10);
        assert_eq!(truncate_to_width("項目の説明", 5), "項目");
        assert_eq!(truncate_to_width("abc", 5), "abc");
        let now = Utc::now();
        let input = parse_input(r#"{"session_id":"s","session_name":"重要な機能の作業"}"#);
        let mut x = extra(None, 20);
        x.lines = 2;
        let out = render(&input, &x, now);
        let second = out.lines().nth(1).unwrap();
        assert!(visible_width(second) <= 20, "{second:?}");
        assert!(second.contains("重要な機能"), "{second:?}");
    }

    #[test]
    fn without_the_dashboard_or_limits() {
        let now = Utc::now();
        let input = parse_input(r#"{"session_id":"s","cost":{"total_cost_usd":0.5}}"#);
        assert_eq!(render(&input, &extra(None, 120), now), "$0.50");
        // The registry fallback still tells who waits; masked names are hidden.
        let mut x = extra(None, 120);
        x.waiting = vec![("other".into(), "client-x".into(), "permission".into(), None)];
        assert_eq!(
            render(&input, &x, now),
            "▲ client-x needs you: permission · swarmy n J │ $0.50"
        );
        x.mask = true;
        assert_eq!(render(&input, &x, now), "▲ 1 needs you · n J │ $0.50");
    }
}
