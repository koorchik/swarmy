//! Focusing the kitty window a Claude Code runs in (`kitten @ focus-window`).
//!
//! Finding the window happens in `claude::proc_env` (`KITTY_WINDOW_ID`, `KITTY_LISTEN_ON`,
//! `KITTY_PID`). Focusing goes through kitty's remote control, which the user turns on in
//! `kitty.conf`.

use std::process::Command;

use super::pane::{JUMP_TIMEOUT, RunOutcome, run_with_timeout};
use crate::core::pane::KittyWindow;

pub const REMOTE_CONTROL_HINT: &str = "turn on kitty's remote control: `allow_remote_control socket-only` \
     and `listen_on unix:@kitty-{kitty_pid}` in kitty.conf, then restart kitty";

/// The `kitten` arguments that focus `window`: through its kitty's socket, else through the
/// kitty swarmy itself runs in (`own_kitty_pid` is swarmy's `KITTY_PID`, and only outside tmux).
/// None when neither can reach that kitty instance.
pub fn focus_args(
    window: &KittyWindow,
    own_kitty_pid: Option<u32>,
    in_tmux: bool,
) -> Option<Vec<String>> {
    let mut args = vec!["@".to_string()];
    match &window.socket {
        Some(socket) => args.extend(["--to".to_string(), socket.clone()]),
        None => {
            let same_kitty =
                !in_tmux && own_kitty_pid.is_some() && window.kitty_pid == own_kitty_pid;
            if !same_kitty {
                return None;
            }
        }
    }
    args.extend([
        "focus-window".into(),
        "--match".into(),
        format!("id:{}", window.id),
    ]);
    Some(args)
}

#[derive(Debug, PartialEq, Eq)]
pub enum FocusError {
    /// kitty's remote control is off or out of reach.
    NoRemoteControl,
    Failed(String),
}

fn classify_stderr(stderr: &str) -> FocusError {
    if stderr.to_ascii_lowercase().contains("remote control") {
        return FocusError::NoRemoteControl;
    }
    let first = stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("kitten failed");
    FocusError::Failed(first.trim().to_string())
}

fn run_kitty(args: &[String]) -> Result<RunOutcome, FocusError> {
    let run = |bin: &str| {
        let mut cmd = Command::new(bin);
        cmd.args(args);
        run_with_timeout(&mut cmd, JUMP_TIMEOUT)
    };
    match run("kitten") {
        Ok(outcome) => Ok(outcome),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            run("kitty").map_err(|e2| FocusError::Failed(format!("cannot run kitten: {e2}")))
        }
        Err(e) => Err(FocusError::Failed(format!("cannot run kitten: {e}"))),
    }
}

pub fn focus(window: &KittyWindow) -> Result<(), FocusError> {
    let own_pid = std::env::var("KITTY_PID")
        .ok()
        .and_then(|v| v.parse::<u32>().ok());
    let in_tmux = std::env::var_os("TMUX").is_some();
    let Some(args) = focus_args(window, own_pid, in_tmux) else {
        return Err(FocusError::NoRemoteControl);
    };
    match run_kitty(&args)? {
        RunOutcome::Exited(status, _, _) if status.success() => Ok(()),
        RunOutcome::Exited(_, _, stderr) => Err(classify_stderr(&stderr)),
        RunOutcome::TimedOut if window.socket.is_none() => Err(FocusError::NoRemoteControl),
        RunOutcome::TimedOut => Err(FocusError::Failed("timed out waiting for kitty".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_goes_through_the_socket_when_there_is_one() {
        let w = KittyWindow {
            id: 7,
            socket: Some("unix:@kitty-42".into()),
            kitty_pid: Some(42),
        };
        assert_eq!(
            focus_args(&w, None, false).unwrap(),
            [
                "@",
                "--to",
                "unix:@kitty-42",
                "focus-window",
                "--match",
                "id:7"
            ]
        );
        // Socket works even when swarmy runs inside tmux or another kitty instance.
        assert!(focus_args(&w, Some(99), true).is_some());
    }

    #[test]
    fn without_a_socket_only_from_the_same_kitty_outside_tmux() {
        let w = KittyWindow {
            id: 7,
            socket: None,
            kitty_pid: Some(42),
        };
        assert_eq!(
            focus_args(&w, Some(42), false).unwrap(),
            ["@", "focus-window", "--match", "id:7"]
        );
        // Different kitty OS process must not focus a window with the same id in swarmy's kitty.
        assert_eq!(focus_args(&w, Some(99), false), None);
        // Inside tmux, TTY remote control does not reach kitty.
        assert_eq!(focus_args(&w, Some(42), true), None);
        // Outside kitty altogether.
        assert_eq!(focus_args(&w, None, false), None);
        // Missing KITTY_PID on the target window cannot prove same kitty instance.
        let unknown = KittyWindow {
            id: 7,
            socket: None,
            kitty_pid: None,
        };
        assert_eq!(focus_args(&unknown, Some(42), false), None);
    }

    #[test]
    fn classifies_kitty_stderr_case_insensitively() {
        assert_eq!(
            classify_stderr(
                "Error: Remote control is disabled. Add allow_remote_control to your kitty.conf\n"
            ),
            FocusError::NoRemoteControl
        );
        assert_eq!(
            classify_stderr("remote control via TTY is disabled"),
            FocusError::NoRemoteControl
        );
        assert_eq!(
            classify_stderr("\n  No matching windows  \nmore detail\n"),
            FocusError::Failed("No matching windows".into())
        );
        assert_eq!(
            classify_stderr("   \n"),
            FocusError::Failed("kitten failed".into())
        );
    }
}
