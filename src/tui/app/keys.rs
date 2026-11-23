//! Keys, the mouse and `:` commands.

use super::*;

const PERIODS: [&str; 4] = ["today", "7d", "30d", "all"];
const GROUPS: [Dim; 6] = [
    Dim::Project,
    Dim::Model,
    Dim::AgentType,
    Dim::Account,
    Dim::Seat,
    Dim::Session,
];

impl App {
    // ───────────── keys ─────────────

    /// Returns true to quit.
    pub fn on_key(&mut self, k: KeyEvent) -> bool {
        if let Some(b) = &mut self.boot {
            b.skip = true;
            return k.code == KeyCode::Char('q');
        }
        if let Some(cmd) = &mut self.command {
            match k.code {
                KeyCode::Esc => self.command = None,
                KeyCode::Enter => {
                    let c = std::mem::take(cmd);
                    self.command = None;
                    return self.run_command(&c);
                }
                KeyCode::Backspace => {
                    cmd.pop();
                }
                KeyCode::Char(c) => cmd.push(c),
                _ => {}
            }
            return false;
        }
        if self.filter_edit {
            let f = self.filter.get_or_insert_with(String::new);
            match k.code {
                KeyCode::Esc => {
                    self.filter = None;
                    self.filter_edit = false;
                }
                KeyCode::Enter => self.filter_edit = false,
                KeyCode::Backspace => {
                    f.pop();
                }
                KeyCode::Char(c) => f.push(c),
                _ => {}
            }
            return false;
        }
        if self.help {
            match k.code {
                KeyCode::Tab | KeyCode::Left | KeyCode::Right => self.help_page ^= 1,
                // `?` again: the legend on small screens, then close.
                KeyCode::Char('?') if self.help_page == 0 && self.shape == Shape::Tabs => {
                    self.help_page = 1
                }
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                    self.help = false;
                    self.help_page = 0;
                }
                _ => {}
            }
            return false;
        }
        // Dialogs of the vault take every key.
        if (self.vault.modal.is_some() || self.vault.picker.is_some()) && self.vault_key(k) {
            return false;
        }
        if self.global_key(k) {
            return true;
        }
        false
    }

    /// Returns true to quit.
    fn global_key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('c') if ctrl => return true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(':') => self.command = Some(String::new()),
            KeyCode::Char('/') => {
                self.filter_edit = true;
                self.filter.get_or_insert_with(String::new);
            }
            KeyCode::Tab => self.cycle_focus(1),
            KeyCode::BackTab => self.cycle_focus(-1),
            KeyCode::Char(c @ '0'..='9')
                if self.shape == Shape::Tabs && ('1'..='5').contains(&c) && self.zoom.is_none() =>
            {
                let (t, _, p) = TABS[c as usize - '1' as usize];
                self.tab = t;
                self.focus = p;
            }
            KeyCode::Char(c @ '0'..='9') => {
                let p = Panel::from_number(c as u8 - b'0');
                if self.focus == p && self.visible_panels().len() > 1 {
                    self.zoom = Some(p);
                } else {
                    self.focus_panel(p);
                }
            }
            KeyCode::F(n) if n <= 10 => {
                let p = Panel::from_number(n % 10);
                self.focus_panel(p);
            }
            KeyCode::Char('z') => {
                self.zoom = if self.zoom.is_some() {
                    None
                } else {
                    Some(self.focus)
                }
            }
            KeyCode::Esc => {
                if self.zoom.is_some() {
                    self.zoom = None;
                } else if self.filter.is_some() {
                    self.filter = None;
                } else if self.shape == Shape::Tabs {
                    self.tab = Tab::Ops;
                    self.focus = Panel::Swarm;
                }
            }
            KeyCode::Char('n') => self.jump_attention(1),
            KeyCode::Char('N') => self.jump_attention(-1),
            KeyCode::Char('J') => self.jump(),
            KeyCode::Char('v') => {
                self.lanes = !self.lanes;
                self.toast(
                    if self.lanes {
                        "SWARM: activity of the last 30 min (thinking · tool · waiting for agents · waiting for you)"
                    } else {
                        "SWARM: spend of the last 15 min"
                    },
                    Tone::Info,
                );
            }
            KeyCode::Char('m') => {
                self.mask = !self.mask;
                self.toast(
                    if self.mask {
                        "mask on: names, emails and paths hidden"
                    } else {
                        "mask off"
                    },
                    Tone::Info,
                );
            }
            KeyCode::Char('p') => {
                self.paused = !self.paused;
                self.toast(
                    if self.paused {
                        "animations paused"
                    } else {
                        "animations on"
                    },
                    Tone::Info,
                );
            }
            KeyCode::Char('T') => {
                self.theme = self.theme.next();
                self.toast(format!("theme: {}", self.theme.name.as_str()), Tone::Info);
            }
            KeyCode::Char('L') => {
                self.layout = match self.layout {
                    LayoutMode::Auto => LayoutMode::Wall,
                    LayoutMode::Wall => LayoutMode::Column,
                    LayoutMode::Column => LayoutMode::Tabs,
                    LayoutMode::Tabs => LayoutMode::Auto,
                };
                self.zoom = None;
                self.toast(
                    format!("layout: {:?}", self.layout).to_lowercase(),
                    Tone::Info,
                );
            }
            KeyCode::Char('t') => {
                let cur = PERIODS
                    .iter()
                    .position(|p| *p == period_key(&self.period))
                    .unwrap_or(1);
                self.period = PERIODS[(cur + 1) % PERIODS.len()]
                    .parse()
                    .unwrap_or(Period::Days(7));
                self.request_analytics();
                self.toast(format!("period: {}", self.period.label()), Tone::Info);
            }
            KeyCode::Char('g') => {
                let cur = GROUPS.iter().position(|g| *g == self.group).unwrap_or(0);
                self.group = GROUPS[(cur + 1) % GROUPS.len()];
                self.request_analytics();
                self.toast(
                    format!("spend grouped by {}", self.group.title().to_lowercase()),
                    Tone::Info,
                );
            }
            _ => self.panel_key(k),
        }
        false
    }

    fn list_len(&self, p: Panel) -> usize {
        match p {
            // Alerts, then the event log under them.
            Panel::Attention => self.attention().len() + self.events.len(),
            // Pages of the efficiency panel.
            Panel::Efficiency => 3,
            // Days of the daily bars (0 = today).
            Panel::History => 14,
            Panel::Projects => self.analytics.projects.len(),
            Panel::Agent => self.timeline.1.len(),
            _ => 0,
        }
    }

    fn panel_key(&mut self, k: KeyEvent) {
        let step: isize = match k.code {
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::PageDown => 10,
            KeyCode::PageUp => -10,
            KeyCode::Home => -100_000,
            KeyCode::End => 100_000,
            _ => 0,
        };
        match self.focus {
            Panel::Swarm => {
                if step != 0 {
                    self.move_agent(step);
                } else if k.code == KeyCode::Enter {
                    self.focus_panel(Panel::Agent);
                }
            }
            Panel::Limits => {
                self.vault_key(k);
            }
            Panel::Attention if k.code == KeyCode::Enter => {
                let items = self.attention();
                if let Some(a) = items.get(self.cur(Panel::Attention)).cloned() {
                    self.act_on_attention(&a);
                }
            }
            Panel::Projects if k.code == KeyCode::Enter => {
                if let Some(p) = self.analytics.projects.get(self.cur(Panel::Projects)) {
                    self.filter = Some(format!("p:{}", p.name.to_lowercase()));
                    self.toast(format!("agents of {} (Esc clears)", p.name), Tone::Info);
                    self.focus_panel(Panel::Swarm);
                }
            }
            Panel::History
                if matches!(
                    k.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l')
                ) =>
            {
                let step = if matches!(k.code, KeyCode::Left | KeyCode::Char('h')) {
                    1
                } else {
                    -1
                };
                let v = (self.cur(Panel::History) as isize + step).clamp(0, 13) as usize;
                self.set_cur(Panel::History, v);
            }
            p @ (Panel::Attention
            | Panel::Projects
            | Panel::Efficiency
            | Panel::Agent
            | Panel::History)
                if step != 0 =>
            {
                let n = self.list_len(p).max(1);
                let v = (self.cur(p) as isize + step).clamp(0, n as isize - 1) as usize;
                self.set_cur(p, v);
            }
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        let hit = self
            .rects
            .iter()
            .find(|(_, r)| {
                m.column >= r.x
                    && m.column < r.x + r.width
                    && m.row >= r.y
                    && m.row < r.y + r.height
            })
            .map(|(p, r)| (*p, *r));
        let Some((p, r)) = hit else { return };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.focus = p;
                // A click on an agent row selects it (rows start two lines below the border).
                if p == Panel::Swarm {
                    let row = m.row.saturating_sub(r.y + 2) as usize;
                    let keys: Vec<String> = self.visible_agents().iter().map(|a| a.key()).collect();
                    let first = self.swarm_offset(r.height.saturating_sub(3) as usize);
                    if let Some(k) = keys.get(first + row) {
                        self.selected_key = Some(k.clone());
                    }
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let prev = self.focus;
                self.focus = p;
                let code = if m.kind == MouseEventKind::ScrollDown {
                    KeyCode::Down
                } else {
                    KeyCode::Up
                };
                self.panel_key(KeyEvent::new(code, KeyModifiers::NONE));
                self.focus = prev;
            }
            _ => {}
        }
    }

    /// First visible row of the agent list for a list `rows` tall (keeps the selection on screen).
    pub fn swarm_offset(&self, rows: usize) -> usize {
        let sel = self.selected_index();
        if rows == 0 || sel < rows {
            0
        } else {
            sel + 1 - rows
        }
    }

    /// Take the user to the tmux pane or kitty window of the selected agent's Claude Code.
    fn jump(&mut self) {
        let Some(a) = self.selected_agent() else {
            return;
        };
        let main = self
            .snapshot
            .agents
            .iter()
            .find(|x| x.is_main() && x.seat == a.seat && x.session_id == a.session_id);
        let Some(pane) = main.and_then(|m| m.pane.clone()).or_else(|| a.pane.clone()) else {
            self.toast(
                "this session is not running inside tmux or kitty",
                Tone::Warn,
            );
            return;
        };
        match crate::claude::pane::jump(&pane) {
            crate::claude::pane::Jump::Done => self.toast(format!("jumped to {pane}"), Tone::Good),
            crate::claude::pane::Jump::Unreachable(why) => self.toast(why, Tone::Warn),
            crate::claude::pane::Jump::Failed(why) => self.toast(why, Tone::Bad),
        }
    }

    fn run_command(&mut self, line: &str) -> bool {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["q"] | ["quit"] => return true,
            ["period", p] => match p.parse::<Period>() {
                Ok(p) => {
                    self.period = p;
                    self.request_analytics();
                }
                Err(e) => self.toast(e, Tone::Bad),
            },
            ["theme", t] => match t.parse() {
                Ok(name) => self.theme = Theme::named(Some(name)),
                Err(e) => self.toast(e, Tone::Bad),
            },
            ["layout", l] => {
                self.layout = match *l {
                    "wall" => LayoutMode::Wall,
                    "column" => LayoutMode::Column,
                    "tabs" => LayoutMode::Tabs,
                    _ => LayoutMode::Auto,
                }
            }
            ["filter", rest @ ..] => self.filter = Some(rest.join(" ")),
            ["capture", seat] | ["capture", seat, _] => {
                let label = parts.get(2).copied();
                let res = self.with_vault(|v, seats| {
                    let s = seats.iter().find(|s| s.label == *seat).ok_or_else(|| anyhow::anyhow!("no seat '{seat}'"))?;
                    v.capture(s, label)?.ok_or_else(|| anyhow::anyhow!("seat '{seat}' has no login"))
                });
                match res {
                    Ok(_) => self.toast(format!("saved the login of seat '{seat}'"), Tone::Good),
                    Err(e) => self.toast(format!("{e:#}"), Tone::Bad),
                }
                self.vault_reload();
                self.request_analytics();
            }
            ["switch", seat, login] => self.request_switch(seat.to_string(), login.to_string()),
            [] => {}
            _ => self.toast("commands: switch <seat> <login> · capture <seat> [label] · period 7d · filter p:shop · layout wall|column|tabs · theme <name> · q", Tone::Info),
        }
        false
    }

    fn with_vault<T>(
        &mut self,
        f: impl FnOnce(&mut VaultTx<'_>, &[Seat]) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let seats = &self.seats;
        Vault::open(&self.vault_dir)?.edit(|tx| {
            tx.sync(seats)?;
            f(tx, seats)
        })
    }
}

fn period_key(p: &Period) -> &'static str {
    match p {
        Period::Today => "today",
        Period::Days(7) => "7d",
        Period::Days(30) => "30d",
        Period::All => "all",
        _ => "7d",
    }
}
