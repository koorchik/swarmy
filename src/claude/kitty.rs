//! Finding and focusing the kitty window a Claude Code runs in.
//!
//! kitty puts `KITTY_WINDOW_ID` (and, with `listen_on`, `KITTY_LISTEN_ON`) into the environment of
//! everything it starts, so a Claude Code's window comes from its own environment, which never
//! changes: it is read once per process. Focusing goes through kitty's remote control
//! (`kitten @ focus-window`), which the user turns on in `kitty.conf`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::core::pane::KittyWindow;

/// Windows already read, per (pid, start time) so a reused pid is read again.
type Seen = HashMap<(u32, String), Option<KittyWindow>>;

/// The kitty window of a running process (Linux; elsewhere none).
pub fn window_of(pid: u32, proc_start: Option<&str>) -> Option<KittyWindow> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let key = (pid, proc_start.unwrap_or_default().to_string());
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
/// kitty swarmy itself runs in (`own_window` is swarmy's `KITTY_WINDOW_ID`). None when neither
/// can reach it.
pub fn focus_args(window: &KittyWindow, own_window: Option<&str>) -> Option<Vec<String>> {
    let mut args = vec!["@".to_string()];
    match (&window.socket, own_window) {
        (Some(socket), _) => args.extend(["--to".to_string(), socket.clone()]),
        (None, Some(_)) => {}
        (None, None) => return None,
    }
    args.extend([
        "focus-window".into(),
        "--match".into(),
        format!("id:{}", window.id),
    ]);
    Some(args)
}

pub enum FocusError {
    /// kitty's remote control is off or out of reach.
    NoRemoteControl,
    Failed(String),
}

pub fn focus(window: &KittyWindow) -> Result<(), FocusError> {
    let own = std::env::var("KITTY_WINDOW_ID").ok();
    let Some(args) = focus_args(window, own.as_deref()) else {
        return Err(FocusError::NoRemoteControl);
    };
    let output = std::process::Command::new("kitten")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| FocusError::Failed(format!("cannot run kitten: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&output.stderr);
    if err.contains("remote control") {
        return Err(FocusError::NoRemoteControl);
    }
    let first = err
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("kitten failed");
    Err(FocusError::Failed(first.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_goes_through_the_socket_when_there_is_one() {
        let w = KittyWindow {
            id: 7,
            socket: Some("unix:@kitty-42".into()),
        };
        assert_eq!(
            focus_args(&w, None).unwrap(),
            [
                "@",
                "--to",
                "unix:@kitty-42",
                "focus-window",
                "--match",
                "id:7"
            ]
        );
    }

    #[test]
    fn without_a_socket_only_from_inside_kitty() {
        let w = KittyWindow {
            id: 7,
            socket: None,
        };
        assert_eq!(
            focus_args(&w, Some("1")).unwrap(),
            ["@", "focus-window", "--match", "id:7"]
        );
        assert_eq!(focus_args(&w, None), None);
    }
}
