//! Live resource-usage chart.
//!
//! `jelly usage` opens a TUI that charts CPU and memory usage **by session** and,
//! within each session, the **processes** running in its panes.
//!
//! Usage is sampled by pairing every tmux pane with its root process
//! (`pane_pid`), walking the process tree to collect that pane's descendants, and
//! reading per-process CPU/memory from `ps`. Everything below a session's panes is
//! attributed to that session.
//!
//! This is a read-only monitor; the only side effect is the optional `Enter` key,
//! which switches to the highlighted session (exactly like picking it in the
//! switcher).

use std::{
    collections::HashMap,
    process::Command,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Bar, BarChart, BarGroup, Block, HighlightSpacing, List, ListItem, ListState, Paragraph,
    },
    DefaultTerminal, Frame,
};

use crate::{error::TmsError, tmux::Tmux, Result};

/// How often the session/process sample is refreshed.
const REFRESH: Duration = Duration::from_secs(2);

/// Longest a single `event::poll` waits before the loop ticks again.
const TICK: Duration = Duration::from_millis(200);

/// Most processes listed under a single session before the rest are summarized.
const MAX_PROCS: usize = 6;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// Which resource the chart is currently keyed on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Cpu,
    Mem,
}

impl Metric {
    fn label(self) -> &'static str {
        match self {
            Metric::Cpu => "CPU",
            Metric::Mem => "Memory",
        }
    }

    /// Base color used for this metric's bars and figures.
    fn color(self) -> Color {
        match self {
            Metric::Cpu => Color::Yellow,
            Metric::Mem => Color::Cyan,
        }
    }
}

/// A single process running inside a tmux session.
#[derive(Clone)]
struct Process {
    cpu: f32,
    mem_kb: u64,
    command: String,
}

impl Process {
    fn metric(&self, metric: Metric) -> f64 {
        match metric {
            Metric::Cpu => self.cpu as f64,
            Metric::Mem => self.mem_kb as f64,
        }
    }
}

/// Aggregated resource usage for one tmux session.
struct SessionUsage {
    name: String,
    cpu: f32,
    mem_kb: u64,
    processes: Vec<Process>,
}

impl SessionUsage {
    fn metric(&self, metric: Metric) -> f64 {
        match metric {
            Metric::Cpu => self.cpu as f64,
            Metric::Mem => self.mem_kb as f64,
        }
    }

    /// The metric as the integer a [`BarChart`] bar expects. CPU is scaled to
    /// tenths of a percent so fractional usage still moves the bar.
    fn bar_value(&self, metric: Metric) -> u64 {
        match metric {
            Metric::Cpu => (self.cpu as f64 * 10.0).round() as u64,
            Metric::Mem => self.mem_kb,
        }
    }

    /// Human-readable figure for the active metric.
    fn metric_display(&self, metric: Metric) -> String {
        match metric {
            Metric::Cpu => format_cpu(self.cpu),
            Metric::Mem => format_mem(self.mem_kb),
        }
    }
}

/// A process row sampled from `ps`, before it is attributed to a session.
struct ProcRow {
    ppid: u32,
    cpu: f32,
    mem_kb: u64,
    command: String,
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

/// Snapshot every process on the machine via `ps`, keyed by pid.
///
/// The columns (`pid ppid pcpu rss comm`) are POSIX-portable, so this works on
/// both Linux and macOS. A failure to run `ps` yields an empty map, which the
/// caller renders as "no sessions" rather than crashing.
fn sample_processes() -> HashMap<u32, ProcRow> {
    let mut rows = HashMap::new();

    let output = Command::new("ps")
        .args([
            "-A", "-o", "pid=", "-o", "ppid=", "-o", "pcpu=", "-o", "rss=", "-o", "comm=",
        ])
        .output();
    let Ok(output) = output else {
        return rows;
    };

    // jelly's own `ps` probe always reports ~100% CPU (a `pcpu` artifact: a
    // just-started process has a cputime/realtime ratio near 1), which would
    // inflate whichever session jelly runs in. Drop it from the sample.
    let own_pid = std::process::id();

    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(cpu), Some(rss)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // `comm` is last and may itself contain spaces, so it claims the remainder.
        let command = basename(&fields.collect::<Vec<_>>().join(" "));

        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        if ppid == own_pid && command == "ps" {
            continue;
        }
        rows.insert(
            pid,
            ProcRow {
                ppid,
                cpu: cpu.parse().unwrap_or(0.0),
                mem_kb: rss.parse().unwrap_or(0),
                command,
            },
        );
    }

    rows
}

/// Parse `list-panes` output into `(session, pane_pid)` pairs.
fn parse_panes(raw: &str) -> Vec<(String, u32)> {
    raw.lines()
        .filter_map(|line| {
            let (name, pid) = line.split_once('\t')?;
            let pid = pid.trim().parse::<u32>().ok()?;
            (!name.is_empty()).then(|| (name.to_string(), pid))
        })
        .collect()
}

/// Attribute every process to a session by walking each pane's process subtree.
///
/// Each pane contributes its root process plus all descendants; a process is
/// counted once, so sibling panes of the same session never double-count.
fn build_sessions(procs: &HashMap<u32, ProcRow>, panes: &[(String, u32)]) -> Vec<SessionUsage> {
    // Reverse the parent links once so subtree walks are cheap.
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, row) in procs {
        children.entry(row.ppid).or_default().push(pid);
    }

    // Group pane roots by their session.
    let mut roots: HashMap<&str, Vec<u32>> = HashMap::new();
    for (name, pid) in panes {
        roots.entry(name.as_str()).or_default().push(*pid);
    }

    let mut claimed: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut sessions: Vec<SessionUsage> = Vec::new();

    for (name, pane_pids) in roots {
        let mut processes: Vec<Process> = Vec::new();
        let mut stack = pane_pids;
        while let Some(pid) = stack.pop() {
            if !claimed.insert(pid) {
                continue;
            }
            if let Some(row) = procs.get(&pid) {
                processes.push(Process {
                    cpu: row.cpu,
                    mem_kb: row.mem_kb,
                    command: row.command.clone(),
                });
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids);
            }
        }

        sessions.push(SessionUsage {
            name: name.to_string(),
            cpu: processes.iter().map(|p| p.cpu).sum(),
            mem_kb: processes.iter().map(|p| p.mem_kb).sum(),
            processes,
        });
    }

    sessions
}

/// Take a fresh sample of every tmux session and its processes.
fn collect(tmux: &Tmux) -> Vec<SessionUsage> {
    let procs = sample_processes();
    let panes = parse_panes(&tmux.list_all_panes("#{session_name}\t#{pane_pid}"));
    build_sessions(&procs, &panes)
}

/// Sort sessions, and the processes within each, by the active metric (largest
/// first) so the chart and the breakdown read top-down.
fn sort_by_metric(sessions: &mut [SessionUsage], metric: Metric) {
    sessions.sort_by(|a, b| {
        b.metric(metric)
            .total_cmp(&a.metric(metric))
            .then_with(|| a.name.cmp(&b.name))
    });
    for session in sessions.iter_mut() {
        session
            .processes
            .sort_by(|a, b| b.metric(metric).total_cmp(&a.metric(metric)));
    }
}

// ---------------------------------------------------------------------------
// Entry point / event loop
// ---------------------------------------------------------------------------

/// Run the `jelly usage` TUI.
pub fn usage_command(tmux: &Tmux) -> Result<()> {
    let mut terminal = ratatui::init();
    let outcome = run(&mut terminal, tmux);
    ratatui::restore();

    // The terminal is restored before switching so the handed-off session draws
    // onto a clean screen.
    if let Some(session) = outcome.map_err(|e| TmsError::TuiError(e.to_string()))? {
        tmux.switch_to_session(&session);
    }
    Ok(())
}

/// Drive the draw/refresh/input loop. Returns the session to switch to, if the
/// user pressed `Enter`.
fn run(terminal: &mut DefaultTerminal, tmux: &Tmux) -> std::io::Result<Option<String>> {
    let mut metric = Metric::Cpu;
    let mut sessions = collect(tmux);
    let mut selected = 0usize;
    let mut last_refresh = Instant::now();

    loop {
        if last_refresh.elapsed() >= REFRESH {
            sessions = collect(tmux);
            last_refresh = Instant::now();
        }
        sort_by_metric(&mut sessions, metric);
        selected = selected.min(sessions.len().saturating_sub(1));

        terminal.draw(|f| render(f, &sessions, metric, selected))?;

        if !event::poll(TICK)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        let count = sessions.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') => metric = Metric::Cpu,
            KeyCode::Char('m') => metric = Metric::Mem,
            KeyCode::Tab => {
                metric = match metric {
                    Metric::Cpu => Metric::Mem,
                    Metric::Mem => Metric::Cpu,
                }
            }
            KeyCode::Char('r') => {
                sessions = collect(tmux);
                last_refresh = Instant::now();
            }
            KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                selected = (selected + 1) % count;
            }
            KeyCode::Up | KeyCode::Char('k') if count > 0 => {
                selected = (selected + count - 1) % count;
            }
            KeyCode::Enter => {
                if let Some(session) = sessions.get(selected) {
                    return Ok(Some(session.name.clone()));
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(f: &mut Frame, sessions: &[SessionUsage], metric: Metric, selected: usize) {
    let area = f.area();
    if sessions.is_empty() {
        render_empty(f, area);
        return;
    }

    let chunks = Layout::new(
        Direction::Vertical,
        [
            Constraint::Percentage(45),
            Constraint::Min(3),
            Constraint::Length(1),
        ],
    )
    .split(area);

    render_chart(f, chunks[0], sessions, metric, selected);
    render_breakdown(f, chunks[1], sessions, metric, selected);
    render_footer(f, chunks[2], metric);
}

/// The headline: a bar chart with one bar per session.
fn render_chart(
    f: &mut Frame,
    area: Rect,
    sessions: &[SessionUsage],
    metric: Metric,
    selected: usize,
) {
    let n = sessions.len() as u16;
    let inner_w = area.width.saturating_sub(2);
    let bar_width = (inner_w.saturating_sub(n.saturating_sub(1)) / n.max(1)).clamp(1, 14);

    let max = sessions
        .iter()
        .map(|s| s.bar_value(metric))
        .max()
        .unwrap_or(1)
        .max(1);

    let bars: Vec<Bar> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let color = if i == selected {
                Color::White
            } else {
                metric.color()
            };
            Bar::default()
                .value(s.bar_value(metric))
                .label(Line::from(truncate(&s.name, (bar_width as usize).max(6))))
                .text_value(s.metric_display(metric))
                .style(Style::default().fg(color))
        })
        .collect();

    let chart = BarChart::default()
        .block(
            Block::bordered()
                .title(format!(" Resource usage by session · {} ", metric.label())),
        )
        .data(BarGroup::default().bars(&bars))
        .bar_width(bar_width)
        .bar_gap(1)
        .max(max)
        .value_style(
            Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .label_style(Style::default().fg(Color::Gray));

    f.render_widget(chart, area);
}

/// The per-session breakdown: a session header followed by its processes, each
/// with an inline bar showing its share of that session's usage.
fn render_breakdown(
    f: &mut Frame,
    area: Rect,
    sessions: &[SessionUsage],
    metric: Metric,
    selected: usize,
) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut header_rows: Vec<usize> = Vec::new();

    for session in sessions {
        header_rows.push(items.len());
        items.push(session_header(session, metric));

        let total = session.metric(metric);
        for proc in session.processes.iter().take(MAX_PROCS) {
            items.push(process_row(proc, metric, total));
        }
        let hidden = session.processes.len().saturating_sub(MAX_PROCS);
        if hidden > 0 {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("      … {hidden} more"),
                Style::default().fg(Color::DarkGray),
            ))));
        }
    }

    let list = List::new(items)
        .block(Block::bordered().title(" Processes by session "))
        .highlight_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )
        .highlight_symbol("▶ ")
        .highlight_spacing(HighlightSpacing::Always);

    let mut state = ListState::default();
    state.select(header_rows.get(selected).copied());
    f.render_stateful_widget(list, area, &mut state);
}

/// One session header line: `name   CPU 12.3%   MEM 1.2 GB   3 proc`.
fn session_header(session: &SessionUsage, metric: Metric) -> ListItem<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let cpu_style = if metric == Metric::Cpu {
        Style::default().fg(Metric::Cpu.color())
    } else {
        dim
    };
    let mem_style = if metric == Metric::Mem {
        Style::default().fg(Metric::Mem.color())
    } else {
        dim
    };

    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:<22}", truncate(&session.name, 22)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("CPU {:>6}", format_cpu(session.cpu)), cpu_style),
        Span::raw("   "),
        Span::styled(format!("MEM {:>9}", format_mem(session.mem_kb)), mem_style),
        Span::styled(format!("   {} proc", session.processes.len()), dim),
    ]))
}

/// One process line, indented under its session, with a share-of-session bar.
fn process_row(proc: &Process, metric: Metric, session_total: f64) -> ListItem<'static> {
    const BAR_W: usize = 14;
    let ratio = if session_total > 0.0 {
        (proc.metric(metric) / session_total).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = (ratio * BAR_W as f64).round() as usize;

    let cpu_style = metric_figure_style(metric, Metric::Cpu);
    let mem_style = metric_figure_style(metric, Metric::Mem);

    ListItem::new(Line::from(vec![
        Span::raw("    "),
        Span::styled(
            format!("{:<18}", truncate(&proc.command, 18)),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            "█".repeat(filled),
            Style::default().fg(metric.color()),
        ),
        Span::styled(
            "░".repeat(BAR_W - filled),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(format!("  {:>6}", format_cpu(proc.cpu)), cpu_style),
        Span::styled(format!("  {:>9}", format_mem(proc.mem_kb)), mem_style),
    ]))
}

/// Color a figure brightly when it is the active metric, dim otherwise.
fn metric_figure_style(active: Metric, figure: Metric) -> Style {
    if active == figure {
        Style::default().fg(figure.color())
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn render_footer(f: &mut Frame, area: Rect, metric: Metric) {
    let key = Style::default().fg(Color::Black).bg(Color::Gray);
    let mut spans = vec![
        Span::styled(" q ", key),
        Span::raw(" quit   "),
        Span::styled(" ↑↓ ", key),
        Span::raw(" select   "),
        Span::styled(" ⏎ ", key),
        Span::raw(" switch   "),
    ];
    for (m, ch) in [(Metric::Cpu, " c "), (Metric::Mem, " m ")] {
        let style = if m == metric {
            Style::default().fg(Color::Black).bg(m.color())
        } else {
            key
        };
        spans.push(Span::styled(ch, style));
        spans.push(Span::raw(format!(" {}   ", m.label())));
    }
    spans.push(Span::styled(" r ", key));
    spans.push(Span::raw(" refresh"));

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_empty(f: &mut Frame, area: Rect) {
    let rows = Layout::new(
        Direction::Vertical,
        [
            Constraint::Percentage(50),
            Constraint::Length(2),
            Constraint::Min(0),
        ],
    )
    .split(area);

    let message = Paragraph::new(vec![
        Line::from("No tmux sessions are running."),
        Line::from(Span::styled(
            "Press q to quit.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .alignment(Alignment::Center);
    f.render_widget(message, rows[1]);
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// Reduce a `comm` value to its final path component (`/usr/bin/nvim` -> `nvim`).
fn basename(command: &str) -> String {
    let command = command.trim();
    match command.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name.to_string(),
        _ => command.to_string(),
    }
}

/// Shorten `s` to at most `max` characters, marking any cut with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn format_cpu(percent: f32) -> String {
    format!("{percent:.1}%")
}

/// Render a KiB count as `KB` / `MB` / `GB`, switching unit at 1000.
fn format_mem(kb: u64) -> String {
    let kb_f = kb as f64;
    if kb < 1000 {
        format!("{kb} KB")
    } else if kb_f < 1000.0 * 1024.0 {
        format!("{:.0} MB", kb_f / 1024.0)
    } else {
        format!("{:.1} GB", kb_f / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(ppid: u32, cpu: f32, mem_kb: u64, command: &str) -> ProcRow {
        ProcRow {
            ppid,
            cpu,
            mem_kb,
            command: command.to_string(),
        }
    }

    #[test]
    fn basename_keeps_plain_names_and_strips_paths() {
        assert_eq!(basename("nvim"), "nvim");
        assert_eq!(basename("/usr/bin/cargo"), "cargo");
        assert_eq!(basename("/Applications/Foo.app/Contents/MacOS/foo"), "foo");
        assert_eq!(basename("trailing/"), "trailing/");
    }

    #[test]
    fn truncate_marks_cuts_with_an_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a-very-long-name", 6), "a-ver…");
    }

    #[test]
    fn format_mem_switches_units_at_1000() {
        assert_eq!(format_mem(0), "0 KB");
        assert_eq!(format_mem(512), "512 KB");
        assert_eq!(format_mem(2048), "2 MB");
        assert_eq!(format_mem(2 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn parse_panes_drops_malformed_lines() {
        let raw = "alpha\t100\nbeta\t200\nbroken-line\ngamma\tnot-a-pid\n";
        assert_eq!(
            parse_panes(raw),
            vec![("alpha".to_string(), 100), ("beta".to_string(), 200)]
        );
    }

    #[test]
    fn build_sessions_attributes_the_whole_process_subtree() {
        // pane root 100 -> shell; 100 spawns 101 (nvim), 101 spawns 102 (lsp).
        let mut procs = HashMap::new();
        procs.insert(100, proc(1, 1.0, 1000, "zsh"));
        procs.insert(101, proc(100, 10.0, 4000, "nvim"));
        procs.insert(102, proc(101, 5.0, 2000, "rust-analyzer"));
        // An unrelated process must not leak into the session.
        procs.insert(900, proc(1, 99.0, 99000, "unrelated"));

        let sessions = build_sessions(&procs, &[("work".to_string(), 100)]);

        assert_eq!(sessions.len(), 1);
        let work = &sessions[0];
        assert_eq!(work.name, "work");
        assert_eq!(work.processes.len(), 3);
        assert!((work.cpu - 16.0).abs() < f32::EPSILON);
        assert_eq!(work.mem_kb, 7000);
    }

    #[test]
    fn build_sessions_counts_each_process_once_across_panes() {
        // Two panes in the same session, each its own shell subtree.
        let mut procs = HashMap::new();
        procs.insert(100, proc(1, 1.0, 1000, "zsh"));
        procs.insert(101, proc(100, 2.0, 2000, "vim"));
        procs.insert(200, proc(1, 3.0, 3000, "zsh"));

        let sessions = build_sessions(
            &procs,
            &[("dev".to_string(), 100), ("dev".to_string(), 200)],
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].processes.len(), 3);
        assert_eq!(sessions[0].mem_kb, 6000);
    }
}
