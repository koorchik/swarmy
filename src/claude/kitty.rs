//! Finding and focusing the kitty window a Claude Code runs in.
//!
//! kitty puts `KITTY_WINDOW_ID` (and, with `listen_on`, `KITTY_LISTEN_ON`) into the environment of
//! everything it starts, so a Claude Code's window comes from its own environment, which never
//! changes: it is read once per process. Focusing goes through kitty's remote control
//! (`kitten @ focus-window`), which the user turns on in `kitty.conf`.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::procs::proc_start_ticks;
use crate::core::pane::KittyWindow;

/// Windows already read, per (pid, start time) so a reused pid is read again.
type Seen = HashMap<(u32, String), Option<KittyWindow>>;

/// Bound how long `kitten @` may block the TUI or CLI (TTY remote control stalls when disabled).
const FOCUS_TIMEOUT: Duration = Duration::from_secs(2);

/// The kitty window of a running process (Linux; elsewhere none).
pub fn window_of(pid: u32, proc_start: Option<&str>) -> Option<KittyWindow> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let start = match proc_start {
        Some(s) if !s.is_empty() => s.to_string(),
        Some(_) | None => proc_start_ticks(pid).unwrap_or_default(),
    };
    let key = (pid, start);
    let seen = SEEN.get_or_init(Default::default);
    if let Some(hit) = seen.lock().ok().and_then(|s| s.get(&key).cloned()) {
        return hit;
    }
    let found = std::fs::read(format!("/proc/{pid}/environ"))
        .ok()
        .and_then(|e| KittyWindow::from_environ(&e));
    if let Ok(mut s) = seen.lock() {
        if s.len() > 512 {
            s.clear();
        }
        s.insert(key, found.clone());
    }
    found
}

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

fn spawn_kitty(args: &[String]) -> Result<Child, FocusError> {
    let spawn = |bin: &str| {
        Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
    };
    match spawn("kitten") {
        Ok(child) => Ok(child),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            spawn("kitty").map_err(|e2| FocusError::Failed(format!("cannot run kitten: {e2}")))
        }
        Err(e) => Err(FocusError::Failed(format!("cannot run kitten: {e}"))),
    }
}

enum WaitOutcome {
    Exited(ExitStatus, String),
    TimedOut,
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Result<WaitOutcome, std::io::Error> {
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
            let stderr = stderr_thread
                .and_then(|h| h.join().ok())
                .unwrap_or_default();
            return Ok(WaitOutcome::Exited(status, stderr));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(h) = stderr_thread {
                let _ = h.join();
            }
            return Ok(WaitOutcome::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(15));
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
    let child = spawn_kitty(&args)?;
    match wait_with_timeout(child, FOCUS_TIMEOUT)
        .map_err(|e| FocusError::Failed(format!("kitten failed: {e}")))?
    {
        WaitOutcome::Exited(status, _) if status.success() => Ok(()),
        WaitOutcome::Exited(_, stderr) => Err(classify_stderr(&stderr)),
        WaitOutcome::TimedOut if window.socket.is_none() => Err(FocusError::NoRemoteControl),
        WaitOutcome::TimedOut => Err(FocusError::Failed("timed out waiting for kitty".into())),
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

    #[cfg(unix)]
    #[test]
    fn wait_with_timeout_kills_a_stalled_command() {
        let child = Command::new("sleep")
            .arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let t0 = Instant::now();
        let outcome = wait_with_timeout(child, Duration::from_millis(60)).unwrap();
        assert!(matches!(outcome, WaitOutcome::TimedOut));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }
}
