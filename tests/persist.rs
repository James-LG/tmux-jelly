//! End-to-end test for native session save/restore.
//!
//! Creates a real tmux session, snapshots it with `save_all_sessions`, tears it down,
//! then restores it with `try_restore_session` and checks the layout came back.
//! Requires `tmux`; skips gracefully when it is unavailable.

use std::collections::HashSet;
use std::process::{Command, Output};
use std::{env, fs};

use jelly::configs::Config;
use jelly::persist::{save_all_sessions, try_restore_session};
use jelly::tmux::Tmux;
use tempfile::tempdir;

fn tmux(socket: &str, args: &[&str]) -> Output {
    Command::new("tmux")
        .args(["-L", socket])
        .args(args)
        .output()
        .expect("failed to run tmux")
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn lines(out: Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn saves_and_restores_a_session() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }

    let socket = format!("jelly-persist-itest-{}", std::process::id());
    let tmp = tempdir().unwrap();
    let dir_a = tmp.path().join("a");
    let dir_b = tmp.path().join("b");
    fs::create_dir_all(&dir_a).unwrap();
    fs::create_dir_all(&dir_b).unwrap();

    env::remove_var("TMUX");
    env::set_var("JELLY_TMUX_SOCKET", &socket);
    let state_dir = tmp.path().join("state");
    env::set_var("XDG_STATE_HOME", &state_dir);

    // Build `proj`: window 0 has two panes (in dir_a and dir_b), window 1 has one.
    tmux(
        &socket,
        &[
            "new-session", "-d", "-s", "proj", "-x", "200", "-y", "50",
            "-c", dir_a.to_str().unwrap(),
        ],
    );
    tmux(&socket, &["split-window", "-t", "proj", "-c", dir_b.to_str().unwrap()]);
    tmux(&socket, &["new-window", "-t", "proj", "-c", dir_a.to_str().unwrap()]);

    let tmux_handle = Tmux::default();
    save_all_sessions(&tmux_handle);

    let saved_file = state_dir.join("jelly/sessions/proj.json");
    assert!(saved_file.exists(), "save file was not written");
    assert!(fs::read_to_string(&saved_file).unwrap().contains("\"proj\""));

    // Tear the session down, then restore it from the save file.
    tmux(&socket, &["kill-session", "-t", "proj"]);
    assert!(
        !tmux(&socket, &["has-session", "-t", "proj"]).status.success(),
        "proj should be gone before restore"
    );

    let config = Config {
        lazy_restore: Some(true),
        ..Default::default()
    };
    let restored = try_restore_session("proj", &config, &tmux_handle);
    assert!(restored, "try_restore_session should report success");

    // Expect 2 windows and 3 panes total (2 in the first window, 1 in the second).
    let pane_windows = lines(tmux(
        &socket,
        &["list-panes", "-s", "-t", "proj", "-F", "#{window_index}"],
    ));
    assert_eq!(pane_windows.len(), 3, "expected 3 panes total");
    let distinct_windows: HashSet<&String> = pane_windows.iter().collect();
    assert_eq!(distinct_windows.len(), 2, "expected 2 windows");

    // The restored panes should be back in their saved directories.
    let cwds = lines(tmux(
        &socket,
        &["list-panes", "-s", "-t", "proj", "-F", "#{pane_current_path}"],
    ));
    assert!(
        cwds.iter().any(|p| p.ends_with("/a")),
        "a pane should be restored in dir_a: {cwds:?}"
    );
    assert!(
        cwds.iter().any(|p| p.ends_with("/b")),
        "a pane should be restored in dir_b: {cwds:?}"
    );

    tmux(&socket, &["kill-server"]);
}
