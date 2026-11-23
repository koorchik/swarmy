//! Finding where a Claude Code runs and taking the user there (`crate::core::pane::Pane`).

use super::{kitty, tmux};
use crate::core::pane::Pane;

/// tmux wins: inside tmux the pane is the place to go, even when tmux itself runs in kitty.
pub fn of(tmux_pane: Option<String>, pid: u32, proc_start: Option<&str>) -> Option<Pane> {
    match tmux_pane.filter(|t| !t.is_empty()) {
        Some(pane) => Some(Pane::Tmux { pane }),
        None => kitty::window_of(pid, proc_start).map(|window| Pane::Kitty { window }),
    }
}

/// Outcome of a jump, for the caller to report.
pub enum Jump {
    Done,
    /// Cannot get there from here; says what would make it work.
    Unreachable(String),
    Failed(String),
}

pub fn jump(pane: &Pane) -> Jump {
    match pane {
        Pane::Tmux { pane } => match tmux::switch_to(pane) {
            tmux::Jump::Done => Jump::Done,
            tmux::Jump::NotInTmux { hint } => {
                Jump::Unreachable(format!("not inside tmux; attach with: {hint}"))
            }
            tmux::Jump::Failed => Jump::Failed(format!("tmux could not switch to {pane}")),
        },
        Pane::Kitty { window } => match kitty::focus(window) {
            Ok(()) => Jump::Done,
            Err(kitty::FocusError::NoRemoteControl) => {
                Jump::Unreachable(kitty::REMOTE_CONTROL_HINT.to_string())
            }
            Err(kitty::FocusError::Failed(why)) => {
                Jump::Failed(format!("kitty could not focus window {}: {why}", window.id))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tmux_pane_from_the_registry_wins() {
        assert_eq!(
            of(Some("0:@1.%2".into()), std::process::id(), None),
            Some(Pane::Tmux {
                pane: "0:@1.%2".into()
            })
        );
    }

    #[test]
    fn a_process_outside_kitty_and_tmux_has_no_pane() {
        if std::env::var_os("KITTY_WINDOW_ID").is_none() {
            assert_eq!(of(None, std::process::id(), Some("test")), None);
        }
    }
}
