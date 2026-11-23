//! Screen composition: the wall (large terminals), the column (medium) and tabs (small), plus
//! the header and the footer. Overlays (dialogs, toast) are in `overlays.rs`, help and the
//! legend in `help.rs`.

mod help;
mod kit;
mod overlays;
mod panels;

use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::app::{App, Mark, Moves, Panel, Shape, Shot, ShotKey, TABS};
use super::boot;
use crate::core::limits::Level;
use crate::live::State;
use crate::ui::fmt;
use help::help;
use kit::*;
use overlays::{modal, picker, toast};

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if app.boot.is_some() {
        if boot::done(app) {
            app.boot = None;
        } else {
            boot::draw(f, app, area);
            return;
        }
    }
    let th = app.theme;
    f.render_widget(Block::new().style(th.base()), area);
    app.rects.clear();
    app.shape = Shape::pick(app.layout, area);
    if !app.visible_panels().contains(&app.focus) {
        app.focus = app.visible_panels()[0];
    }
    let head = Rect { height: 1, ..area };
    let foot = Rect {
        y: area.y + area.height.saturating_sub(1),
        height: 1,
        ..area
    };
    let body = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(2),
        ..area
    };
    header(f, app, head);
    match app.zoom {
        Some(p) => draw_panel(f, app, p, body, true),
        None => match app.shape {
            Shape::Wall => wall(f, app, body),
            Shape::Column => column(f, app, body),
            Shape::Tabs => tabs(f, app, body),
        },
    }
    footer(f, app, foot);
    picker(f, app, area);
    modal(f, app, area);
    if app.help {
        help(f, app, area);
    }
    toast(f, app, area);
    if app.mask {
        // Login labels made from an email's local part look like any word; blank them (and the
        // OS user name) wherever they were drawn.
        let mut words =
            crate::ui::mask::words(app.vault.logins.iter().filter_map(|l| l.email.as_deref()));
        words.extend(user_word().map(str::to_string));
        mask_words(f, area, &words);
    }
}

/// The OS user name (from the home dir), the one personal word that can sit anywhere on screen:
/// login labels, project dirs, paths. Masking blanks it out of the finished frame.
fn user_word() -> Option<&'static str> {
    static U: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    U.get_or_init(|| {
        let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
        let name = home.file_name()?.to_string_lossy().into_owned();
        (name.chars().count() >= 3 && name.is_ascii()).then_some(name)
    })
    .as_deref()
}

/// Blanks every whole occurrence of each word, one cell per character, wherever it was drawn.
/// One pass per row: the row's single-character cells are read once, and a word is compared only
/// where its first character is.
fn mask_words(f: &mut Frame, area: Rect, words: &[String]) {
    let words: Vec<Vec<char>> = words
        .iter()
        .map(|w| w.chars().collect::<Vec<char>>())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return;
    }
    let buf = f.buffer_mut();
    let mut row: Vec<Option<char>> = Vec::with_capacity(area.width as usize);
    for y in area.top()..area.bottom() {
        row.clear();
        row.extend((area.left()..area.right()).map(|x| {
            buf.cell((x, y)).and_then(|c| {
                let mut cs = c.symbol().chars();
                match (cs.next(), cs.next()) {
                    (Some(ch), None) => Some(ch),
                    (Some(_), Some(_)) | (None, Some(_)) | (None, None) => None,
                }
            })
        }));
        for w in &words {
            let n = w.len();
            let mut i = 0;
            while i + n <= row.len() {
                if row[i] == Some(w[0]) && w.iter().zip(&row[i..i + n]).all(|(c, r)| Some(*c) == *r)
                {
                    for (j, r) in row.iter_mut().enumerate().skip(i).take(n) {
                        if let Some(c) = buf.cell_mut((area.left() + j as u16, y)) {
                            c.set_symbol("•");
                        }
                        *r = Some('•');
                    }
                    i += n;
                } else {
                    i += 1;
                }
            }
        }
    }
}

fn draw_panel(f: &mut Frame, app: &mut App, p: Panel, area: Rect, big: bool) {
    if area.width < 10 || area.height < 3 {
        return;
    }
    // A panel that did not animate is drawn again only when the app's state, its place or the
    // clock's second changed: frames come five times a second for the animated ones.
    let key = ShotKey {
        area,
        big,
        rev: app.rev,
        second: Utc::now().timestamp(),
        eco: app.eco,
    };
    let phase = blink_at(app.clock_now());
    if let Some(shot) = app
        .shots
        .get(&p)
        .filter(|s| s.key == key && s.blink.is_none_or(|b| b == phase))
    {
        let buf = f.buffer_mut();
        let mut cells = shot.cells.iter();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let (Some(c), Some(dst)) = (cells.next(), buf.cell_mut((x, y))) {
                    dst.clone_from(c);
                }
            }
        }
        patch_marks(buf, &shot.marks, app);
        app.rects.extend(shot.rects.iter().copied());
        return;
    }
    let rects_before = app.rects.len();
    app.anim_used.set(false);
    app.marking.set(app.cache_panels);
    app.blink_used.set(false);
    app.pulses.borrow_mut().clear();
    draw_panel_now(f, app, p, area, big);
    if !app.cache_panels {
        return;
    }
    app.marking.set(false);
    let buf = f.buffer_mut();
    let marks = find_marks(buf, area, &app.pulses.borrow());
    if !app.anim_used.get() {
        let mut cells = Vec::with_capacity(area.area() as usize);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(c) = buf.cell((x, y)) {
                    cells.push(c.clone());
                }
            }
        }
        let rects = app.rects[rects_before..].to_vec();
        patch_marks(buf, &marks, app);
        app.shots.insert(
            p,
            Shot {
                key,
                cells,
                marks,
                blink: app.blink_used.get().then_some(phase),
                rects,
            },
        );
    } else {
        patch_marks(buf, &marks, app);
        app.shots.remove(&p);
    }
}

/// The cells of `area` that pulse or spin, from the marks drawing left in them.
fn find_marks(buf: &ratatui::buffer::Buffer, area: Rect, pulses: &[(Color, f64)]) -> Vec<Mark> {
    let pulse = |c: Color| {
        let Color::Indexed(i) = c else { return None };
        i.checked_sub(PULSE_MARK)
            .and_then(|k| pulses.get(k as usize).copied())
    };
    let mut out = Vec::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let Some(c) = buf.cell((x, y)) else { continue };
            if let Some((col, hz)) = pulse(c.fg) {
                out.push(Mark {
                    x,
                    y,
                    what: Moves::Fg(col, hz),
                });
            }
            if let Some((col, hz)) = pulse(c.bg) {
                out.push(Mark {
                    x,
                    y,
                    what: Moves::Bg(col, hz),
                });
            }
            if c.symbol() == SPINNER_MARK {
                out.push(Mark {
                    x,
                    y,
                    what: Moves::Spinner,
                });
            }
        }
    }
    out
}

/// Draws this frame's colour or spinner into each marked cell.
fn patch_marks(buf: &mut ratatui::buffer::Buffer, marks: &[Mark], app: &App) {
    if marks.is_empty() {
        return;
    }
    let t = app.clock_now();
    let mut spin = [0u8; 4];
    let spin: &str = spinner_at(t).encode_utf8(&mut spin);
    for m in marks {
        let Some(c) = buf.cell_mut((m.x, m.y)) else {
            continue;
        };
        match m.what {
            Moves::Fg(col, hz) => {
                c.fg = pulse_at(app, col, hz, t);
            }
            Moves::Bg(col, hz) => {
                c.bg = pulse_at(app, col, hz, t);
            }
            Moves::Spinner => {
                c.set_symbol(spin);
            }
        }
    }
}

fn draw_panel_now(f: &mut Frame, app: &mut App, p: Panel, area: Rect, big: bool) {
    match p {
        Panel::Swarm => panels::swarm(f, app, area),
        Panel::Limits => panels::limits_seats(f, app, area, big),
        Panel::Burn => panels::burn(f, app, area),
        Panel::Attention => panels::attention(f, app, area),
        Panel::Agent => panels::agent(f, app, area),
        Panel::Projects => panels::projects(f, app, area),
        Panel::Models => panels::models(f, app, area),
        Panel::History => panels::history(f, app, area),
        Panel::Efficiency => panels::efficiency(f, app, area),
        Panel::System => panels::system(f, app, area),
    }
}

/// Rows the LIMITS · SEATS table needs: borders, header, one per seat and parked login, a hint.
fn limits_height(app: &App) -> u16 {
    app.limits_rows().len() as u16 + 4
}

/// Everything at once (from about 250×55).
fn wall(f: &mut Frame, app: &mut App, body: Rect) {
    // A 4K wall gets one row of bottom panels; a smaller one stacks them in two rows, so every
    // panel stays on screen, smaller.
    let wide = body.width >= 240;
    let band_a = if body.height >= 70 {
        (limits_height(app) + 9).clamp(14, 21)
    } else {
        limits_height(app).clamp(8, 14)
    };
    let band_c = if wide {
        if body.height >= 75 {
            24
        } else if body.height >= 62 {
            19
        } else {
            15
        }
    } else {
        (body.height * 34 / 100).clamp(18, 30)
    };
    let band_b = body.height.saturating_sub(band_a + band_c);
    let rows = vsplit_fixed(body, &[band_a, band_b, band_c]);
    let a = hsplit(rows[0], if wide { &[46, 26, 28] } else { &[50, 28, 22] });
    draw_panel(f, app, Panel::Limits, a[0], false);
    draw_panel(f, app, Panel::Attention, a[1], false);
    draw_panel(f, app, Panel::Burn, a[2], false);
    let b = hsplit(rows[1], if wide { &[64, 36] } else { &[60, 40] });
    draw_panel(f, app, Panel::Swarm, b[0], false);
    draw_panel(f, app, Panel::Agent, b[1], false);
    if wide {
        let c = hsplit(rows[2], &[25, 23, 19, 33]);
        draw_panel(f, app, Panel::Projects, c[0], false);
        draw_panel(f, app, Panel::Models, c[1], false);
        draw_panel(f, app, Panel::History, c[2], false);
        draw_panel(f, app, Panel::Efficiency, c[3], false);
    } else {
        let half = rows[2].height / 2;
        let r = vsplit_fixed(rows[2], &[half, rows[2].height - half]);
        let c1 = hsplit(r[0], &[45, 55]);
        draw_panel(f, app, Panel::Projects, c1[0], false);
        draw_panel(f, app, Panel::Models, c1[1], false);
        let c2 = hsplit(r[1], &[40, 60]);
        draw_panel(f, app, Panel::History, c2[0], false);
        draw_panel(f, app, Panel::Efficiency, c2[1], false);
    }
}

/// Narrower or shorter terminals (from about 110×40): the same panels stacked.
fn column(f: &mut Frame, app: &mut App, body: Rect) {
    let band_a = limits_height(app).clamp(8, 13);
    let rest = body.height.saturating_sub(band_a);
    let swarm = rest * 40 / 100;
    let agent = rest * 22 / 100;
    let bottom = rest.saturating_sub(swarm + agent);
    let rows = vsplit_fixed(body, &[band_a, swarm, agent, bottom]);
    let a = hsplit(
        rows[0],
        if body.width >= 140 {
            &[50, 30, 20]
        } else {
            &[62, 38, 0]
        },
    );
    draw_panel(f, app, Panel::Limits, a[0], false);
    draw_panel(f, app, Panel::Attention, a[1], false);
    draw_panel(f, app, Panel::Burn, a[2], false);
    draw_panel(f, app, Panel::Swarm, rows[1], false);
    draw_panel(f, app, Panel::Agent, rows[2], false);
    if bottom >= 22 {
        let half = bottom / 2;
        let r = vsplit_fixed(rows[3], &[half, bottom - half]);
        let c1 = hsplit(r[0], &[55, 45]);
        draw_panel(f, app, Panel::Projects, c1[0], false);
        draw_panel(f, app, Panel::Efficiency, c1[1], false);
        let c2 = hsplit(r[1], &[45, 55]);
        draw_panel(f, app, Panel::History, c2[0], false);
        draw_panel(f, app, Panel::Models, c2[1], false);
    } else {
        let c = hsplit(rows[3], &[30, 25, 20, 25]);
        draw_panel(f, app, Panel::Projects, c[0], false);
        draw_panel(f, app, Panel::Efficiency, c[1], false);
        draw_panel(f, app, Panel::History, c[2], false);
        draw_panel(f, app, Panel::Models, c[3], false);
    }
}

/// Small terminals: one tab at a time.
fn tabs(f: &mut Frame, app: &mut App, body: Rect) {
    use super::app::Tab;
    match app.tab {
        Tab::Ops => {
            let limits_h = limits_height(app).clamp(6, 10).min(body.height / 3);
            let strip = if body.height > 28 { 8 } else { 0 };
            let swarm = body.height.saturating_sub(limits_h + strip);
            let rows = vsplit_fixed(body, &[limits_h, swarm, strip]);
            draw_panel(f, app, Panel::Limits, rows[0], false);
            draw_panel(f, app, Panel::Swarm, rows[1], false);
            if strip > 0 {
                draw_panel(f, app, Panel::Attention, rows[2], false);
            }
        }
        Tab::Agent => {
            let c = hsplit(body, &[45, 55]);
            draw_panel(f, app, Panel::Swarm, c[0], false);
            draw_panel(f, app, Panel::Agent, c[1], false);
        }
        Tab::Limits => draw_panel(f, app, Panel::Limits, body, true),
        Tab::Spend => {
            let c = hsplit(body, &[52, 48]);
            let l = vsplit_fixed(c[0], &[c[0].height / 2, c[0].height - c[0].height / 2]);
            draw_panel(f, app, Panel::Projects, l[0], false);
            draw_panel(f, app, Panel::Efficiency, l[1], false);
            let r = vsplit_fixed(c[1], &[c[1].height / 2, c[1].height - c[1].height / 2]);
            draw_panel(f, app, Panel::History, r[0], false);
            draw_panel(f, app, Panel::Models, r[1], false);
        }
        Tab::System => draw_panel(f, app, Panel::System, body, true),
    }
}

fn header(f: &mut Frame, app: &App, area: Rect) {
    let th = &app.theme;
    let s = &app.snapshot;
    let fresh = s
        .taken_at
        .is_some_and(|t| (Utc::now() - t).num_milliseconds() < 2500);
    let total_burn: Vec<f64> = (0..60)
        .map(|i| s.burn.values().map(|v| v[i]).sum())
        .collect();
    let n = |p: &dyn Fn(&State) -> bool| s.count(p);
    let needs = n(&|x| matches!(x, State::NeedsYou(_)));
    let mut spans = vec![
        Span::styled(" SWARMY ", th.bold(th.bg).bg(th.accent)),
        Span::styled(
            if fresh { " ● " } else { " ○ " },
            Style::new().fg(if fresh { th.good } else { th.dim }),
        ),
        Span::styled("today ", Style::new().fg(th.faint)),
        Span::styled(fmt::usd(s.today_cost), th.bold(th.fg)),
    ];
    // Groups that can go when the line is too long (label and value leave together).
    let mut groups: Vec<(&str, usize)> = Vec::new();
    groups.push(("period", spans.len()));
    if let (Some(t), Some(p)) = (&app.analytics.total, &app.analytics.period) {
        spans.push(Span::styled(
            format!("  {} ", p.label()),
            Style::new().fg(th.faint),
        ));
        spans.push(Span::styled(fmt::usd(t.cost), th.bold(th.fg)));
        if let Some(prev) = app.analytics.prev_total {
            spans.push(panels::trend(app, t.cost, prev));
        }
    }
    groups.push(("burn", spans.len()));
    spans.push(Span::styled("  burn ", Style::new().fg(th.faint)));
    spans.push(Span::styled(
        format!("{}/h ", fmt::usd(s.burn_per_hour)),
        th.bold(th.accent),
    ));
    groups.push(("spark", spans.len()));
    if area.width >= 170 {
        spans.push(spark(&total_burn[36..], th.accent));
    }
    groups.push(("agents", spans.len()));
    spans.push(Span::styled(
        format!("   {} agents ", s.agents.len()),
        Style::new().fg(th.dim),
    ));
    // The same glyphs and words as ATTENTION and the legend.
    for (glyph, count, label, c) in [
        (
            "●",
            n(&|x| matches!(x, State::Tool { .. })),
            "working",
            th.good,
        ),
        (
            "◌",
            n(&|x| matches!(x, State::Thinking)),
            "thinking",
            th.thinking,
        ),
    ] {
        spans.push(Span::styled(format!("{glyph}{count} "), Style::new().fg(c)));
        spans.push(Span::styled(format!("{label} "), Style::new().fg(th.faint)));
    }
    if needs > 0 {
        spans.push(Span::styled(
            format!("▲{needs} needs you "),
            th.bold(pulse(app, th.warn, 1.5)),
        ));
    }
    let stalled = n(&|x| matches!(x, State::Stalled));
    if stalled > 0 {
        spans.push(Span::styled(
            format!("✖{stalled} stalled "),
            Style::new().fg(th.bad),
        ));
    }
    groups.push(("week", spans.len()));
    // The tightest account that sits in a seat (a parked login cannot be used anyway).
    let seated: Vec<&str> = app
        .analytics
        .seats
        .iter()
        .filter_map(|s| s.account.as_deref())
        .collect();
    if let Some((name, pct, resets)) = app
        .analytics
        .limits
        .iter()
        .filter(|r| seated.is_empty() || seated.contains(&r.account_uuid.as_str()))
        .filter_map(|r| {
            r.windows
                .iter()
                .filter(|w| !w.reset_passed)
                .map(|w| (r.name.clone(), w.estimate_now.get(), w.reading.resets_at))
                .max_by(|a, b| a.1.total_cmp(&b.1))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
    {
        spans.push(Span::styled("  week used ", Style::new().fg(th.faint)));
        let value = if Level::of(pct) == Level::Reached {
            "limit reached".to_string()
        } else {
            fmt::pct(pct)
        };
        spans.push(Span::styled(
            format!("{} {value}", mask_name(app, &name)),
            th.bold(th.utilization(pct)),
        ));
        spans.push(Span::styled(
            format!(
                " · resets in {}",
                fmt::duration(resets - chrono::Utc::now())
            ),
            Style::new().fg(th.faint),
        ));
    }
    groups.push(("eco", spans.len()));
    if app.eco {
        spans.push(Span::styled(
            "  ECO (battery: slower refresh)",
            Style::new().fg(th.good),
        ));
    }
    let right = if area.width >= 170 {
        format!("layout {:?} · {} ", app.shape, app.clock()).to_lowercase()
    } else {
        format!("{} ", app.clock())
    };
    // Too long: whole groups go, least useful first.
    groups.push(("end", spans.len()));
    for drop in ["eco", "spark", "period", "burn", "agents", "week"] {
        if spans.iter().map(|s| w(&s.content)).sum::<usize>() + w(&right) < area.width as usize {
            break;
        }
        let Some(i) = groups.iter().position(|g| g.0 == drop) else {
            continue;
        };
        let (start, end) = (groups[i].1, groups[i + 1].1);
        for sp in &mut spans[start..end] {
            *sp = Span::raw("");
        }
    }
    let used: usize = spans.iter().map(|s| w(&s.content)).sum();
    spans.push(Span::raw(
        " ".repeat((area.width as usize).saturating_sub(used + w(&right))),
    ));
    spans.push(Span::styled(right, Style::new().fg(th.faint)));
    f.render_widget(Paragraph::new(Line::from(spans)).style(th.base()), area);
}

fn panel_keys(p: Panel) -> &'static str {
    match p {
        Panel::Swarm => "↑↓ select · Enter details · J jump · / filter",
        Panel::Agent => "↑↓ scroll tools · J jump",
        Panel::Limits => {
            "↑↓ · Enter switch · S best · c save · a add · r rename · d forget · R re-read"
        }
        Panel::Attention => "↑↓ · Enter go there · n/N next",
        Panel::Projects => "↑↓ · Enter its agents · t period · g group",
        Panel::Models | Panel::Burn => "t period",
        Panel::Efficiency => "↑↓ page (when it does not fit)",
        Panel::History => "←→ pick a day",
        Panel::System => "",
    }
}

fn footer(f: &mut Frame, app: &App, area: Rect) {
    let th = &app.theme;
    if let Some(cmd) = &app.command {
        let line = Line::from(vec![
            Span::styled(" :", th.bold(th.accent)),
            Span::styled(format!("{cmd}▏"), Style::new().fg(th.fg)),
            Span::styled(
                "   switch <seat> <login> · capture <seat> [label] · period 7d · filter p:shop · layout wall|column|tabs · theme amber",
                Style::new().fg(th.faint),
            ),
        ]);
        f.render_widget(Paragraph::new(line).style(th.base()), area);
        return;
    }
    if app.filter_edit {
        let line = Line::from(vec![
            Span::styled(" /", th.bold(th.accent)),
            Span::styled(
                format!("{}▏", app.filter.clone().unwrap_or_default()),
                Style::new().fg(th.fg),
            ),
            Span::styled(
                "   words match names; p:project m:model s:seat state:needs · Enter keep · Esc clear",
                Style::new().fg(th.faint),
            ),
        ]);
        f.render_widget(Paragraph::new(line).style(th.base()), area);
        return;
    }
    let mut spans = vec![Span::raw(" ")];
    if app.shape == Shape::Tabs && app.zoom.is_none() {
        for (i, (t, name, _)) in TABS.iter().enumerate() {
            let on = *t == app.tab;
            spans.push(Span::styled(
                format!(" {} {name} ", i + 1),
                if on {
                    th.bold(th.bg).bg(th.accent)
                } else {
                    Style::new().fg(th.dim)
                },
            ));
        }
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(
        format!("{} ", app.focus.title()),
        th.bold(th.accent),
    ));
    // Global keys on the right: help and quit always, the rest while there is room; the focused
    // panel's keys take what is left and are cut first.
    let width = area.width as usize;
    let lead: usize = spans.iter().map(|s| w(&s.content)).sum();
    let optional: Vec<&str> = if app.zoom.is_some() {
        vec!["Esc back", "z unzoom"]
    } else if app.shape == Shape::Tabs {
        vec![
            "Tab next panel",
            "1-5 tabs",
            "n next alert",
            "t period",
            ": cmd",
        ]
    } else {
        vec![
            "Tab next panel",
            "1-9 focus (again: zoom)",
            "n next alert",
            "t period",
            ": cmd",
        ]
    };
    let mut right: Vec<&str> = vec!["? help & legend", "q quit"];
    let budget = width.saturating_sub(lead + 34);
    for o in optional {
        let cur = right.iter().map(|x| w(x) + 3).sum::<usize>() + 5;
        if cur + w(o) + 3 <= budget {
            right.insert(right.len() - 2, o);
        }
    }
    let global = format!("  │  {} ", right.join(" · "));
    let keys = crate::ui::table::truncate(
        panel_keys(app.focus),
        width.saturating_sub(lead + w(&global)),
    );
    spans.push(Span::styled(keys, Style::new().fg(th.dim)));
    let used: usize = spans.iter().map(|s| w(&s.content)).sum();
    let g = crate::ui::table::truncate(&global, width.saturating_sub(used));
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used + w(&g)))));
    spans.push(Span::styled(g, Style::new().fg(th.faint)));
    f.render_widget(Paragraph::new(Line::from(spans)).style(th.base()), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

    /// First review defect: `--mask` left the login label made from an email's local part on
    /// screen (it has no `@`). Whatever draws it, the finished frame shows it blanked.
    #[test]
    fn a_label_made_from_an_email_is_masked() {
        let t = tempfile::tempdir().unwrap();
        let login: crate::vault::Login = serde_json::from_value(serde_json::json!({
            "id": "l1", "label": "zelda", "account_uuid": "u1", "email": "zelda@example.com",
            "plan": null, "meta": {"fingerprint": "f", "subscription_type": null, "rate_limit_tier": null,
                "access_expires_at": null, "refresh_expires_at": null, "scopes": []},
            "oauth_account": null, "captured_at": "2026-11-01T00:00:00Z",
            "updated_at": "2026-11-01T00:00:00Z", "mounted": null
        }))
        .unwrap();
        let screen = |mask: bool| -> String {
            let mut app = App::new(Theme::named(None), mask, true, Vec::new(), t.path().into());
            app.boot = None;
            app.vault.logins = vec![login.clone()];
            app.toast(
                "seat 'work' now uses 'zelda-2'",
                crate::tui::app::Tone::Good,
            );
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 40)).unwrap();
            term.draw(|f| draw(f, &mut app)).unwrap();
            let buf = term.backend().buffer().clone();
            buf.content().iter().map(|c| c.symbol()).collect()
        };
        let plain = screen(false);
        assert!(plain.contains("zelda-2"), "the toast is on screen: {plain}");
        let masked = screen(true);
        assert!(!masked.contains("zelda"), "{masked}");
        assert!(masked.contains("•••••-2"));
    }

    /// Panels kept between frames (`draw_panel`) paste exactly what drawing them again gives,
    /// mouse rects included, and are drawn again when the app's state changes.
    #[test]
    fn kept_panels_are_what_drawing_again_gives() {
        let t = tempfile::tempdir().unwrap();
        let frame = |app: &mut App| {
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(326, 81)).unwrap();
            term.draw(|f| draw(f, app)).unwrap();
            // The header's clock may tick between two frames: compare from the second row.
            let cells = term.backend().buffer().content()[326..].to_vec();
            (cells, app.rects.clone())
        };
        let agent = |id: &str, state: State| crate::live::Agent {
            seat: crate::core::ids::SeatId::from_stored("0123456789abcdef"),
            seat_label: "work".into(),
            session_id: crate::core::ids::SessionId::new("s1"),
            agent_id: id.into(),
            parent: (!id.is_empty()).then(String::new),
            depth: u32::from(!id.is_empty()),
            name: format!("agent {id}"),
            agent_type: None,
            project: "shop".into(),
            model: Some("claude-opus-5-5".into()),
            effort: None,
            state,
            since: Utc::now(),
            last_activity: Utc::now(),
            cost: 1.5,
            requests: 3,
            context_tokens: 1000,
            spark: vec![0.1; 15],
            lane: Vec::new(),
            pane: None,
            pid: None,
            account: None,
            first_seen: Utc::now(),
            session_cost: 2.0,
            session_burn: 0.5,
            cpu: 0.0,
            memory: 0,
            last_request_ts: None,
        };
        let new_app = |cache: bool| {
            let mut app = App::new(Theme::named(None), false, true, Vec::new(), t.path().into());
            app.boot = None;
            app.cache_panels = cache;
            // Two apps animate in step on a stopped clock.
            app.t_fixed = Some(1.3);
            app.snapshot.agents = vec![
                agent("", State::Idle),
                agent("a1", State::Thinking),
                agent(
                    "a2",
                    State::Tool {
                        name: "Bash".into(),
                        summary: "ls".into(),
                    },
                ),
                agent("a3", State::NeedsYou("permission: Bash".into())),
            ];
            app
        };
        let (mut kept, mut fresh) = (new_app(true), new_app(false));
        let first = frame(&mut kept);
        assert!(
            kept.shots.contains_key(&Panel::Swarm),
            "spinning and pulsing rows are kept"
        );
        assert!(kept.shots[&Panel::Swarm].marks.len() >= 2);
        assert_eq!(first, frame(&mut fresh));
        // Nothing changed: pasted, and the spinner and the pulse move on.
        for app in [&mut kept, &mut fresh] {
            // The blink flips too (2.6 → 3.6 half seconds).
            app.t_fixed = Some(1.8);
        }
        let later = frame(&mut kept);
        assert_ne!(later.0, first.0);
        assert_eq!(later, frame(&mut fresh));
        assert!(
            kept.shots[&Panel::Swarm].blink.is_some(),
            "a blinking swarm is kept"
        );
        // The focus moved (a key): drawn again, the plates follow.
        for app in [&mut kept, &mut fresh] {
            app.focus = Panel::Projects;
            app.rev += 1;
        }
        let moved = frame(&mut kept);
        assert_ne!(moved.0, first.0);
        assert_eq!(moved, frame(&mut fresh));
    }
}
