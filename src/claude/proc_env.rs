//! Cached lookup of a running process's terminal/multiplexer pane from its environment.
//!
//! A process's environment does not change after start, so it is read once per `(pid, start time)`
//! and cached to stay within the dashboard's battery budget:
//! - Linux: reads `/proc/<pid>/environ` directly (one file read, no process-table scan).
//! - macOS: refreshes only that single PID via `sysinfo`.
//! - Other platforms: returns `None`.

use crate::core::pane::Pane;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::collections::HashMap;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::{Mutex, OnceLock};

#[cfg(any(target_os = "linux", target_os = "macos"))]
type Seen = HashMap<(u32, String), Option<Pane>>;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cache() -> &'static Mutex<Seen> {
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    SEEN.get_or_init(Default::default)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn lookup_cached(key: &(u32, String)) -> Option<Option<Pane>> {
    cache().lock().ok().and_then(|s| s.get(key).cloned())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn store_cached(key: (u32, String), value: Option<Pane>) {
    if let Ok(mut s) = cache().lock() {
        if s.len() > 512 {
            s.clear();
        }
        s.insert(key, value);
    }
}

/// The terminal or multiplexer pane of a running process, read from its environment once per
/// process (Linux and macOS; elsewhere `None`).
pub fn pane_of(pid: u32, proc_start: Option<&str>) -> Option<Pane> {
    #[cfg(target_os = "linux")]
    {
        let start = match proc_start {
            Some(s) if !s.is_empty() => s.to_string(),
            Some(_) | None => super::procs::proc_start_ticks(pid).unwrap_or_default(),
        };
        let key = (pid, start);
        if let Some(hit) = lookup_cached(&key) {
            return hit;
        }
        let found = std::fs::read(format!("/proc/{pid}/environ"))
            .ok()
            .and_then(|e| Pane::from_proc_environ(&e));
        store_cached(key, found.clone());
        found
    }

    #[cfg(target_os = "macos")]
    {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

        // Fast path when Claude Code recorded `procStart`: cache hit needs zero syscalls.
        if let Some(s) = proc_start.filter(|s| !s.is_empty()) {
            let key = (pid, s.to_string());
            if let Some(hit) = lookup_cached(&key) {
                return hit;
            }
            let found = read_macos_env(pid);
            store_cached(key, found.clone());
            return found;
        }

        // Without `procStart`, read this single PID's start time first so reused PIDs miss cache.
        static SYS: OnceLock<Option<Mutex<System>>> = OnceLock::new();
        let sys_lock = SYS
            .get_or_init(|| System::new().ok().map(Mutex::new))
            .as_ref()?;
        let Ok(mut sys) = sys_lock.lock() else {
            return None;
        };
        let spid = Pid::from_u32(pid);
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[spid]),
            true,
            ProcessRefreshKind::nothing(),
        );
        let start = sys.process(spid)?.start_time().to_string();
        let key = (pid, start);
        if let Some(hit) = lookup_cached(&key) {
            return hit;
        }
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[spid]),
            true,
            ProcessRefreshKind::nothing().with_environ(UpdateKind::OnlyIfNotSet),
        );
        let found = sys.process(spid).and_then(|p| {
            let entries: Vec<String> = p
                .environ()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            Pane::from_environ_pairs(entries.iter().filter_map(|e| e.split_once('=')))
        });
        store_cached(key, found.clone());
        found
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (pid, proc_start);
        None
    }
}

#[cfg(target_os = "macos")]
fn read_macos_env(pid: u32) -> Option<Pane> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let mut sys = System::new().ok()?;
    let spid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[spid]),
        true,
        ProcessRefreshKind::nothing().with_environ(UpdateKind::OnlyIfNotSet),
    );
    let p = sys.process(spid)?;
    let entries: Vec<String> = p
        .environ()
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    Pane::from_environ_pairs(entries.iter().filter_map(|e| e.split_once('=')))
}
