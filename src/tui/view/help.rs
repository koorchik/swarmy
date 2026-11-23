//! The help overlay: keys of the focused panel and everywhere, and the legend.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::kit::*;
use super::overlays::{centered, dialog};
use super::panel_keys;
use crate::tui::app::{App, Shape};

pub(super) fn help(f: &mut Frame, app: &App, area: Rect) {
    let th = &app.theme;
    let tabs = app.shape == Shape::Tabs;
    let mut rows: Vec<(String, String)> =
        vec![(format!("— {} —", app.focus.title()), String::new())];
    for part in panel_keys(app.focus).split(" · ") {
        if let Some((k, d)) = part.split_once(' ') {
            rows.push((k.to_string(), d.to_string()));
        }
    }
    rows.push(("— everywhere —".into(), String::new()));
    let focus_keys = if tabs {
        (
            "1-5",
            "switch tab (OPS · AGENT · LIMITS · SPEND · SYSTEM); Tab moves between its panels",
        )
    } else {
        (
            "1-9, 0",
            "focus the panel with that number (in its title); again = zoom; 0 = SYSTEM",
        )
    };
    for (k, d) in [
        (
            "Tab · Shift-Tab",
            "next / previous panel (the focused one has a bright border)",
        ),
        focus_keys,
        (
            "z · Esc",
            "zoom the focused panel · back (closes dialogs, zoom, filter)",
        ),
        (
            "n · N",
            "next / previous thing that needs attention (selects the agent)",
        ),
        (
            "/",
            "filter agents: words, p:project m:model s:seat state:needs",
        ),
        (
            ":",
            "switch <seat> <login> · capture <seat> · period 7d · layout wall|column|tabs",
        ),
        ("t · g", "period (today · 7d · 30d · all) · spend grouping"),
        (
            "J",
            "jump to the selected agent's Claude Code (tmux, zellij, kitty, wezterm, iTerm2; outside: `swarmy jump`)",
        ),
        ("v", "SWARM: activity lanes of 30 min ↔ spend of 15 min"),
        ("L", "layout: auto · wall · column · tabs"),
        ("m · p · T", "mask names · pause animations · theme"),
        (
            "mouse",
            "click focuses a panel / selects an agent; wheel scrolls",
        ),
        (
            "? · q",
            if tabs {
                "legend, then close · quit"
            } else {
                "close help · quit"
            },
        ),
    ] {
        rows.push((k.into(), d.into()));
    }
    rows.push(("— vault (panel 2) —".into(), String::new()));
    for (k, d) in [
        (
            "Enter",
            "on a seat: pick a saved login to put in · on a saved login: pick a seat",
        ),
        (
            "S",
            "put the saved login with the most limit left into the seat",
        ),
        (
            "c · a",
            "save the seat's current login · add a new login (runs claude /login)",
        ),
        (
            "r · d · R",
            "rename · forget a saved login · re-read logins from disk",
        ),
        (
            "",
            "a switch saves the seat's current login first and backs up both files;",
        ),
        ("", "running sessions keep their old login until restarted"),
    ] {
        rows.push((k.into(), d.into()));
    }
    let keys: Vec<Line> = rows
        .into_iter()
        .map(|(k, d)| {
            if d.is_empty() {
                Line::from(Span::styled(k, th.bold(th.dim)))
            } else {
                Line::from(vec![
                    Span::styled(fit(&k, 16), th.bold(th.accent)),
                    Span::styled(d, Style::new().fg(th.fg)),
                ])
            }
        })
        .collect();
    let legend = legend_lines(app);
    let wide = area.width >= 170;
    let height = if wide {
        keys.len().max(legend.len())
    } else if app.help_page == 0 {
        keys.len()
    } else {
        legend.len()
    };
    let width = if wide {
        area.width.saturating_sub(6).min(186)
    } else {
        104
    };
    let r = centered(area, width, height as u16 + 2);
    f.render_widget(Clear, r);
    let title = if wide {
        "KEYS · LEGEND"
    } else if app.help_page == 0 {
        "KEYS · Tab: legend"
    } else {
        "LEGEND · Tab: keys"
    };
    let block = dialog(app, title);
    let inner = block.inner(r);
    f.render_widget(block, r);
    if wide {
        let c = hsplit(inner, &[50, 50]);
        f.render_widget(Paragraph::new(keys), c[0]);
        f.render_widget(
            Paragraph::new(legend),
            Rect {
                x: c[1].x + 2,
                width: c[1].width.saturating_sub(2),
                ..c[1]
            },
        );
    } else if app.help_page == 0 {
        f.render_widget(Paragraph::new(keys), inner);
    } else {
        f.render_widget(Paragraph::new(legend), inner);
    }
}

/// What the glyphs, numbers, colours and words on the screen mean.
fn legend_lines(app: &App) -> Vec<Line<'static>> {
    let th = &app.theme;
    let head = |t: &str| Line::from(Span::styled(format!("— {t} —"), th.bold(th.dim)));
    let row = |g: &str, c: ratatui::style::Color, d: &str| {
        Line::from(vec![
            Span::styled(fit(g, 16), th.bold(c)),
            Span::styled(d.to_string(), Style::new().fg(th.fg)),
        ])
    };
    vec![
        head("agent states"),
        row("●", th.good, "working: running a tool"),
        row(
            "⠋ ◌",
            th.thinking,
            "thinking: waiting for the model's answer",
        ),
        row("◐", th.accent, "waiting for its own agents"),
        row(
            "▲",
            th.warn,
            "needs you: a question, a permission, a plan to approve",
        ),
        row("✖", th.bad, "stalled: no activity for a while"),
        row("○ ✓ ·", th.dim, "idle · done · exited"),
        row(
            "▃ ▅ ▆ █",
            th.accent,
            "SWARM lanes, a minute each: thinking · tool · agents · you",
        ),
        head("alerts and events"),
        row("⊘", th.warn, "a weekly or 5-hour limit is close or reached"),
        row(
            "⚠",
            th.warn,
            "two agents edited the same file within the hour",
        ),
        row(
            "⚿",
            th.accent,
            "logins: unsaved, unknown, expiring (the vault)",
        ),
        row(
            "+ ✓ ✎ ◆ !",
            th.dim,
            "spawned · finished · your prompt · online · API error",
        ),
        head("numbers"),
        row(
            "$",
            th.fg,
            "API-equivalent cost at list prices (not what a plan bills)",
        ),
        row(
            "Σ",
            th.fg,
            "the whole session: main thread and all its agents",
        ),
        row(
            "week used",
            th.fg,
            "share of the account's weekly limit; 1% ≈ $X: API dollars per 1%",
        ),
        row(
            "≈",
            th.fg,
            "an estimate between Claude Code's limit readings",
        ),
        row(
            "▲ ▼",
            th.warn,
            "change against the previous period (HISTORY: the day before)",
        ),
        row(
            "lo md hi xh mx",
            th.fg,
            "effort: low · medium · high · xhigh · max",
        ),
        row(
            "CONTEXT",
            th.fg,
            "share of the model's context window in use",
        ),
        row(
            "IN STATE",
            th.fg,
            "how long the agent has been in its current state",
        ),
        row(
            "index · 12 mo ≈",
            th.fg,
            "swarmy's own database · growth a day · size in a year (warning colour: over budget)",
        ),
        head("colours"),
        Line::from(vec![
            Span::styled(fit("limit", 16), th.bold(th.fg)),
            Span::styled("below 60% ", Style::new().fg(th.good)),
            Span::styled("60–85% ", Style::new().fg(th.warn)),
            Span::styled("above 85%", Style::new().fg(th.bad)),
        ]),
        Line::from(vec![
            Span::styled(fit("models", 16), th.bold(th.fg)),
            Span::styled("fable ", Style::new().fg(th.fable)),
            Span::styled("opus ", Style::new().fg(th.opus)),
            Span::styled("sonnet ", Style::new().fg(th.sonnet)),
            Span::styled("haiku", Style::new().fg(th.haiku)),
            Span::styled(" (the same in every panel)", Style::new().fg(th.dim)),
        ]),
        row("project", th.dir, "project and directory names"),
        row(
            "● ○ ECO",
            th.good,
            "header: data fresh / stale · on battery, refreshes less often",
        ),
        row(
            "│ now",
            th.faint,
            "the limit chart: the vertical line is now, ⊘ where the projection hits 100%",
        ),
        head("words"),
        row(
            "seat",
            th.fg,
            "a Claude Code config dir (~/.claude, ~/.claude-work…)",
        ),
        row(
            "login",
            th.fg,
            "an OAuth sign-in; saved ones live in the vault",
        ),
        row(
            "account",
            th.fg,
            "the Anthropic account behind a login; limits are per account",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

    /// Words the panels put on screen have their legend line (CLAUDE.md: keep them in sync).
    #[test]
    fn the_legend_explains_the_storage_line() {
        let t = tempfile::tempdir().unwrap();
        let app = App::new(Theme::named(None), false, true, Vec::new(), t.path().into());
        let text: Vec<String> = legend_lines(&app)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let line = text.iter().find(|l| l.starts_with("index · 12 mo ≈"));
        assert!(
            line.is_some_and(|l| l.contains("size in a year") && l.contains("over budget")),
            "{text:#?}"
        );
    }
}
