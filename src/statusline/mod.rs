//! `swarmy statusline`: Claude Code's status line as a source of exact limit readings, and a
//! short line about the fleet for Claude Code to show.
//!
//! When `settings.json` has `"statusLine": {"type": "command", "command": "swarmy statusline"}`,
//! Claude Code runs the command after each model response and passes a JSON on stdin
//! ([`input`]). For subscribers it carries `rate_limits.{five_hour,seven_day}.{used_percentage,
//! resets_at}`, taken from the response itself: the real numbers, no extra request. `swarmy
//! statusline` writes them to `<data>/statusline/<seat id>.json` ([`reading`]) and prints a short
//! line ([`render`]). The dashboard reads those files as limit readings of the seat's account.
//!
//! It runs after every response, so it must stay fast: no index, no scan of transcripts.

pub mod input;
pub mod reading;
pub mod render;

use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::Utc;

use input::parse_input;
use reading::{file_for, live_from, read, write};
use render::{Extra, render};

/// The config dir of the Claude Code that runs us: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn current_config_dir(home: &Path) -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"))
}

/// How far back from its end the status line reads a transcript for the last response's owner.
const TAIL_MAX_BYTES: u64 = 1 << 20;

/// Keeps this call's limit reading when it says something new (`reading::next`): only when the
/// numbers moved is the transcript's tail read for whose numbers they are and since when, so an
/// unchanged line costs one small file read. The file is the dashboard's trigger.
fn record(input: &input::Input, data: &Path, home: &Path, dir: &Path, now: chrono::DateTime<Utc>) {
    let Some(windows) = reading::windows(input) else {
        return;
    };
    let file = file_for(data, dir);
    let old = read(&file);
    if reading::same_numbers(old.as_ref(), &windows) {
        return;
    }
    let tail = input
        .transcript_path
        .as_deref()
        .and_then(|p| crate::claude::transcript::tail(Path::new(p), TAIL_MAX_BYTES).ok())
        .unwrap_or_default();
    // The session's owner is who the response was for; the seat's login only fills in.
    let account = tail.owner.or_else(|| {
        crate::claude::config_json::read_seat_file(&crate::claude::seats::claude_json_of(dir, home))
            .ok()
            .and_then(|cj| cj.account)
            .map(|a| a.account_uuid)
    });
    let Some(account) = account else {
        // Nobody to book the numbers to: better no reading than one under a guessed account.
        return;
    };
    let at = tail.last_response.filter(|t| *t <= now).unwrap_or(now);
    let Some(live) = live_from(input, dir, crate::core::ids::AccountId::new(account), at) else {
        return;
    };
    if let Some(new) = reading::next(old.as_ref(), live)
        && let Err(e) = write(&file, &new)
    {
        crate::diag::log(
            "statusline",
            &format!("cannot write {}: {e}", file.display()),
        );
    }
}

/// `swarmy statusline [--then CMD]`: record, then print (after the user's own status line).
pub fn run(
    dirs: &crate::config::Dirs,
    conf: &crate::config::StatuslineConf,
    then: Option<&str>,
) -> anyhow::Result<()> {
    let mut text = String::new();
    let _ = std::io::stdin().read_to_string(&mut text);
    let input = parse_input(&text);
    let dir = current_config_dir(&dirs.home);
    let now = Utc::now();
    record(&input, &dirs.data, &dirs.home, &dir, now);
    let fleet = crate::fleet::read_fresh(&crate::fleet::path(&dirs.data), conf.fresh_seconds, now);
    // No dashboard running: find the sessions that wait for the user ourselves.
    let waiting = if fleet.is_some() || !conf.segments.iter().any(|s| s == "attention") {
        Vec::new()
    } else {
        let mut dirs_: Vec<PathBuf> = vec![dirs.home.join(".claude"), dir.clone()];
        if let Ok(rd) = std::fs::read_dir(&dirs.home) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(".claude-")
                    && e.path().join("sessions").is_dir()
                {
                    dirs_.push(e.path());
                }
            }
        }
        dirs_.sort();
        dirs_.dedup();
        dirs_
            .iter()
            .flat_map(|d| crate::claude::registry::waiting_sessions(&d.join("sessions")))
            .map(|w| {
                (
                    w.session_id.to_string(),
                    w.label(),
                    w.what.clone(),
                    w.pane.clone(),
                )
            })
            .collect()
    };
    let seat_key = crate::claude::seats::seat_id(&dir);
    let columns: usize = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(120);
    let theirs = then
        .and_then(|cmd| run_previous(cmd, &text))
        .filter(|s| !s.trim().is_empty());
    let theirs_w = theirs
        .as_ref()
        .map(|t| render::visible_width(t.trim_end()) + 3)
        .unwrap_or(0);
    let x = Extra {
        fleet,
        waiting,
        seat_key,
        mask: conf.mask || std::env::var_os("SWARMY_MASK").is_some(),
        color: std::env::var_os("NO_COLOR").is_none(),
        segments: conf.segments.clone(),
        width: columns
            .saturating_sub(conf.reserve as usize + theirs_w + 4)
            .max(20),
        lines: conf.lines,
        branch: input
            .workspace
            .as_ref()
            .and_then(|w| w.current_dir.as_deref())
            .filter(|_| conf.lines >= 2)
            .and_then(|d| branch_of(Path::new(d))),
    };
    let mine = render(&input, &x, now);
    match theirs {
        Some(t) => println!("{} │ {mine}", t.trim_end()),
        None => println!("{mine}"),
    }
    Ok(())
}

/// The status line the user had before: same stdin, its first line of output.
fn run_previous(cmd: &str, stdin: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", cmd])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .ok()?
    } else {
        Command::new("sh")
            .args(["-c", cmd])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .ok()?
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin.as_bytes());
    }
    let out = child.wait_with_output().ok()?;
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string(),
    )
}

/// The current git branch of `dir`, read from `.git/HEAD` (never by running git).
fn branch_of(dir: &Path) -> Option<String> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let git = d.join(".git");
        let head = if git.is_dir() {
            git.join("HEAD")
        } else if git.is_file() {
            let t = std::fs::read_to_string(&git).ok()?;
            let gd = t
                .lines()
                .find_map(|l| l.strip_prefix("gitdir:"))?
                .trim()
                .to_string();
            let gd = if Path::new(&gd).is_absolute() {
                PathBuf::from(gd)
            } else {
                d.join(gd)
            };
            gd.join("HEAD")
        } else {
            cur = d.parent();
            continue;
        };
        let h = std::fs::read_to_string(head).ok()?;
        return Some(match h.trim().strip_prefix("ref: refs/heads/") {
            Some(b) => b.to_string(),
            None => h.trim().chars().take(8).collect(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review H2 and M5 end to end: the reading is booked to the owner of the session's last
    /// response (not to the seat's login now) and dated by that response; the same numbers again
    /// write nothing.
    #[test]
    fn a_reading_names_the_sessions_owner_and_the_response_time() {
        let t = tempfile::tempdir().unwrap();
        let (data, home) = (t.path().join("data"), t.path().join("home"));
        let dir = home.join(".claude-work");
        std::fs::create_dir_all(&dir).unwrap();
        // The seat now holds acc-b; the session's last response ran under acc-a.
        std::fs::write(
            dir.join(".claude.json"),
            r#"{"oauthAccount": {"accountUuid": "acc-b"}}"#,
        )
        .unwrap();
        let transcript = dir.join("s1.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"session-owner","accountUuid":"acc-a","sessionId":"s1"}"#, "\n",
                r#"{"type":"assistant","timestamp":"2026-11-22T09:00:00Z","message":{"id":"m1","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}"#, "\n",
            ),
        )
        .unwrap();
        let input = |pct: f64| {
            parse_input(&format!(
                r#"{{"session_id":"s1","transcript_path":{},"rate_limits":{{"seven_day":{{"used_percentage":{pct},"resets_at":1795960000}}}}}}"#,
                // A JSON string: Windows paths carry backslashes.
                serde_json::Value::String(transcript.display().to_string())
            ))
        };
        let now = "2026-11-22T11:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        record(&input(40.0), &data, &home, &dir, now);
        let file = file_for(&data, &dir);
        let r = read(&file).unwrap();
        assert_eq!(r.account.as_str(), "acc-a");
        assert_eq!(
            r.at,
            "2026-11-22T09:00:00Z"
                .parse::<chrono::DateTime<Utc>>()
                .unwrap()
        );
        // Later, on `refreshInterval`, the same numbers: nothing is written, the time stays.
        let stamp = std::fs::metadata(&file).unwrap().modified().unwrap();
        record(
            &input(40.0),
            &data,
            &home,
            &dir,
            now + chrono::Duration::minutes(30),
        );
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), stamp);
        // Without a transcript the seat's login names the account, dated now.
        std::fs::remove_file(&transcript).unwrap();
        record(
            &input(41.0),
            &data,
            &home,
            &dir,
            now + chrono::Duration::hours(1),
        );
        let r = read(&file).unwrap();
        assert_eq!(
            (r.account.as_str(), r.at),
            ("acc-b", now + chrono::Duration::hours(1))
        );
    }
}
