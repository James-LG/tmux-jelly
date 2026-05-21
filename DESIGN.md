# Design Decisions

Notable decisions for the jelly fork. Each is recorded here and may be revisited.

## Session restore: layout only, no program restore

`lazy_restore` restores a session's **structure** — window names, pane working
directories, and pane layout — but does **not** re-run the programs that were
running in each pane. Restored panes come back as shells in the correct
directories.

Rationale: re-running arbitrary programs is unreliable. tmux only exposes a
pane's foreground process *name* (`pane_current_command`), not its full command
line or arguments, so a pane running `nvim foo.txt` could at best be relaunched
as bare `nvim`. Restoring directories + layout is the dependable subset and
covers the common need.

Status: **subject to change.** Best-effort program restore — e.g. an opt-in
allowlist of commands to relaunch — could be added later.

## Persistence: native, not tmux-resurrect

jelly saves and restores sessions entirely on its own (per-session JSON files
under `~/.local/state/jelly/sessions/`). It does not depend on, delegate to, or
import from tmux-resurrect.

Rationale: delegating to tmux-resurrect's scripts proved fragile — it depended
on the plugin being installed, its `restore.sh`, the `@resurrect-dir` option,
continuum's save timing, and a reboot to populate state. A self-contained
implementation is testable and predictable.

Status: settled.

## Interval saving: background helper process

To keep snapshots fresh between `jelly` runs, jelly starts a background helper
that saves every `save_interval` minutes — rather than hooking tmux's status
line (the tmux-continuum technique) or installing a systemd timer.

Rationale: jelly is a standalone binary, not a tmux plugin, so it has no plugin
init hook to maintain a status-line tick — and that tick only fires while a
client is attached and would modify the user's `status-right`. A systemd timer
is robust but Linux-only. A self-managed background process is cross-platform,
needs no tmux or OS configuration, works while detached, holds a single-instance
lock, and exits when the tmux server does.

Status: settled; the interval is user-configurable (`save_interval`, `0`
disables the helper).

## Save on session close: recommended, not auto-installed

The background helper only refreshes snapshots every `save_interval` minutes, so
state captured the instant a tmux server shuts down could be that stale. A tmux
`session-closed` hook running `jelly save` closes that gap by snapshotting
whenever any session ends.

jelly does **not** install this hook itself. Every other jelly/tmux integration
point — the `display-popup` keybindings, the `status-right` line — already lives
in the user's `tmux.conf` as explicit configuration; silently mutating the
running tmux server's hooks would be inconsistent with that and surprising. The
hook is instead documented as a recommended one-line addition (see the README),
sitting alongside the keybindings.

Rationale for the hook itself: a tmux server has no "server is exiting" hook, and
once the server is gone its sessions can no longer be queried. `session-closed`
is the latest point at which the *remaining* sessions can still be snapshotted.

Limitation: when the **last** session closes the server exits immediately, so
that final session's most recent state cannot be captured — it falls back to its
last interval snapshot.

Status: settled.
