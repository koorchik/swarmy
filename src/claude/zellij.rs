//! Focusing the Zellij pane a Claude Code runs in (`zellij action focus-pane-id`).

use std::process::Command;

use super::pane::{JUMP_TIMEOUT, RunOutcome, run_with_timeout};

#[derive(Debug, PartialEq, Eq)]
pub enum FocusError {
    Unreachable(String),
    Failed(String),
}

fn focus_args(
    session: &str,
    pane_id: &str,
    own_session: Option<&str>,
) -> Result<Vec<String>, FocusError> {
    if own_session != Some(session) {
        return Err(FocusError::Unreachable(format!(
            "not in zellij session '{session}'; attach with: zellij attach {session}"
        )));
    }
    Ok(vec![
        "--session".into(),
        session.into(),
        "action".into(),
        "focus-pane-id".into(),
        pane_id.into(),
    ])
}

fn classify_stderr(stderr: &str) -> FocusError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("unrecognized subcommand")
        || lower.contains("unknown subcommand")
        || lower.contains("unexpected argument")
    {
        return FocusError::Unreachable(
            "zellij 0.44+ is needed to jump to a pane (`action focus-pane-id`)".into(),
        );
    }
    let first = stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("zellij failed");
    FocusError::Failed(first.trim().to_string())
}

pub fn focus(session: &str, pane_id: &str) -> Result<(), FocusError> {
    let own_session = std::env::var("ZELLIJ_SESSION_NAME")
        .ok()
        .filter(|s| !s.is_empty());
    let args = focus_args(session, pane_id, own_session.as_deref())?;
    let mut cmd = Command::new("zellij");
    cmd.args(&args);
    match run_with_timeout(&mut cmd, JUMP_TIMEOUT) {
        Ok(RunOutcome::Exited(status, _, _)) if status.success() => Ok(()),
        Ok(RunOutcome::Exited(_, _, stderr)) => Err(classify_stderr(&stderr)),
        Ok(RunOutcome::TimedOut) => Err(FocusError::Failed("timed out waiting for zellij".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(FocusError::Unreachable("zellij is not on PATH".into()))
        }
        Err(e) => Err(FocusError::Failed(format!("cannot run zellij: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focuses_only_when_attached_to_the_same_session() {
        assert_eq!(
            focus_args("dev", "3", Some("dev")).unwrap(),
            ["--session", "dev", "action", "focus-pane-id", "3"]
        );
        assert_eq!(
            focus_args("dev", "3", Some("other")),
            Err(FocusError::Unreachable(
                "not in zellij session 'dev'; attach with: zellij attach dev".into()
            ))
        );
        assert_eq!(
            focus_args("dev", "3", None),
            Err(FocusError::Unreachable(
                "not in zellij session 'dev'; attach with: zellij attach dev".into()
            ))
        );
    }

    #[test]
    fn detects_older_zellij_without_focus_pane_id() {
        assert_eq!(
            classify_stderr("error: unrecognized subcommand 'focus-pane-id'\n"),
            FocusError::Unreachable(
                "zellij 0.44+ is needed to jump to a pane (`action focus-pane-id`)".into()
            )
        );
        assert_eq!(
            classify_stderr("pane 9 not found\n"),
            FocusError::Failed("pane 9 not found".into())
        );
    }
}
