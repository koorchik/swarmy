//! Where a Claude Code runs, so `J` and `swarmy jump` can take the user there: a tmux pane or
//! a kitty window. Plain data; finding and focusing it is `claude::pane`.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Pane {
    /// `session:@window.%pane`, as Claude Code records it in its registry.
    Tmux {
        pane: String,
    },
    Kitty {
        window: KittyWindow,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KittyWindow {
    /// `KITTY_WINDOW_ID` of the process.
    pub id: u32,
    /// `KITTY_LISTEN_ON` of its kitty, e.g. `unix:@kitty-1234`; set only with `listen_on`.
    pub socket: Option<String>,
}

impl KittyWindow {
    /// The window from a process environment as `/proc/<pid>/environ` holds it (NUL-separated).
    pub fn from_environ(environ: &[u8]) -> Option<KittyWindow> {
        let mut id = None;
        let mut socket = None;
        for var in environ.split(|b| *b == 0) {
            let var = String::from_utf8_lossy(var);
            if let Some(v) = var.strip_prefix("KITTY_WINDOW_ID=") {
                id = v.parse().ok();
            } else if let Some(v) = var.strip_prefix("KITTY_LISTEN_ON=") {
                socket = Some(v.to_string()).filter(|s| !s.is_empty());
            }
        }
        id.map(|id| KittyWindow { id, socket })
    }
}

impl Pane {
    /// The terminal it lives in, for labels.
    pub fn terminal(&self) -> &'static str {
        match self {
            Pane::Tmux { .. } => "tmux",
            Pane::Kitty { .. } => "kitty",
        }
    }

    /// Where it is, without the terminal's name.
    pub fn place(&self) -> String {
        match self {
            Pane::Tmux { pane } => pane.clone(),
            Pane::Kitty { window } => format!("window {}", window.id),
        }
    }
}

impl fmt::Display for Pane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.terminal(), self.place())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_window_and_socket_from_the_environment() {
        let env =
            b"PATH=/usr/bin\0KITTY_WINDOW_ID=7\0KITTY_LISTEN_ON=unix:@kitty-42\0TERM=xterm-kitty\0";
        assert_eq!(
            KittyWindow::from_environ(env),
            Some(KittyWindow {
                id: 7,
                socket: Some("unix:@kitty-42".into())
            })
        );
    }

    #[test]
    fn a_window_without_listen_on_has_no_socket() {
        assert_eq!(
            KittyWindow::from_environ(b"KITTY_WINDOW_ID=3\0"),
            Some(KittyWindow {
                id: 3,
                socket: None
            })
        );
    }

    #[test]
    fn outside_kitty_there_is_no_window() {
        assert_eq!(
            KittyWindow::from_environ(b"TERM=xterm-256color\0KITTY_LISTEN_ON=unix:@x\0"),
            None
        );
        assert_eq!(KittyWindow::from_environ(b"KITTY_WINDOW_ID=\0"), None);
    }

    #[test]
    fn labels() {
        assert_eq!(
            Pane::Tmux {
                pane: "0:@1.%2".into()
            }
            .to_string(),
            "tmux 0:@1.%2"
        );
        let kitty = Pane::Kitty {
            window: KittyWindow {
                id: 4,
                socket: None,
            },
        };
        assert_eq!(kitty.to_string(), "kitty window 4");
        assert_eq!(
            (kitty.terminal(), kitty.place().as_str()),
            ("kitty", "window 4")
        );
    }

    #[test]
    fn round_trips_through_the_fleet_summary() {
        let pane = Pane::Kitty {
            window: KittyWindow {
                id: 4,
                socket: Some("unix:@k".into()),
            },
        };
        let json = serde_json::to_string(&pane).unwrap();
        assert_eq!(serde_json::from_str::<Pane>(&json).unwrap(), pane);
    }
}
