//! AGENT: the selected agent: card, the last thing it said, tool timeline and its life chart.

use chrono::{Local, Utc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CLine};

use super::{dim, faint};
use crate::core::models::{context_window, short_name};
use crate::tui::app::{App, Panel};
use crate::tui::view::kit::*;
use crate::ui::fmt;

fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if w(&cur) + w(word) + 1 > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            if out.len() == max_lines {
                return out;
            }
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() && out.len() < max_lines {
        out.push(cur);
    }
    out
}

pub fn agent(f: &mut Frame, app: &mut App, area: Rect) {
    let name = app
        .selected_agent()
        .map(|a| a.name.clone())
        .unwrap_or_default();
    let inner = frame(f, app, Panel::Agent, area, &name, false);
    let app: &App = app;
    let th = app.theme;
    let Some(a) = app.selected_agent().cloned() else {
        put(
            f,
            inner,
            vec![Line::from(Span::styled(
                "select an agent in SWARM",
                dim(app),
            ))],
        );
        return;
    };
    let wide = inner.width >= 90;
    let (card, tl) = if wide {
        let cw = (inner.width / 2).clamp(38, 56);
        (
            Rect { width: cw, ..inner },
            Rect {
                x: inner.x + cw + 1,
                width: inner.width - cw - 1,
                ..inner
            },
        )
    } else {
        let ch = 13.min(inner.height);
        (
            Rect {
                height: ch,
                ..inner
            },
            Rect {
                y: inner.y + ch,
                height: inner.height - ch,
                ..inner
            },
        )
    };
    let now = Utc::now();
    let (g, gc) = glyph(app, &a.state);
    let kw = 10usize;
    let kv = |k: &str, v: String, c: Color| {
        Line::from(vec![
            Span::styled(fit(k, kw), faint(app)),
            Span::styled(v, Style::new().fg(c)),
        ])
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{g} "), Style::new().fg(gc)),
        Span::styled(a.name.clone(), th.bold(th.fg)),
    ])];
    lines.push(kv("state", activity(&a.state), th.state(&a.state)));
    lines.push(kv(
        "since",
        format!(
            "{} · {}",
            a.since.with_timezone(&Local).format("%H:%M:%S"),
            fmt::duration(now - a.since)
        ),
        th.fg,
    ));
    lines.push(kv(
        "model",
        format!(
            "{} · effort {}",
            a.model.as_deref().map(short_name).unwrap_or_default(),
            a.effort.clone().unwrap_or_else(|| "-".into())
        ),
        th.family(a.family()),
    ));
    lines.push(kv(
        "type",
        a.agent_type.clone().unwrap_or_else(|| {
            if a.is_main() {
                "main thread".into()
            } else {
                "subagent".into()
            }
        }),
        th.fg,
    ));
    lines.push(Line::from(vec![
        Span::styled(fit("where", kw), faint(app)),
        Span::styled(a.project.clone(), th.bold(th.dir)),
        Span::styled(
            format!(" · {} · {}", a.seat_label, a.session_id.short()),
            Style::new().fg(th.fg),
        ),
    ]));
    let cost = if a.is_main() {
        format!(
            "{} · session Σ{} · {}/h",
            fmt::usd(a.cost),
            fmt::usd(a.session_cost),
            fmt::usd(a.session_burn)
        )
    } else {
        format!("{} · {} requests", fmt::usd(a.cost), fmt::count(a.requests))
    };
    lines.push(kv("cost", cost, th.fg));
    let win = a.model.as_deref().map(context_window).unwrap_or(1_000_000);
    let frac = a.context_tokens as f64 / win as f64;
    let mut ctx = vec![Span::styled(fit("context", kw), faint(app))];
    ctx.extend(gauge(
        frac,
        14,
        if frac >= 0.8 { th.warn } else { th.thinking },
        &th,
    ));
    ctx.push(Span::styled(
        format!(" {} of {}", fmt::tokens(a.context_tokens), fmt::tokens(win)),
        dim(app),
    ));
    lines.push(Line::from(ctx));
    if let Some(p) = &a.pane {
        lines.push(kv(
            p.terminal(),
            format!("{} · J jumps there", p.place()),
            th.accent,
        ));
    }
    if app.timeline.0 == a.key()
        && let Some(msg) = &app.timeline.2
    {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(" LAST MESSAGE ", th.heading())));
        // The message gets what the life chart (at most 12 lines) leaves.
        let room = (card.height as usize)
            .saturating_sub(lines.len() + 13)
            .clamp(3, 14);
        for l in wrap(
            if app.mask { "•••" } else { msg },
            card.width as usize,
            room,
        ) {
            lines.push(Line::from(Span::styled(l, Style::new().fg(th.fg))));
        }
    }
    let used = lines.len() as u16;
    put(f, card, lines);
    let life_area = Rect {
        y: card.y + used + 1,
        height: card.height.saturating_sub(used + 1).min(12),
        ..card
    };
    if life_area.height >= 6 && app.life.0 == a.key() {
        agent_life(f, app, &a, life_area);
    }

    // Tool timeline, newest first, scrollable when the panel has focus.
    let mut lines = Vec::new();
    let total = if app.timeline.0 == a.key() {
        app.timeline.1.len()
    } else {
        0
    };
    lines.push(Line::from(vec![
        Span::styled(" TOOLS ", th.heading()),
        Span::styled(
            format!(
                " · {total} recent{}",
                if app.focus == Panel::Agent {
                    " · ↑↓ scroll"
                } else {
                    ""
                }
            ),
            dim(app),
        ),
    ]));
    if app.timeline.0 == a.key() {
        let start = app.cur(Panel::Agent).min(total.saturating_sub(1));
        let sum_w = (tl.width as usize).saturating_sub(29);
        for t in app
            .timeline
            .1
            .iter()
            .skip(start)
            .take(tl.height.saturating_sub(1) as usize)
        {
            let (mark, c, dur) = match t.done {
                Some(d) if t.is_error => ("✗", th.bad, fmt::duration(d - t.ts)),
                Some(d) => ("✓", th.good, fmt::duration(d - t.ts)),
                None => ("▶", pulse(app, th.warn, 1.0), fmt::clock(now - t.ts)),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", t.ts.with_timezone(&Local).format("%H:%M:%S")),
                    faint(app),
                ),
                Span::styled(format!("{mark} "), Style::new().fg(c)),
                Span::styled(fit_right(&dur, 6), dim(app)),
                Span::styled(format!(" {} ", fit(&t.name, 8)), th.bold(th.fg)),
                Span::styled(fit(if app.mask { "" } else { &t.summary }, sum_w), dim(app)),
            ]));
        }
    } else {
        lines.push(Line::from(Span::styled("loading…", faint(app))));
    }
    put(f, tl, lines);
}

/// The agent's life: what each request cost (bars) and how the context grew (line), from its
/// first request to now.
fn agent_life(f: &mut Frame, app: &App, a: &crate::live::Agent, area: Rect) {
    let th = app.theme;
    let pts = &app.life.1;
    let (Some(first), Some(last)) = (pts.first(), pts.last()) else {
        return;
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(" LIFE ", th.heading()),
        Span::styled(
            format!(
                " · {} requests over {} · max {}/req",
                fmt::count(pts.len() as u64),
                fmt::duration(Utc::now() - first.ts),
                fmt::usd(pts.iter().map(|p| p.cost).fold(0.0, f64::max))
            ),
            faint(app),
        ),
    ])];
    put(f, Rect { height: 1, ..area }, std::mem::take(&mut lines));
    let chart = Rect {
        y: area.y + 1,
        height: area.height - 2,
        ..area
    };
    let legend = Rect {
        y: area.y + area.height - 1,
        height: 1,
        ..area
    };
    let t0 = first.ts;
    let span = ((Utc::now() - t0).num_seconds().max(60)) as f64;
    let x = |t: chrono::DateTime<Utc>| (t - t0).num_seconds() as f64 / span * 100.0;
    // Costs summed into as many columns as the chart has braille dots.
    let cols = (chart.width as usize * 2).max(2);
    let mut bins = vec![0.0f64; cols];
    for p in pts {
        let i = ((x(p.ts) / 100.0) * (cols - 1) as f64).round() as usize;
        bins[i.min(cols - 1)] += p.cost;
    }
    let bmax = bins.iter().copied().fold(0.0, f64::max).max(1e-9);
    let win = a.model.as_deref().map(context_window).unwrap_or(1_000_000) as f64;
    let ctx_pts: Vec<(f64, f64)> = pts
        .iter()
        .map(|p| (x(p.ts), p.context as f64 / win * 100.0))
        .collect();
    let (bar_c, ctx_c, track) = (th.family(a.family()), th.thinking, th.track);
    let compactions: Vec<f64> = pts
        .windows(2)
        .filter(|w| w[1].context * 2 < w[0].context && w[0].context > 50_000)
        .map(|w| x(w[1].ts))
        .collect();
    let warn = th.warn;
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, 100.0])
        .y_bounds([0.0, 100.0])
        .paint(move |ctx| {
            for (i, b) in bins.iter().enumerate() {
                if *b > 0.0 {
                    let xx = i as f64 / (cols - 1) as f64 * 100.0;
                    ctx.draw(&CLine::new(xx, 0.0, xx, b / bmax * 60.0, bar_c));
                }
            }
            for w in ctx_pts.windows(2) {
                ctx.draw(&CLine::new(w[0].0, w[0].1, w[1].0, w[1].1, ctx_c));
            }
            for c in &compactions {
                ctx.draw(&CLine::new(*c, 0.0, *c, 100.0, warn));
            }
            ctx.draw(&CLine::new(0.0, 0.0, 100.0, 0.0, track));
        });
    f.render_widget(canvas, chart);
    let now_ctx = last.context as f64 / win * 100.0;
    put(
        f,
        legend,
        vec![Line::from(vec![
            Span::styled("▮ $ per request  ", Style::new().fg(bar_c)),
            Span::styled(
                format!("─ context {}  ", fmt::pct(now_ctx)),
                Style::new().fg(ctx_c),
            ),
            Span::styled("│ compaction", Style::new().fg(th.warn)),
        ])],
    );
}
