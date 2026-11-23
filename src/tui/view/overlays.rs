//! Overlays: the vault's picker and dialogs, and the toast.

use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use super::kit::*;
use crate::tui::app::App;
use crate::tui::vault_ui::{Modal, PickFor};
use crate::ui::fmt;

pub(super) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let wdt = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - wdt) / 2,
        y: area.y + (area.height - h) / 2,
        width: wdt,
        height: h,
    }
}

pub(super) fn dialog<'a>(app: &App, title: &'a str) -> Block<'a> {
    let th = &app.theme;
    Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(Style::new().fg(th.accent))
        .title(Line::from(Span::styled(
            format!(" {title} "),
            th.bold(th.accent),
        )))
        .style(th.base())
}

pub(super) fn picker(f: &mut Frame, app: &App, area: Rect) {
    let Some(p) = &app.vault.picker else { return };
    let th = &app.theme;
    let title = match &p.what {
        PickFor::Seat(s) => format!("put a saved login into seat '{s}' · most limit left first"),
        PickFor::Login(l) => format!("put login '{l}' into a seat"),
    };
    let r = centered(area, 96, p.items.len() as u16 + 6);
    f.render_widget(Clear, r);
    let block = dialog(app, &title);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let now = Utc::now();
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "  {} {} {} {} {}",
            fit("LOGIN / SEAT", 20),
            fit("ACCOUNT", 22),
            fit("5-HOUR", 13),
            fit("WEEK", 13),
            "WEEK RESETS · LOGIN EXPIRES"
        ),
        Style::new().fg(th.faint),
    ))];
    for (i, item) in p.items.iter().enumerate() {
        let account = match &p.what {
            PickFor::Seat(_) => app
                .vault
                .logins
                .iter()
                .find(|l| &l.label == item)
                .and_then(|l| l.account_uuid.clone()),
            PickFor::Login(_) => app
                .analytics
                .seats
                .iter()
                .find(|s| &s.label == item)
                .and_then(|s| s.account.clone()),
        };
        let (five, week, reset) = app.limits_of(account.as_deref());
        let acct = account
            .as_deref()
            .map(|a| mask_name(app, &app.account_label(a)))
            .unwrap_or_else(|| "(unknown)".into());
        let pc = |v: Option<f64>| -> Vec<Span<'static>> {
            match v {
                Some(x) => {
                    let mut g = gauge(x / 100.0, 7, th.utilization(x), th);
                    g.push(Span::styled(
                        fit_right(&fmt::pct(x), 5),
                        th.bold(th.utilization(x)),
                    ));
                    g.push(Span::raw(" "));
                    g
                }
                None => vec![Span::styled(
                    fit("  unknown", 13),
                    Style::new().fg(th.faint),
                )],
            }
        };
        let expires = app
            .vault
            .logins
            .iter()
            .find(|l| &l.label == item)
            .and_then(|l| l.meta.refresh_expires_at)
            .map(|t| format!(" · exp. {}", fmt::duration(t - now)))
            .unwrap_or_default();
        let mut sp = vec![
            Span::styled(if i == p.sel { "▸ " } else { "  " }, th.bold(th.accent)),
            Span::styled(fit(item, 20), th.bold(th.fg)),
            Span::styled(format!(" {}", fit(&acct, 22)), Style::new().fg(th.dim)),
            Span::raw(" "),
        ];
        if five.is_none() && week.is_none() {
            // Nothing known yet: say why instead of two "unknown" columns.
            sp.push(Span::styled(
                fit("limit unknown until this login is used", 26),
                Style::new().fg(th.faint),
            ));
        } else {
            sp.extend(pc(five));
            sp.extend(pc(week));
        }
        sp.push(Span::styled(
            format!(
                "{}{expires}",
                reset.map(|r| fmt::duration(r - now)).unwrap_or_default()
            ),
            Style::new().fg(th.dim),
        ));
        let mut line = Line::from(sp);
        if i == p.sel {
            line = line.style(Style::new().bg(th.sel));
        }
        lines.push(line);
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑↓ choose · Enter switch (asks first) · Esc cancel",
        Style::new().fg(th.faint),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn modal(f: &mut Frame, app: &App, area: Rect) {
    let Some(m) = &app.vault.modal else { return };
    let th = &app.theme;
    let (title, body, input) = match m {
        Modal::Input { title, value, .. } => ("INPUT", title.clone(), Some(value.clone())),
        Modal::Confirm { text, .. } => ("CONFIRM", text.clone(), None),
    };
    let wdt = 84u16;
    let lines_n: usize = body.lines().map(|l| w(l) / (wdt as usize - 4) + 1).sum();
    let r = centered(
        area,
        wdt,
        lines_n as u16 + if input.is_some() { 5 } else { 3 },
    );
    f.render_widget(Clear, r);
    let block = dialog(app, title);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let mut lines: Vec<Line> = body
        .lines()
        .map(|l| Line::from(Span::styled(l.to_string(), Style::new().fg(th.fg))))
        .collect();
    if let Some(val) = input {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("› ", th.bold(th.accent)),
            Span::styled(val, th.bold(th.fg)),
            Span::styled(
                if blink(app) { "▏" } else { " " },
                Style::new().fg(th.accent),
            ),
            Span::styled("   Enter ok · Esc cancel", Style::new().fg(th.faint)),
        ]));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

pub(super) fn toast(f: &mut Frame, app: &App, area: Rect) {
    let th = &app.theme;
    let msg = if let Some(e) = &app.error {
        Some((format!("error: {e}"), th.bad))
    } else {
        app.toast
            .as_ref()
            .filter(|(_, _, t)| t.elapsed().as_secs_f32() < 4.0)
            .map(|(m, tone, _)| (m.clone(), app.tone_color(*tone)))
    };
    let Some((m, c)) = msg else { return };
    let wdt = (w(&m) as u16 + 4).min(area.width);
    let inner_w = wdt.saturating_sub(4).max(1) as usize;
    let lines_n = w(&m).div_ceil(inner_w).clamp(1, 3) as u16;
    let height = (lines_n + 2).min(area.height);
    let r = Rect {
        x: area.x + area.width.saturating_sub(wdt),
        y: area.y + area.height.saturating_sub(height + 1),
        width: wdt,
        height,
    };
    f.render_widget(Clear, r);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(c))
        .style(th.base());
    let inner = block.inner(r);
    f.render_widget(block, r);
    f.render_widget(
        Paragraph::new(Span::styled(
            m,
            Style::new().fg(th.fg).add_modifier(Modifier::BOLD),
        ))
        .wrap(Wrap { trim: true }),
        inner,
    );
}
