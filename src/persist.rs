//! Native session persistence.
//!
//! jelly snapshots the layout of live tmux sessions to its own state files and
//! restores them one at a time, with no external plugin dependency.
//!
//! When `lazy_restore` is disabled every entry point here is a no-op, so behavior is
//! identical to upstream tmux-sessionizer.

use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use serde_derive::{Deserialize, Serialize};

use crate::{
    configs::{Config, DEFAULT_SAVE_INTERVAL},
    tmux::Tmux,
};

/// File under the tms state directory storing the last boot id jelly acted on.
const BOOT_ID_FILE: &str = "boot-id";

/// A saved tmux session: its windows, each with a layout and per-pane directories.
#[derive(Debug, Serialize, Deserialize)]
struct SavedSession {
    name: String,
    windows: Vec<SavedWindow>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedWindow {
    name: String,
    /// `true` when this window's name was driven by tmux's `automatic-rename`
    /// and should be re-derived on restore; `false` when it was a name the
    /// user set explicitly and must survive verbatim.
    ///
    /// Defaults to `true` for snapshots written before the field existed —
    /// guessing "auto" is the only safe default, since restoring a stale
    /// auto-rename name as if it were manual is what locks every window to
    /// whatever process happened to be running at save time.
    #[serde(default = "default_true")]
    automatic_rename: bool,
    /// The tmux `window_layout` string (encodes pane geometry).
    layout: String,
    /// Working directory of each pane, in pane-index order.
    panes: Vec<String>,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------------

/// Snapshot every live tmux session to `<state>/jelly/sessions/<name>.json`.
///
/// Files for sessions that are not currently live are left untouched, so a session's
/// last-known layout survives until the next time that session is seen.
pub fn save_all_sessions(tmux: &Tmux) {
    let Some(dir) = sessions_dir() else {
        return;
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }

    // `#{E:automatic-rename}` resolves the *effective* per-window option as
    // `1`/`0` (tmux 3.6+). `#{pane_current_command}` lets us fall back to a
    // heuristic when the option lies (see `compute_automatic_rename`).
    let format = "#{session_name}\t#{window_index}\t#{window_name}\t#{window_layout}\t#{pane_index}\t#{pane_current_path}\t#{E:automatic-rename}\t#{pane_current_command}";

    let mut sessions: BTreeMap<String, BTreeMap<u32, WindowAcc>> = BTreeMap::new();
    for line in tmux.list_all_panes(format).lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 8 {
            continue;
        }
        let (Ok(window_index), Ok(pane_index)) =
            (fields[1].parse::<u32>(), fields[4].parse::<u32>())
        else {
            continue;
        };
        let window = sessions
            .entry(fields[0].to_string())
            .or_default()
            .entry(window_index)
            .or_insert_with(|| WindowAcc {
                name: fields[2].to_string(),
                layout: fields[3].to_string(),
                auto_rename_option: fields[6] == "1",
                panes: BTreeMap::new(),
                pane_commands: Vec::new(),
            });
        window.panes.insert(pane_index, fields[5].to_string());
        window.pane_commands.push(fields[7].to_string());
    }

    for (name, windows) in sessions {
        let saved = SavedSession {
            name: name.clone(),
            windows: windows
                .into_values()
                .map(|w| SavedWindow {
                    automatic_rename: compute_automatic_rename(
                        w.auto_rename_option,
                        &w.name,
                        &w.pane_commands,
                    ),
                    name: w.name,
                    layout: w.layout,
                    panes: w.panes.into_values().collect(),
                })
                .collect(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&saved) {
            let _ = fs::write(dir.join(format!("{}.json", path_safe(&name))), json);
        }
    }
}

struct WindowAcc {
    name: String,
    layout: String,
    auto_rename_option: bool,
    panes: BTreeMap<u32, String>,
    pane_commands: Vec<String>,
}

/// Decide whether a window's name should be treated as automatic-rename output.
///
/// The straightforward case is the tmux option: if `automatic-rename` is `on`
/// for the window, the name is whatever the foreground command is right now,
/// and we mustn't re-impose it on restore.
///
/// The subtle case is the recovery from jelly's own past mistake: older
/// versions called `rename-window` on every restored window, which tmux treats
/// as "the user has named this", flipping the per-window option to `off`. The
/// resulting name still looks like an auto-rename name (e.g. matches `zsh`,
/// `nvim`, …), so when it equals one of the panes' current foreground command
/// we treat it as automatic. That breaks the lock-in loop without needing the
/// user to reset anything by hand.
fn compute_automatic_rename(option_on: bool, name: &str, pane_commands: &[String]) -> bool {
    option_on || (!name.is_empty() && pane_commands.iter().any(|c| c == name))
}

// ---------------------------------------------------------------------------
// Restoring
// ---------------------------------------------------------------------------

/// Try to restore `session_name` from saved state.
///
/// Returns `true` when the session was restored (the caller should not create a fresh
/// session), and `false` when the caller should fall back to creating one normally.
pub fn try_restore_session(session_name: &str, config: &Config, tmux: &Tmux) -> bool {
    if !config.lazy_restore.unwrap_or(false) {
        return false;
    }
    let Some(saved) = load_saved_session(session_name) else {
        return false;
    };
    if saved.windows.is_empty() {
        return false;
    }
    restore_session(&saved, tmux);
    true
}

/// Load a session's saved state from jelly's own save file.
fn load_saved_session(session_name: &str) -> Option<SavedSession> {
    let dir = sessions_dir()?;
    let file = dir.join(format!("{}.json", path_safe(session_name)));
    let data = fs::read_to_string(file).ok()?;
    serde_json::from_str(&data).ok()
}

/// Recreate `saved` as a detached tmux session.
fn restore_session(saved: &SavedSession, tmux: &Tmux) {
    for (index, window) in saved.windows.iter().enumerate() {
        let first_path = window.panes.first().and_then(|p| usable_dir(p));

        // Only force a window name when the user had set one explicitly. For
        // automatic-rename windows, letting tmux pick the initial name (and
        // keep updating it) avoids permanently locking the name to whatever
        // process was running at save time — both `rename-window` and
        // `new-window -n` flip the per-window `automatic-rename` option to
        // `off`, which is what produces the "stuck on 0: zsh" bug.
        let explicit_name = (!window.automatic_rename && !window.name.is_empty())
            .then_some(window.name.as_str());

        if index == 0 {
            tmux.new_session(Some(&saved.name), first_path);
            if let Some(name) = explicit_name {
                tmux.rename_window(&saved.name, name);
            }
        } else {
            tmux.new_window(explicit_name, first_path, Some(&saved.name));
        }

        // `-t <session>` targets the session's current window — the one just created.
        for path in window.panes.iter().skip(1) {
            tmux.split_window(&saved.name, usable_dir(path));
        }
        if !window.layout.is_empty() {
            tmux.select_layout(&saved.name, &window.layout);
        }
    }
}

/// Returns the path only if it is an existing directory (a stale cwd would make
/// `tmux new-session -c` fail, aborting the whole restore).
fn usable_dir(path: &str) -> Option<&str> {
    (!path.is_empty() && Path::new(path).is_dir()).then_some(path)
}

// ---------------------------------------------------------------------------
// First run after a reboot
// ---------------------------------------------------------------------------

/// On the first `jelly` run after a reboot, close every open tmux session for a clean
/// slate. Best-effort: any failure degrades gracefully to upstream behavior.
pub fn handle_first_run(config: &Config, tmux: &Tmux) {
    if !config.lazy_restore.unwrap_or(false) {
        return;
    }
    let Some(state_dir) = jelly_state_dir() else {
        return;
    };
    let Some(boot_id) = current_boot_id() else {
        return;
    };

    let boot_id_file = state_dir.join(BOOT_ID_FILE);
    let stored = fs::read_to_string(&boot_id_file)
        .ok()
        .map(|s| s.trim().to_string());

    match stored {
        // Same boot: the clean-slate step already ran this boot.
        Some(ref s) if *s == boot_id => return,
        // First boot jelly has ever tracked: record it but do not close anything,
        // since we cannot tell whether a reboot actually happened.
        None => {
            let _ = write_state_file(&boot_id_file, &boot_id);
            return;
        }
        // Stored id differs from the current one: the machine rebooted.
        Some(_) => {}
    }

    // Record the new boot id up front so the clean slate runs at most once per boot.
    if write_state_file(&boot_id_file, &boot_id).is_err() {
        eprintln!("jelly: could not record boot id; skipping post-reboot clean slate");
        return;
    }
    kill_all_sessions(tmux);
}

/// Close every open tmux session.
///
/// When jelly runs inside tmux the attached session is left alone so jelly is not
/// terminated mid-run; every other session is closed.
fn kill_all_sessions(tmux: &Tmux) {
    let current = if env::var_os("TMUX").is_some() {
        let mut name = tmux.display_message("'#S'");
        name.retain(|c| c != '\'' && c != '\n');
        (!name.is_empty()).then_some(name)
    } else {
        None
    };

    for line in tmux.list_sessions("'#S'").lines() {
        let name: String = line.chars().filter(|c| *c != '\'').collect();
        let name = name.trim();
        if name.is_empty() || current.as_deref() == Some(name) {
            continue;
        }
        tmux.kill_session(name);
    }
}

// ---------------------------------------------------------------------------
// Background saver
// ---------------------------------------------------------------------------

/// Spawn a detached background process that snapshots sessions every
/// `save_interval` minutes. A lock file ensures only one runs at a time, so this
/// is safe to call on every `jelly` invocation.
pub fn ensure_save_daemon(config: &Config) {
    if config.save_interval.unwrap_or(DEFAULT_SAVE_INTERVAL) == 0 {
        return;
    }
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .arg("save-loop")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0) // detach from jelly's process group
        .spawn();
}

/// Body of the `save-loop` background process: snapshot every `save_interval`
/// minutes until the tmux server goes away. Exits immediately if another saver
/// already holds the lock.
pub fn run_save_loop(config: &Config, tmux: &Tmux) {
    if !config.lazy_restore.unwrap_or(false) {
        return;
    }
    let interval = config.save_interval.unwrap_or(DEFAULT_SAVE_INTERVAL);
    if interval == 0 {
        return;
    }
    let Some(state_dir) = jelly_state_dir() else {
        return;
    };
    let lock = state_dir.join("saver.pid");
    if !claim_lock(&lock) {
        return;
    }

    let nap = Duration::from_secs(interval * 60);
    loop {
        thread::sleep(nap);
        if !server_has_sessions(tmux) {
            break;
        }
        save_all_sessions(tmux);
    }
    let _ = fs::remove_file(&lock);
}

/// Claim the single-saver lock. Returns `false` when another live saver holds it.
fn claim_lock(lock: &Path) -> bool {
    if let Some(parent) = lock.parent() {
        let _ = fs::create_dir_all(parent);
    }
    for _ in 0..5 {
        match fs::OpenOptions::new().write(true).create_new(true).open(lock) {
            Ok(mut file) => {
                let _ = write!(file, "{}", std::process::id());
                return true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = fs::read_to_string(lock)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok());
                match holder {
                    // A live saver already owns the lock.
                    Some(pid) if pid_alive(pid) => return false,
                    // Stale lock from a crashed saver — drop it and retry.
                    _ if fs::remove_file(lock).is_err() => return false,
                    _ => {}
                }
            }
            Err(_) => return false,
        }
    }
    false
}

/// Whether process `pid` is currently alive.
fn pid_alive(pid: u32) -> bool {
    if Path::new("/proc").is_dir() {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether the tmux server currently has at least one session.
fn server_has_sessions(tmux: &Tmux) -> bool {
    !tmux.list_sessions("'#S'").trim().is_empty()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The jelly state directory (`$XDG_STATE_HOME/jelly` or `~/.local/state/jelly`).
fn jelly_state_dir() -> Option<PathBuf> {
    if let Some(xdg) = env::var_os("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("jelly"));
        }
    }
    dirs::home_dir().map(|home| home.join(".local/state/jelly"))
}

/// The directory holding per-session save files.
fn sessions_dir() -> Option<PathBuf> {
    jelly_state_dir().map(|d| d.join("sessions"))
}

/// A value that uniquely identifies the current boot.
fn current_boot_id() -> Option<String> {
    if let Ok(id) = fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        let id = id.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    if let Ok(output) = Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()
    {
        if output.status.success() {
            let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

/// Reduce a session name to a single safe path component.
fn path_safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn write_state_file(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_safe_reduces_to_a_single_component() {
        assert_eq!(path_safe("repo-feature/foo"), "repo-feature_foo");
        assert_eq!(path_safe("a.b:c"), "a_b_c");
        assert_eq!(path_safe("plain_name-1"), "plain_name-1");
    }

    #[test]
    fn saved_session_round_trips_through_json() {
        let saved = SavedSession {
            name: "proj".to_string(),
            windows: vec![SavedWindow {
                name: "main".to_string(),
                automatic_rename: false,
                layout: "abcd,80x24,0,0,0".to_string(),
                panes: vec!["/home/u/proj".to_string()],
            }],
        };
        let json = serde_json::to_string(&saved).unwrap();
        let back: SavedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "proj");
        assert_eq!(back.windows.len(), 1);
        assert!(!back.windows[0].automatic_rename);
        assert_eq!(back.windows[0].panes, vec!["/home/u/proj"]);
    }

    #[test]
    fn legacy_snapshot_without_field_defaults_to_automatic_rename() {
        // Snapshots written before the field existed shouldn't lock window
        // names on restore — they have to default to "treat as auto-rename".
        let json =
            r#"{"name":"proj","windows":[{"name":"zsh","layout":"","panes":["/tmp"]}]}"#;
        let back: SavedSession = serde_json::from_str(json).unwrap();
        assert!(back.windows[0].automatic_rename);
    }

    #[test]
    fn auto_rename_heuristic_trusts_the_tmux_option() {
        // When tmux says `automatic-rename` is on, that's authoritative.
        assert!(compute_automatic_rename(true, "anything", &[]));
        assert!(compute_automatic_rename(true, "logs", &["zsh".into()]));
    }

    #[test]
    fn auto_rename_heuristic_recovers_locked_loop_windows() {
        // The window's option has been forced off, but its name still matches
        // a foreground process — almost certainly leftover from an earlier
        // restore that called rename-window. Treat as auto-rename so the next
        // restore stops re-locking it.
        assert!(compute_automatic_rename(
            false,
            "nvim",
            &["zsh".to_string(), "nvim".to_string()],
        ));
    }

    #[test]
    fn auto_rename_heuristic_preserves_truly_manual_names() {
        // No pane is running anything that matches "logs", so the name is
        // genuinely user-set and must survive verbatim.
        assert!(!compute_automatic_rename(
            false,
            "logs",
            &["zsh".to_string(), "tail".to_string()],
        ));
    }

    #[test]
    fn lock_admits_one_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = tmp.path().join("saver.pid");
        assert!(claim_lock(&lock), "first claim should succeed");
        assert!(!claim_lock(&lock), "a held lock should reject a second claim");
        // A lock left by a dead process is reclaimable.
        std::fs::write(&lock, "4294967290").unwrap();
        assert!(claim_lock(&lock), "a stale lock should be reclaimable");
    }
}
