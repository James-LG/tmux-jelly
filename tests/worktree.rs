//! End-to-end tests for the `worktree_sessions` feature: real git repositories with
//! real worktrees, discovered through `create_sessions`.

use std::process::Command;
use std::path::Path;

use tempfile::tempdir;
use jelly::configs::{Config, SearchDirectory};
use jelly::session::{create_sessions, SessionContainer};

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("failed to run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Create a repo with one commit on `main` plus a linked worktree on `feature`.
/// Returns the search-root directory containing the repo.
fn repo_with_worktree(root: &Path) {
    let repo = root.join("myrepo");
    std::fs::create_dir(&repo).unwrap();

    run_git(&repo, &["init", "-b", "main", "-q"]);
    run_git(&repo, &["config", "user.email", "test@example.com"]);
    run_git(&repo, &["config", "user.name", "Test"]);
    run_git(&repo, &["config", "commit.gpgsign", "false"]);
    run_git(&repo, &["commit", "--allow-empty", "-m", "init", "-q"]);
    // Linked worktree at <root>/myrepo-feature checked out on branch `feature`.
    run_git(&repo, &["worktree", "add", "-b", "feature", "../myrepo-feature", "-q"]);
}

fn config_for(root: &Path, worktree_sessions: Option<bool>) -> Config {
    Config {
        search_dirs: Some(vec![SearchDirectory::new(root.to_path_buf(), 1)]),
        worktree_sessions,
        ..Default::default()
    }
}

#[test]
fn worktrees_become_sessions_when_enabled() {
    let dir = tempdir().unwrap();
    repo_with_worktree(dir.path());

    let sessions = create_sessions(&config_for(dir.path(), Some(true)))
        .expect("create_sessions should succeed");
    let list = sessions.list();

    assert!(list.contains(&"myrepo".to_string()), "repo missing: {list:?}");
    assert!(
        list.contains(&"myrepo-feature".to_string()),
        "worktree session missing: {list:?}"
    );
}

#[test]
fn worktrees_are_hidden_when_disabled() {
    let dir = tempdir().unwrap();
    repo_with_worktree(dir.path());

    // `worktree_sessions` unset => identical to upstream: only the repo is listed.
    let sessions =
        create_sessions(&config_for(dir.path(), None)).expect("create_sessions should succeed");
    let list = sessions.list();

    assert!(list.contains(&"myrepo".to_string()), "repo missing: {list:?}");
    assert!(
        !list.iter().any(|s| s.starts_with("myrepo-")),
        "no worktree sessions expected when disabled: {list:?}"
    );
}
