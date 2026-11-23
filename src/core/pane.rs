//! Where a Claude Code runs, so `J` and `swarmy jump` can take the user there: a tmux pane,
//! a Zellij pane, a kitty window, a WezTerm pane, or an iTerm2 session. Plain data; finding and
//! focusing it is `claude::pane`.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::ids::short;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Pane {
    /// `session:@window.%pane`, as Claude Code records it in its registry.
    Tmux { pane: String },
    /// `ZELLIJ_SESSION_NAME` + `ZELLIJ_PANE_ID`.
    Zellij { session: String, pane_id: String },
    /// `KITTY_WINDOW_ID` (with optional `KITTY_LISTEN_ON` and `KITTY_PID`).
    Kitty { window: KittyWindow },
    /// `WEZTERM_PANE` (with optional `WEZTERM_UNIX_SOCKET`).
    WezTerm {
        pane_id: u32,
        #[serde(default)]
        socket: Option<String>,
    },
    /// iTerm2 session unique ID (extracted from `ITERM_SESSION_ID`).
    ITerm2 { session_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KittyWindow {
    /// `KITTY_WINDOW_ID` of the process.
    pub id: u32,
    /// `KITTY_LISTEN_ON` of its kitty, e.g. `unix:@kitty-1234`; set only with `listen_on`.
    pub socket: Option<String>,
    /// `KITTY_PID` of its kitty OS process, so a no-socket jump never targets another kitty.
    #[serde(default)]
    pub kitty_pid: Option<u32>,
}

impl Pane {
    /// Detect a terminal or multiplexer pane from `(key, value)` environment pairs.
    ///
    /// Precedence follows nesting (innermost multiplexer wins over outer terminal):
    /// Zellij -> Kitty -> WezTerm -> iTerm2. (`tmux` is checked first from Claude Code's registry.)
    pub fn from_environ_pairs<'a, I>(vars: I) -> Option<Pane>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut zellij_session: Option<String> = None;
        let mut zellij_pane: Option<String> = None;
        let mut kitty_id: Option<u32> = None;
        let mut kitty_socket: Option<String> = None;
        let mut kitty_pid: Option<u32> = None;
        let mut wezterm_pane: Option<u32> = None;
        let mut wezterm_socket: Option<String> = None;
        let mut iterm_session: Option<String> = None;

        for (k, v) in vars {
            if v.is_empty() {
                continue;
            }
            if k == "ZELLIJ_SESSION_NAME" {
                zellij_session = Some(v.to_string());
            } else if k == "ZELLIJ_PANE_ID" {
                zellij_pane = Some(v.to_string());
            } else if k == "KITTY_WINDOW_ID" {
                kitty_id = v.parse().ok();
            } else if k == "KITTY_LISTEN_ON" {
                kitty_socket = Some(v.to_string());
            } else if k == "KITTY_PID" {
                kitty_pid = v.parse().ok();
            } else if k == "WEZTERM_PANE" {
                wezterm_pane = v.parse().ok();
            } else if k == "WEZTERM_UNIX_SOCKET" {
                wezterm_socket = Some(v.to_string());
            } else if k == "ITERM_SESSION_ID" {
                // `ITERM_SESSION_ID` looks like `w0t0p0:UUID`; iTerm2's AppleScript `unique ID`
                // matches the UUID part after `:`.
                let id = match v.split_once(':') {
                    Some((_, rhs)) if !rhs.is_empty() => rhs,
                    Some(_) | None => v,
                };
                iterm_session = Some(id.to_string());
            }
        }

        if let (Some(session), Some(pane_id)) = (zellij_session, zellij_pane) {
            return Some(Pane::Zellij { session, pane_id });
        }
        if let Some(id) = kitty_id {
            return Some(Pane::Kitty {
                window: KittyWindow {
                    id,
                    socket: kitty_socket,
                    kitty_pid,
                },
            });
        }
        if let Some(pane_id) = wezterm_pane {
            return Some(Pane::WezTerm {
                pane_id,
                socket: wezterm_socket,
            });
        }
        iterm_session.map(|session_id| Pane::ITerm2 { session_id })
    }

    /// Detect a pane from `/proc/<pid>/environ` bytes (NUL-separated `KEY=VALUE` entries).
    pub fn from_proc_environ(environ: &[u8]) -> Option<Pane> {
        let decoded: Vec<String> = environ
            .split(|b| *b == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| String::from_utf8_lossy(entry).into_owned())
            .collect();
        Self::from_environ_pairs(decoded.iter().filter_map(|entry| entry.split_once('=')))
    }

    /// The terminal or multiplexer it lives in, for labels.
    pub fn terminal(&self) -> &'static str {
        match self {
            Pane::Tmux { .. } => "tmux",
            Pane::Zellij { .. } => "zellij",
            Pane::Kitty { .. } => "kitty",
            Pane::WezTerm { .. } => "wezterm",
            Pane::ITerm2 { .. } => "iTerm2",
        }
    }

    /// Where it is, without the terminal's name.
    pub fn place(&self) -> String {
        match self {
            Pane::Tmux { pane } => pane.clone(),
            Pane::Zellij { session, pane_id } => format!("{session} · pane {pane_id}"),
            Pane::Kitty { window } => format!("window {}", window.id),
            Pane::WezTerm { pane_id, .. } => format!("pane {pane_id}"),
            Pane::ITerm2 { session_id } => format!("session {}", short(session_id)),
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
    fn reads_kitty_window_socket_and_pid_from_proc_environ() {
        let env = b"PATH=/usr/bin\0KITTY_WINDOW_ID=7\0KITTY_LISTEN_ON=unix:@kitty-42\0KITTY_PID=42\0TERM=xterm-kitty\0";
        assert_eq!(
            Pane::from_proc_environ(env),
            Some(Pane::Kitty {
                window: KittyWindow {
                    id: 7,
                    socket: Some("unix:@kitty-42".into()),
                    kitty_pid: Some(42),
                },
            })
        );
    }

    #[test]
    fn kitty_without_listen_on_has_no_socket() {
        assert_eq!(
            Pane::from_proc_environ(b"KITTY_WINDOW_ID=3\0KITTY_PID=99\0"),
            Some(Pane::Kitty {
                window: KittyWindow {
                    id: 3,
                    socket: None,
                    kitty_pid: Some(99),
                },
            })
        );
    }

    #[test]
    fn reads_wezterm_pane_and_socket() {
        let env = b"WEZTERM_PANE=12\0WEZTERM_UNIX_SOCKET=/run/user/1000/wezterm/gui-sock-1\0";
        assert_eq!(
            Pane::from_proc_environ(env),
            Some(Pane::WezTerm {
                pane_id: 12,
                socket: Some("/run/user/1000/wezterm/gui-sock-1".into()),
            })
        );
        assert_eq!(
            Pane::from_proc_environ(b"WEZTERM_PANE=0\0"),
            Some(Pane::WezTerm {
                pane_id: 0,
                socket: None,
            })
        );
    }

    #[test]
    fn reads_zellij_session_and_pane() {
        let env = b"ZELLIJ_SESSION_NAME=dev\0ZELLIJ_PANE_ID=3\0";
        assert_eq!(
            Pane::from_proc_environ(env),
            Some(Pane::Zellij {
                session: "dev".into(),
                pane_id: "3".into(),
            })
        );
        // Incomplete Zellij env is ignored.
        assert_eq!(Pane::from_proc_environ(b"ZELLIJ_SESSION_NAME=dev\0"), None);
    }

    #[test]
    fn reads_iterm2_session_uuid() {
        let env = b"ITERM_SESSION_ID=w0t1p2:11223344-5566-7788-99AA-BBCCDDEEFF00\0";
        assert_eq!(
            Pane::from_proc_environ(env),
            Some(Pane::ITerm2 {
                session_id: "11223344-5566-7788-99AA-BBCCDDEEFF00".into(),
            })
        );
        // Bare ID without colon prefix is kept as-is.
        assert_eq!(
            Pane::from_proc_environ(b"ITERM_SESSION_ID=abcdef1234\0"),
            Some(Pane::ITerm2 {
                session_id: "abcdef1234".into(),
            })
        );
    }

    #[test]
    fn multiplexer_wins_over_outer_terminal() {
        let zellij_in_kitty =
            b"KITTY_WINDOW_ID=7\0KITTY_LISTEN_ON=unix:@k\0ZELLIJ_SESSION_NAME=work\0ZELLIJ_PANE_ID=5\0";
        assert_eq!(
            Pane::from_proc_environ(zellij_in_kitty),
            Some(Pane::Zellij {
                session: "work".into(),
                pane_id: "5".into(),
            })
        );
        let zellij_in_wezterm =
            b"WEZTERM_PANE=2\0ZELLIJ_SESSION_NAME=work\0ZELLIJ_PANE_ID=terminal_9\0";
        assert_eq!(
            Pane::from_proc_environ(zellij_in_wezterm),
            Some(Pane::Zellij {
                session: "work".into(),
                pane_id: "terminal_9".into(),
            })
        );
    }

    #[test]
    fn outside_supported_terminals_there_is_no_pane() {
        assert_eq!(
            Pane::from_proc_environ(b"TERM=xterm-256color\0KITTY_LISTEN_ON=unix:@x\0"),
            None
        );
        assert_eq!(Pane::from_proc_environ(b"KITTY_WINDOW_ID=\0"), None);
        assert_eq!(Pane::from_proc_environ(b"WEZTERM_PANE=notanumber\0"), None);
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
                kitty_pid: None,
            },
        };
        assert_eq!(kitty.to_string(), "kitty window 4");
        assert_eq!(
            (kitty.terminal(), kitty.place().as_str()),
            ("kitty", "window 4")
        );
        let wez = Pane::WezTerm {
            pane_id: 9,
            socket: None,
        };
        assert_eq!(wez.to_string(), "wezterm pane 9");
        let zellij = Pane::Zellij {
            session: "dev".into(),
            pane_id: "3".into(),
        };
        assert_eq!(zellij.to_string(), "zellij dev · pane 3");
        let iterm = Pane::ITerm2 {
            session_id: "11223344-5566-7788-99AA-BBCCDDEEFF00".into(),
        };
        assert_eq!(iterm.to_string(), "iTerm2 session 11223344");
    }

    #[test]
    fn round_trips_through_the_fleet_summary() {
        let cases = [
            Pane::Tmux {
                pane: "0:@1.%2".into(),
            },
            Pane::Zellij {
                session: "dev".into(),
                pane_id: "3".into(),
            },
            Pane::Kitty {
                window: KittyWindow {
                    id: 4,
                    socket: Some("unix:@k".into()),
                    kitty_pid: Some(42),
                },
            },
            Pane::WezTerm {
                pane_id: 2,
                socket: Some("/tmp/wez.sock".into()),
            },
            Pane::ITerm2 {
                session_id: "11223344-5566".into(),
            },
        ];
        for pane in cases {
            let json = serde_json::to_string(&pane).unwrap();
            assert_eq!(serde_json::from_str::<Pane>(&json).unwrap(), pane);
        }

        let without_pid = r#"{"kind":"kitty","window":{"id":4,"socket":"unix:@k"}}"#;
        assert_eq!(
            serde_json::from_str::<Pane>(without_pid).unwrap(),
            Pane::Kitty {
                window: KittyWindow {
                    id: 4,
                    socket: Some("unix:@k".into()),
                    kitty_pid: None,
                }
            }
        );
    }
}
