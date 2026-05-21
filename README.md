# tmux-jelly

The fastest way to manage projects as tmux sessions

## What is jelly?

`jelly` is a tmux session manager — a fast Rust TUI that fuzzy-finds the git
repositories on your machine and opens each one as a tmux session. It is a fork of
[`tmux-sessionizer`](https://github.com/jrmoulton/tmux-sessionizer) by jrmoulton,
itself based on ThePrimeagen's
[tmux-sessionizer](https://github.com/ThePrimeagen/.dotfiles/blob/master/bin/.local/scripts/tmux-sessionizer)
script.

Git worktrees can be opened as new windows, specific directories can be excluded, a
default session can be set, and killing a session jumps you to a default.

Tmux has keybindings built-in to allow you to switch between sessions. By default,
these are `leader-(` and `leader-)`. Switching between windows is done by default
with `leader-p` and `leader-n`.

![jelly-gif](images/tms-v0_1_1.gif)

## What's different from tmux-sessionizer

jelly is a drop-in fork — every new feature is behind a config toggle that defaults
to off, so with nothing enabled jelly behaves identically to upstream
`tmux-sessionizer`. Two opt-in features set it apart:

**🪟 Worktree sessions** — `jelly config --worktree-sessions true` makes each git
worktree appear in the picker as its own `[repo]-[branch]` session instead of a
window inside the repo's session, so you can jump straight to a specific worktree.
The repository still appears as a plain `[repo]` entry. ([details](#worktree-sessions))

**💾 Native lazy session restore** — `jelly config --lazy-restore true` makes jelly
save and restore tmux sessions on its own, with no tmux-resurrect or tmux-continuum
plugins. It snapshots session layout (window names, pane working directories, pane
layout) to `~/.local/state/jelly/sessions/` and recreates a session from its
snapshot the moment you pick it — restoring just that one session, not everything.
([details](#lazy-session-restore))

## Usage

### The `jelly` command

Running `jelly` will find the repos and fuzzy find on them. It is very convenient to bind the jelly
command to a tmux keybinding so that you don't have to leave your text editor to open a new project.
I have this tmux binding `bind C-o display-popup -E "jelly"`. See the image below for what this look
like with the `jelly switch` keybinding

### The `jelly switch` command

There is also the `jelly switch` command that will show other active sessions with a fuzzy finder and
a preview window. This can be very useful when used with the tmux `display-popup` which can open a
popup window above the current session. That popup window with a command can have a keybinding. The
config could look like this `bind C-j display-popup -E "jelly switch"`. Then when using leader+C-j the
popup is displayed (and it's fast)

![jelly-switch](images/tms_switch-v2_1.png)

### The `jelly windows` command

Similar to `jelly switch`, you can show other active windows in the current session with a fuzzy
finder and a preview window. A config for use with `display-popup`, could look like this
`bind C-w display-popup -E "jelly windows"`.

### The `jelly rename` command

Using this command you can automatically rename the active session along with the directory name and
the active directory inside all the panes in the active session will be changed to the renamed
directory

`jelly rename <new_session_name>`

`bind C-w command-prompt -p "Rename active session to: " "run-shell 'jelly rename %1'"`.

### The `jelly refresh` command

Using this command you can automatically generate missing worktree windows for the active session or
a provided `session_name`.

`jelly refresh <session_name>`

`bind C-r "run-shell 'jelly refresh'"`.

### The `jelly kill` command

Using this command you can kill current tmux session and automatically jump to another. The config
could look like this `bind C-k confirm -p "Kill current session? (y/N):" "run-shell 'jelly kill'"`.
Then when using leader+C-k you have to confirm killing of the current session with `y`. Any other
input or just pressing enter aborts it.

With `jelly config --session <name>` you can define to which session you will switch to after the
kill.

### CLI overview

Use `jelly --help`

```
Scan for all git folders in specified directorires, select one and open it as a new tmux session

Usage: jelly [COMMAND]

Commands:
  config        Configure the defaults for search paths and excluded directories
  start         Initialize tmux with the default sessions
  switch        Display other sessions with a fuzzy finder and a preview window
  windows       Display the current session's windows with a fuzzy finder and a preview window
  kill          Kill the current tmux session and jump to another
  sessions      Show running tmux sessions with asterisk on the current session
  save          Snapshot all live tmux sessions so they can be lazily restored later
  rename        Rename the active session and the working directory
  refresh       Creates new worktree windows for the selected session
  clone-repo    Clone repository and create a new session for it
  init-repo     Initialize empty repository
  bookmark      Bookmark a directory so it is available to select along with the Git repositories
  open-session  Open a session
  marks         Manage list of sessions that can be instantly accessed by their index
  help          Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

### Configuring defaults

```
Configure the defaults for search paths and excluded directories

Usage: jelly config [OPTIONS]

Options:
  -p, --paths <search paths>...
          The paths to search through. Shell like expansions such as '~' are supported
  -s, --session <default session>
          The default session to switch to (if available) when killing another session
      --excluded <excluded dirs>...
          As many directory names as desired to not be searched over
      --remove <remove dir>...
          As many directory names to be removed from exclusion list
      --full-path <true | false>
          Use the full path when displaying directories [possible values: true, false]
      --search-submodules <true | false>
          Also show initialized submodules [possible values: true, false]
      --recursive-submodules <true | false>
          Search submodules for submodules [possible values: true, false]
      --switch-filter-unknown <true | false>
          Only include sessions from search paths in the switcher [possible values: true, false]
  -d, --max-depths <max depth>...
          The maximum depth to traverse when searching for repositories in search paths, length should match the number of search paths if specified (defaults to 10)
      --picker-highlight-color <#rrggbb>
          Background color of the highlighted item in the picker
      --picker-highlight-text-color <#rrggbb>
          Text color of the hightlighted item in the picker
      --picker-border-color <#rrggbb>
          Color of the borders between widgets in the picker
      --picker-info-color <#rrggbb>
          Color of the item count in the picker
      --picker-prompt-color <#rrggbb>
          Color of the prompt in the picker
      --session-sort-order <Alphabetical | LastAttach>
          Set the sort order of the sessions in the switch command [possible values: Alphabetical, LastAttached]
      --worktree-sessions <true | false>
          Show each git worktree as its own session named [repo]-[branch] [possible values: true, false]
      --lazy-restore <true | false>
          Lazy session restore [possible values: true, false]
      --save-interval <minutes>
          Minutes between automatic background session snapshots (0 disables it)
  -h, --help
          Print help
```

#### Worktree sessions

By default `jelly` opens a repository's git worktrees as windows inside that
repository's session. With `jelly config --worktree-sessions true` each worktree
instead appears in the picker as its own session named `[repo]-[branch]`, so you
can jump straight to a specific worktree. The repository itself still appears as
a plain `[repo]` entry. When this option is off, behavior is unchanged.

#### Lazy session restore

`jelly config --lazy-restore true` makes `jelly` remember and restore tmux
sessions itself — no plugins required.

While enabled, jelly snapshots the layout of all live tmux sessions (window
names, pane working directories, and pane layout) to
`~/.local/state/jelly/sessions/`. Snapshots happen on every `jelly` run, on
demand with `jelly save`, and — so state stays fresh between runs — from a
background helper that jelly starts automatically and that snapshots every
`save_interval` minutes (default 15; set `jelly config --save-interval 0` to
disable it). The helper runs at most one copy at a time and exits when the tmux
server does. For an extra snapshot the instant a session closes, add the
recommended `session-closed` hook (see below).

Selecting a session that is not currently running recreates it from its saved
snapshot — just that one session — instead of starting empty. The first `jelly`
run after a reboot additionally closes every open tmux session, giving a clean
slate so sessions come back one at a time as you pick them.

When this option is off, behavior is unchanged.

> Note: the post-reboot clean slate closes all sessions. If you run `jelly` from
> inside tmux, the session you are attached to is left open so `jelly` is not
> terminated.

##### Recommended: snapshot when a session closes

The background helper only refreshes snapshots every `save_interval` minutes. To
also capture one the instant any session closes — keeping saved state current
right up to server shutdown — add a `session-closed` hook to your `tmux.conf`,
alongside the `jelly` keybindings:

```tmux
# jelly keybindings
bind C-o display-popup -E "jelly"
bind C-j display-popup -E "jelly switch"
bind C-w display-popup -E "jelly windows"

# snapshot sessions the moment one closes (lazy restore)
set-hook -g session-closed 'run-shell -b "jelly save"'
```

jelly does not install this hook automatically; like the keybindings, it is left
as explicit tmux configuration. When the *last* session closes the tmux server
exits immediately, so that final session falls back to its last interval
snapshot.

#### Config file location

By default, jelly looks for a configuration in the platform-specific config directory:

```
Linux: /home/alice/.config/jelly/config.toml
macOS: /Users/Alice/Library/Application Support/jelly/config.toml
Windows: C:\Users\Alice\AppData\Roaming\jelly\config.toml
```

If the config directory can't be found, it will also check `~/.config/jelly/config.toml` (only
relevant on Windows and macOS). Alternatively, you can specify a custom config location by setting
the `JELLY_CONFIG_FILE` environment variable in your shell profile with your desired config path.

#### Customizing keyboard shortcuts

Keyboard shortcuts can be customized by adding a `[shortcuts]` section in the config file and adding
bindings as pairs of `shortcut = action`, for example:

```
[shortcuts]
"ctrl-k" = "delete_to_line_end"
```

Available actions are:

- "" (to remove a default binding)
- "cancel"
- "confirm"
- "backspace"
- "delete"
- "move_up"
- "move_down"
- "cursor_left"
- "cursor_right"
- "delete_word"
- "delete_to_line_start"
- "delete_to_line_end"
- "move_to_line_start"
- "move_to_line_end"

## Installation

jelly is built from source — clone the repository and install with cargo:

```sh
git clone https://github.com/James-LG/tmux-jelly.git
cd tmux-jelly
cargo install --path . --force
```

This installs the `jelly` binary to `~/.cargo/bin`. To update later, pull the latest
changes and rerun the `cargo install` command.

## Usage Notes

The 'jelly sessions' command can be used to get a styled output of the active sessions with an
asterisk on the current session. The configuration would look something like this

```
set -g status-right " #(jelly sessions)"
```

E.g. ![tmux status bar](images/tmux-status-bar.png) If this configuration is used it can be helpful
to rebind the default tmux keys for switching sessions so that the status bar is refreshed on every
session switch. This can be configured with settings like this.

```
bind -r '(' switch-client -p\; refresh-client -S
bind -r ')' switch-client -n\; refresh-client -S
```
 
## Shell completions

### Bash
```bash
echo "source <(COMPLETE=bash jelly)" >> ~/.bashrc
```

### Zsh
```zsh
echo "source <(COMPLETE=zsh jelly)" >> ~/.zshrc
```

### Fish
```fish
echo "COMPLETE=fish jelly | source" >> ~/.config/fish/config.fish
```
