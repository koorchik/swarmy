//! Focusing the iTerm2 session a Claude Code runs in (macOS, via AppleScript).

use std::process::Command;

use super::pane::{JUMP_TIMEOUT, RunOutcome, run_with_timeout};

#[derive(Debug, PartialEq, Eq)]
pub enum FocusError {
    Unreachable(String),
    Failed(String),
}

/// Fixed AppleScript receiving the target session's `unique ID` as `argv[1]` (never interpolated).
const FOCUS_SCRIPT: &str = r#"on run argv
    set targetId to item 1 of argv
    tell application "iTerm2"
        repeat with w in windows
            repeat with t in tabs of w
                repeat with s in sessions of t
                    if unique ID of s is targetId then
                        select w
                        select t
                        select s
                        activate
                        return "ok"
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    return "not_found"
end run"#;

fn classify_output(success: bool, stdout: &str, stderr: &str) -> Result<(), FocusError> {
    if success {
        let out = stdout.trim();
        if out == "ok" {
            return Ok(());
        }
        if out == "not_found" {
            return Err(FocusError::Failed(
                "iTerm2 session is no longer open".into(),
            ));
        }
    }
    let lower = stderr.to_ascii_lowercase();
    if stderr.contains("-1743") || lower.contains("not authorized") {
        return Err(FocusError::Unreachable(
            "allow Automation for iTerm2 in macOS Privacy & Security settings".into(),
        ));
    }
    let first = stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("osascript failed");
    Err(FocusError::Failed(first.trim().to_string()))
}

pub fn focus(session_id: &str) -> Result<(), FocusError> {
    if !cfg!(target_os = "macos") {
        return Err(FocusError::Unreachable(
            "iTerm2 jump is only available on macOS".into(),
        ));
    }
    let mut cmd = Command::new("osascript");
    cmd.args(["-e", FOCUS_SCRIPT, "--", session_id]);
    match run_with_timeout(&mut cmd, JUMP_TIMEOUT) {
        Ok(RunOutcome::Exited(status, stdout, stderr)) => {
            classify_output(status.success(), &stdout, &stderr)
        }
        Ok(RunOutcome::TimedOut) => Err(FocusError::Failed("timed out waiting for iTerm2".into())),
        Err(e) => Err(FocusError::Failed(format!("cannot run osascript: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_osascript_outcomes() {
        assert_eq!(classify_output(true, "ok\n", ""), Ok(()));
        assert_eq!(
            classify_output(true, "not_found\n", ""),
            Err(FocusError::Failed(
                "iTerm2 session is no longer open".into()
            ))
        );
        assert_eq!(
            classify_output(
                false,
                "",
                "execution error: Not authorized to send Apple events to iTerm2. (-1743)\n"
            ),
            Err(FocusError::Unreachable(
                "allow Automation for iTerm2 in macOS Privacy & Security settings".into()
            ))
        );
        assert_eq!(
            classify_output(false, "", "iTerm2 got an error: something went wrong\n"),
            Err(FocusError::Failed(
                "iTerm2 got an error: something went wrong".into()
            ))
        );
    }
}
