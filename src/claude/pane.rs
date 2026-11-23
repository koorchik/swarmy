//! Finding where a Claude Code runs and taking the user there (`crate::core::pane::Pane`).

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use super::{iterm2, kitty, proc_env, tmux, wezterm, zellij};
use crate::core::pane::Pane;

/// Bound how long an external terminal/multiplexer jump command may block the TUI or CLI.
pub(super) const JUMP_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) enum RunOutcome {
    Exited(ExitStatus, String, String),
    TimedOut,
}

/// Spawn `cmd` with null stdin and piped stdout/stderr, killing it if it does not finish within
/// `timeout`.
pub(super) fn run_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
) -> Result<RunOutcome, std::io::Error> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout_thread = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = pipe.read_to_string(&mut buf);
            buf
        })
    });
    let stderr_thread = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = pipe.read_to_string(&mut buf);
            buf
        })
    });
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = stdout_thread
                .and_then(|h| h.join().ok())
                .unwrap_or_default();
            let stderr = stderr_thread
                .and_then(|h| h.join().ok())
                .unwrap_or_default();
            return Ok(RunOutcome::Exited(status, stdout, stderr));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(h) = stdout_thread {
                let _ = h.join();
            }
            if let Some(h) = stderr_thread {
                let _ = h.join();
            }
            return Ok(RunOutcome::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// tmux wins: inside tmux the pane is the place to go, even when tmux runs inside another terminal.
pub fn of(tmux_pane: Option<String>, pid: u32, proc_start: Option<&str>) -> Option<Pane> {
    match tmux_pane.filter(|t| !t.is_empty()) {
        Some(pane) => Some(Pane::Tmux { pane }),
        None => proc_env::pane_of(pid, proc_start),
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
        Pane::Zellij { session, pane_id } => match zellij::focus(session, pane_id) {
            Ok(()) => Jump::Done,
            Err(zellij::FocusError::Unreachable(why)) => Jump::Unreachable(why),
            Err(zellij::FocusError::Failed(why)) => {
                Jump::Failed(format!("zellij could not focus pane {pane_id}: {why}"))
            }
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
        Pane::WezTerm { pane_id, socket } => match wezterm::focus(*pane_id, socket.as_deref()) {
            Ok(()) => Jump::Done,
            Err(wezterm::FocusError::Unreachable(why)) => Jump::Unreachable(why),
            Err(wezterm::FocusError::Failed(why)) => {
                Jump::Failed(format!("wezterm could not focus pane {pane_id}: {why}"))
            }
        },
        Pane::ITerm2 { session_id } => match iterm2::focus(session_id) {
            Ok(()) => Jump::Done,
            Err(iterm2::FocusError::Unreachable(why)) => Jump::Unreachable(why),
            Err(iterm2::FocusError::Failed(why)) => {
                Jump::Failed(format!("iTerm2 could not focus session: {why}"))
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
    fn a_process_outside_supported_terminals_has_no_pane() {
        let in_any = [
            "KITTY_WINDOW_ID",
            "WEZTERM_PANE",
            "ZELLIJ_SESSION_NAME",
            "ITERM_SESSION_ID",
        ]
        .iter()
        .any(|k| std::env::var_os(k).is_some());
        if !in_any {
            assert_eq!(of(None, std::process::id(), Some("test")), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_a_stalled_command() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let t0 = Instant::now();
        let outcome = run_with_timeout(&mut cmd, Duration::from_millis(60)).unwrap();
        assert!(matches!(outcome, RunOutcome::TimedOut));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }
}
