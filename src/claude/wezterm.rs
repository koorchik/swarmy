//! Focusing the WezTerm pane a Claude Code runs in (`wezterm cli activate-pane`).

use std::process::Command;

use super::pane::{JUMP_TIMEOUT, RunOutcome, run_with_timeout};

#[derive(Debug, PartialEq, Eq)]
pub enum FocusError {
    Unreachable(String),
    Failed(String),
}

fn can_reach(socket: Option<&str>, own_socket: Option<&str>, own_pane: Option<&str>) -> bool {
    socket.is_some() || own_socket.is_some() || own_pane.is_some()
}

fn focus_args(pane_id: u32) -> Vec<String> {
    vec![
        "cli".into(),
        "activate-pane".into(),
        "--pane-id".into(),
        pane_id.to_string(),
    ]
}

fn classify_stderr(stderr: &str) -> FocusError {
    let first = stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("wezterm cli failed");
    FocusError::Failed(first.trim().to_string())
}

pub fn focus(pane_id: u32, socket: Option<&str>) -> Result<(), FocusError> {
    let own_socket = std::env::var("WEZTERM_UNIX_SOCKET")
        .ok()
        .filter(|s| !s.is_empty());
    let own_pane = std::env::var("WEZTERM_PANE").ok().filter(|s| !s.is_empty());
    if !can_reach(socket, own_socket.as_deref(), own_pane.as_deref()) {
        return Err(FocusError::Unreachable(
            "not inside wezterm and no WEZTERM_UNIX_SOCKET found".into(),
        ));
    }
    let mut cmd = Command::new("wezterm");
    cmd.args(focus_args(pane_id));
    if let Some(sock) = socket {
        cmd.env("WEZTERM_UNIX_SOCKET", sock);
    }
    match run_with_timeout(&mut cmd, JUMP_TIMEOUT) {
        Ok(RunOutcome::Exited(status, _, _)) if status.success() => Ok(()),
        Ok(RunOutcome::Exited(_, _, stderr)) => Err(classify_stderr(&stderr)),
        Ok(RunOutcome::TimedOut) => Err(FocusError::Failed("timed out waiting for wezterm".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(FocusError::Unreachable("wezterm CLI is not on PATH".into()))
        }
        Err(e) => Err(FocusError::Failed(format!("cannot run wezterm: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaches_wezterm_through_target_or_own_env() {
        assert!(can_reach(Some("/tmp/wez.sock"), None, None));
        assert!(can_reach(None, Some("/tmp/wez.sock"), None));
        assert!(can_reach(None, None, Some("0")));
        assert!(!can_reach(None, None, None));
    }

    #[test]
    fn builds_activate_pane_args() {
        assert_eq!(focus_args(12), ["cli", "activate-pane", "--pane-id", "12"]);
    }

    #[test]
    fn classifies_wezterm_stderr() {
        assert_eq!(
            classify_stderr("\n  pane id 99 not found\nstack trace\n"),
            FocusError::Failed("pane id 99 not found".into())
        );
        assert_eq!(
            classify_stderr("   "),
            FocusError::Failed("wezterm cli failed".into())
        );
    }
}
