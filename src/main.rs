use std::{collections::HashSet, env};

use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use error_stack::Report;

use jelly::{
    cli::{Cli, SubCommandGiven},
    configs::SessionSortOrderConfig,
    error::{Result, Suggestion},
    session::{create_sessions, SessionContainer},
    tmux::Tmux,
};

fn main() -> Result<()> {
    // Install debug hooks for formatting of error handling
    Report::install_debug_hook::<Suggestion>(|value, context| {
        context.push_body(format!("{value}"));
    });
    #[cfg(any(not(debug_assertions), test))]
    Report::install_debug_hook::<std::panic::Location>(|_value, _context| {});

    let bin_name = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_name().map(|exe| exe.to_string_lossy().to_string()))
        .unwrap_or("jelly".into());
    match CompleteEnv::with_factory(Cli::command)
        .bin(bin_name)
        .try_complete(env::args_os(), None)
    {
        Ok(true) => return Ok(()),
        Err(e) => {
            panic!("failed to generate completions: {e}");
        }
        Ok(false) => {}
    };

    // Use CLAP to parse the command line arguments
    let cli_args = Cli::parse();

    let tmux = Tmux::default();

    let config = match cli_args.handle_sub_commands(&tmux)? {
        SubCommandGiven::Yes => return Ok(()),
        SubCommandGiven::No(config) => config, // continue
    };

    // Snapshot live sessions so they can be restored later, then — on the first run
    // after a reboot (when `lazy_restore` is enabled) — close all open tmux sessions
    // so selecting one below restores just that session.
    if config.lazy_restore.unwrap_or(false) {
        jelly::persist::save_all_sessions(&tmux);
    }
    jelly::persist::handle_first_run(&config, &tmux);

    let sessions = create_sessions(&config)?;
    let (session_strings, active_sessions) = get_session_list(&sessions, &config, &tmux);

    // Create picker with active session styling
    let mut picker = jelly::picker::Picker::new(
        &session_strings,
        None,
        config.shortcuts.as_ref(),
        config.input_position.unwrap_or_default(),
        &tmux,
    )
    .set_colors(config.picker_colors.as_ref());

    if let Some(active) = active_sessions {
        picker = picker.set_active_sessions(active);
    }

    let selected_str = if let Some(str) = picker.run()? {
        str
    } else {
        return Ok(());
    };

    if let Some(session) = sessions.find_session(&selected_str) {
        // Ensure the background interval-saver is running before handing off.
        if config.lazy_restore.unwrap_or(false) {
            jelly::persist::ensure_save_daemon(&config);
        }
        session.switch_to(&tmux, &config)?;
    }

    Ok(())
}

/// Get the session list, optionally sorted with active sessions first
/// Returns (session_list, active_sessions_set)
fn get_session_list(
    sessions: &impl SessionContainer,
    config: &jelly::configs::Config,
    tmux: &Tmux,
) -> (Vec<String>, Option<HashSet<String>>) {
    let all_sessions = sessions.list();

    // If LastAttached is configured, prioritize active tmux sessions
    if matches!(
        config.session_sort_order,
        Some(SessionSortOrderConfig::LastAttached)
    ) {
        // Get active sessions from tmux with timestamps, excluding the currently attached one
        let active_sessions_raw =
            tmux.list_sessions("'#{?session_attached,,#{session_name}#,#{session_last_attached}}'");

        // Parse into (name, timestamp) pairs
        let active_sessions: Vec<(&str, i64)> = active_sessions_raw
            .trim()
            .split('\n')
            .filter_map(|line| {
                let line = line.trim_matches('\'');
                let (name, timestamp) = line.split_once(',')?;
                let timestamp = timestamp.parse::<i64>().ok()?;
                Some((name, timestamp))
            })
            .collect();

        // Build a set of active session names for fast lookup
        let active_names: HashSet<&str> = active_sessions.iter().map(|(name, _)| *name).collect();
        let active_names_owned: HashSet<String> =
            active_names.iter().map(|s| s.to_string()).collect();

        // Partition sessions into active and inactive
        let (mut active_list, mut inactive_list): (Vec<String>, Vec<String>) =
            all_sessions.into_iter().partition(|session_name| {
                // Check if this session name (or its normalized form) is active
                // Tmux normalizes both dots and hyphens to underscores in session names
                let normalized = session_name.replace(['.', '-'], "_");
                active_names.contains(session_name.as_str())
                    || active_names.contains(&normalized.as_str())
            });

        // Sort active sessions by timestamp (most recent first)
        active_list.sort_by_cached_key(|name| {
            // Find the timestamp for this session
            // Tmux normalizes both dots and hyphens to underscores
            let normalized = name.replace(['.', '-'], "_");
            active_sessions
                .iter()
                .find(|(active_name, _)| *active_name == name || *active_name == normalized)
                .map(|(_, timestamp)| -timestamp) // Negative for descending order
                .unwrap_or(0)
        });

        // Sort inactive sessions alphabetically
        inactive_list.sort();

        // Combine: active first, then inactive
        active_list.extend(inactive_list);
        (active_list, Some(active_names_owned))
    } else {
        // Default behavior: alphabetically sorted
        (all_sessions, None)
    }
}
