//! `onedrive-rc` with no subcommand: the ratatui dashboard.
//!
//! Five panes (`Tab`/`Shift+Tab` to switch): Overview, Pins, Conflicts,
//! Outbox, Log. `r` refreshes now; refresh also happens on its own every
//! [`REFRESH_INTERVAL`]. `Enter` acts on the selected row — the one safe,
//! un-ambiguous action for that pane (closing a conflict with both versions
//! kept, or retrying one remote's outbox) — never anything that could lose
//! data, since there is no confirmation dialog here to second-guess a worse
//! choice.
//!
//! Reads the same way every other command does: [`client::status`] already
//! falls back to the state database when the daemon is not answering, so the
//! Overview pane degrades the same way `onedrive-rc status` does. Conflicts
//! and Outbox are IPC-only (there is no read-only database fallback for
//! them), so a down daemon just shows them empty rather than stale or wrong.

use std::io::Stdout;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use onedrive_rc_core::ipc::{ConflictInfo, Keep, OutboxInfo, PinStatus, Request, Response};
use onedrive_rc_core::paths::Paths;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::{Frame, text::Line};

use crate::{client, log_cmd, render};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const LOG_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Overview,
    Pins,
    Conflicts,
    Outbox,
    Log,
}

const PANES: [Pane; 5] = [
    Pane::Overview,
    Pane::Pins,
    Pane::Conflicts,
    Pane::Outbox,
    Pane::Log,
];

impl Pane {
    fn index(self) -> usize {
        PANES.iter().position(|p| *p == self).expect("in PANES")
    }

    fn next(self) -> Pane {
        PANES[(self.index() + 1) % PANES.len()]
    }

    fn prev(self) -> Pane {
        PANES[(self.index() + PANES.len() - 1) % PANES.len()]
    }

    fn title(self) -> &'static str {
        match self {
            Pane::Overview => "Overview",
            Pane::Pins => "Pins",
            Pane::Conflicts => "Conflicts",
            Pane::Outbox => "Outbox",
            Pane::Log => "Log",
        }
    }
}

/// How far down the currently selected row moves `List` selection, clamped to
/// what actually exists — pure, so the clamping is tested without a terminal.
fn move_selection(selected: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let max = len - 1;
    let next = selected as i32 + delta;
    next.clamp(0, max as i32) as usize
}

/// How many rows the currently active pane has to select among. `Overview`
/// and `Log` are not selectable, hence zero.
fn selectable_len(
    pane: Pane,
    pins: &[PinStatus],
    conflicts: &[ConflictInfo],
    outbox: &[OutboxInfo],
) -> usize {
    match pane {
        Pane::Pins => pins.len(),
        Pane::Conflicts => conflicts.len(),
        Pane::Outbox => outbox.len(),
        Pane::Overview | Pane::Log => 0,
    }
}

/// What pressing `Enter` on the selected row should ask the daemon to do.
/// `None` for panes with nothing to act on, or an out-of-range selection —
/// pure and total, so every case is testable without a daemon or a terminal.
fn enter_request(
    pane: Pane,
    selected: usize,
    conflicts: &[ConflictInfo],
    outbox: &[OutboxInfo],
) -> Option<Request> {
    match pane {
        Pane::Conflicts => conflicts.get(selected).map(|c| Request::ConflictResolve {
            id: c.id,
            keep: Keep::Both,
        }),
        Pane::Outbox => outbox.get(selected).map(|o| Request::OutboxRetry {
            remote: Some(o.remote.clone()),
        }),
        Pane::Overview | Pane::Pins | Pane::Log => None,
    }
}

struct State {
    pane: Pane,
    selected: usize,
    status: Option<onedrive_rc_core::ipc::StatusReport>,
    conflicts: Vec<ConflictInfo>,
    outbox: Vec<OutboxInfo>,
    log: Vec<String>,
    message: Option<String>,
    last_refresh: Instant,
}

impl State {
    fn new() -> Self {
        Self {
            pane: Pane::Overview,
            selected: 0,
            status: None,
            conflicts: Vec::new(),
            outbox: Vec::new(),
            log: Vec::new(),
            message: None,
            // Due immediately: the first draw must not wait out a full interval.
            last_refresh: Instant::now() - REFRESH_INTERVAL,
        }
    }

    fn refresh(&mut self, paths: &Paths) {
        self.status = client::status(paths).ok();
        self.conflicts = match client::try_request(paths, &Request::ConflictList) {
            Ok(Some(Response::Conflicts { conflicts })) => conflicts,
            _ => Vec::new(),
        };
        self.outbox = match client::try_request(paths, &Request::OutboxList) {
            Ok(Some(Response::Outbox { rows })) => rows,
            _ => Vec::new(),
        };
        self.log = log_cmd::tail(paths, LOG_LINES).unwrap_or_default();
        let pins = self
            .status
            .as_ref()
            .map(|s| s.pins.as_slice())
            .unwrap_or(&[]);
        self.selected = move_selection(
            self.selected,
            selectable_len(self.pane, pins, &self.conflicts, &self.outbox),
            0,
        );
        self.last_refresh = Instant::now();
    }

    fn switch_pane(&mut self, pane: Pane) {
        self.pane = pane;
        self.selected = 0;
        self.message = None;
    }

    fn act(&mut self, paths: &Paths) {
        let Some(req) = enter_request(self.pane, self.selected, &self.conflicts, &self.outbox)
        else {
            return;
        };
        self.send(paths, &req);
    }

    /// Send one request and show the daemon's answer in the footer.
    fn send(&mut self, paths: &Paths, req: &Request) {
        self.message = Some(match client::try_request(paths, req) {
            Ok(Some(Response::Ok { msg })) => msg,
            Ok(Some(Response::Err { msg, .. })) => format!("error: {msg}"),
            Ok(Some(_)) | Ok(None) => "onedrive-rcd is not running".to_string(),
            Err(e) => format!("error: {e:#}"),
        });
        self.refresh(paths);
    }
}

/// Puts the terminal back the way it was. Raw mode clears the terminal's own
/// `ISIG` flag, so a Ctrl-C while the dashboard is up never reaches us as a
/// signal at all — it arrives as an ordinary key event, handled below like
/// any other key. The one way the terminal can still get left broken is a
/// panic partway through a draw, which is what the hook installed in [`run`]
/// is for.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
}

pub fn run(paths: &Paths) -> Result<()> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, paths);

    restore_terminal();
    terminal.show_cursor()?;
    let _ = std::panic::take_hook();
    result
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, paths: &Paths) -> Result<()> {
    let mut state = State::new();

    loop {
        if state.last_refresh.elapsed() >= REFRESH_INTERVAL {
            state.refresh(paths);
        }
        terminal.draw(|f| draw(f, &state))?;

        let timeout = REFRESH_INTERVAL.saturating_sub(state.last_refresh.elapsed());
        if !event::poll(timeout)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        let pins = state
            .status
            .as_ref()
            .map(|s| s.pins.clone())
            .unwrap_or_default();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            // Raw mode clears the terminal's own ISIG flag, so Ctrl-C never
            // arrives as SIGINT while the dashboard is up — it is just
            // another key event, so it must be handled explicitly.
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(());
            }
            KeyCode::Tab => state.switch_pane(state.pane.next()),
            KeyCode::BackTab => state.switch_pane(state.pane.prev()),
            KeyCode::Char('r') => state.refresh(paths),
            KeyCode::Char('c') => state.send(paths, &Request::IndexRefresh { full: true }),
            KeyCode::Char('m') => state.send(paths, &Request::MountUp { remote: None }),
            KeyCode::Char('u') => state.send(paths, &Request::MountDown { remote: None }),
            KeyCode::Char('R') => state.send(paths, &Request::MountRemount { remote: None }),
            KeyCode::Down | KeyCode::Char('j') => {
                state.selected = move_selection(
                    state.selected,
                    selectable_len(state.pane, &pins, &state.conflicts, &state.outbox),
                    1,
                );
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.selected = move_selection(
                    state.selected,
                    selectable_len(state.pane, &pins, &state.conflicts, &state.outbox),
                    -1,
                );
            }
            KeyCode::Enter => state.act(paths),
            _ => {}
        }
    }
}

fn draw(f: &mut Frame, state: &State) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_tabs(f, chunks[0], state.pane);
    draw_body(f, chunks[1], state);
    draw_footer(f, chunks[2], state);
}

fn draw_tabs(f: &mut Frame, area: Rect, pane: Pane) {
    let titles: Vec<Line> = PANES.iter().map(|p| Line::from(p.title())).collect();
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title("onedrive-rc"))
        .select(pane.index())
        .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED));
    f.render_widget(tabs, area);
}

fn draw_body(f: &mut Frame, area: Rect, state: &State) {
    let now = Utc::now();
    match state.pane {
        Pane::Overview => {
            let text = match &state.status {
                Some(report) => render::status(report, now),
                None => "onedrive-rcd is not running and has never recorded any state.\n\
                          Run `onedrive-rc setup` to get started."
                    .to_string(),
            };
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .block(pane_block(Pane::Overview)),
                area,
            );
        }
        Pane::Pins => {
            let pins = state
                .status
                .as_ref()
                .map(|s| s.pins.as_slice())
                .unwrap_or(&[]);
            let items: Vec<ListItem> = if pins.is_empty() {
                vec![ListItem::new("no pins configured")]
            } else {
                pins.iter()
                    .map(|p| ListItem::new(render::pin_line(p, now)))
                    .collect()
            };
            render_list(f, area, Pane::Pins, items, state.selected);
        }
        Pane::Conflicts => {
            let items: Vec<ListItem> = if state.conflicts.is_empty() {
                vec![ListItem::new("no conflicts")]
            } else {
                state
                    .conflicts
                    .iter()
                    .map(|c| ListItem::new(render::conflict_line(c, now)))
                    .collect()
            };
            render_list(f, area, Pane::Conflicts, items, state.selected);
        }
        Pane::Outbox => {
            let items: Vec<ListItem> = if state.outbox.is_empty() {
                vec![ListItem::new("outbox empty")]
            } else {
                state
                    .outbox
                    .iter()
                    .map(|o| ListItem::new(render::outbox_line(o, now)))
                    .collect()
            };
            render_list(f, area, Pane::Outbox, items, state.selected);
        }
        Pane::Log => {
            let text = if state.log.is_empty() {
                "no log lines yet".to_string()
            } else {
                state.log.join("\n")
            };
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .block(pane_block(Pane::Log)),
                area,
            );
        }
    }
}

fn pane_block(pane: Pane) -> Block<'static> {
    Block::default().borders(Borders::ALL).title(pane.title())
}

fn render_list(f: &mut Frame, area: Rect, pane: Pane, items: Vec<ListItem>, selected: usize) {
    let mut list_state = ListState::default();
    if !items.is_empty() {
        list_state.select(Some(selected));
    }
    let list = List::new(items)
        .block(pane_block(pane))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut list_state);
}

fn draw_footer(f: &mut Frame, area: Rect, state: &State) {
    let text = state.message.clone().unwrap_or_else(|| {
        "Tab: pane  ↑/↓: select  Enter: act  c: crawl  m: mount  u: unmount  R: remount  r: refresh  q: quit"
            .to_string()
    });
    f.render_widget(Paragraph::new(text), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict(id: i64) -> ConflictInfo {
        ConflictInfo {
            id,
            subject: "Keepass".into(),
            remote: "od".into(),
            file: "Timm.kdbx".into(),
            folder: "/home/t/onedrive-keepass".into(),
            copies: vec!["Timm.conflict-remote-x.kdbx".into()],
            detected_at: Utc::now(),
        }
    }

    fn outbox_row(id: i64, remote: &str) -> OutboxInfo {
        OutboxInfo {
            id,
            remote: remote.into(),
            path: "notes.txt".into(),
            staged_path: "/home/t/OneDrive.pending/notes.txt".into(),
            queued_at: Utc::now(),
            state: "queued".into(),
            attempts: 0,
            last_err: None,
        }
    }

    #[test]
    fn pane_cycling_wraps_in_both_directions() {
        assert_eq!(Pane::Overview.next(), Pane::Pins);
        assert_eq!(Pane::Log.next(), Pane::Overview, "wraps forward");
        assert_eq!(Pane::Overview.prev(), Pane::Log, "wraps backward");
        assert_eq!(Pane::Pins.prev(), Pane::Overview);
    }

    #[test]
    fn selection_clamps_rather_than_wrapping_or_going_negative() {
        assert_eq!(move_selection(0, 3, -1), 0, "cannot go below the first row");
        assert_eq!(move_selection(2, 3, 1), 2, "cannot go past the last row");
        assert_eq!(move_selection(0, 3, 1), 1);
        assert_eq!(
            move_selection(1, 0, 1),
            0,
            "an empty list has nothing to select"
        );
    }

    #[test]
    fn only_conflicts_and_outbox_panes_are_selectable() {
        let conflicts = vec![conflict(1)];
        let outbox = vec![outbox_row(2, "od")];
        let pins = vec![];
        assert_eq!(
            selectable_len(Pane::Overview, &pins, &conflicts, &outbox),
            0
        );
        assert_eq!(selectable_len(Pane::Log, &pins, &conflicts, &outbox), 0);
        assert_eq!(
            selectable_len(Pane::Conflicts, &pins, &conflicts, &outbox),
            1
        );
        assert_eq!(selectable_len(Pane::Outbox, &pins, &conflicts, &outbox), 1);
    }

    #[test]
    fn enter_on_a_conflict_asks_to_keep_both() {
        let conflicts = vec![conflict(7)];
        match enter_request(Pane::Conflicts, 0, &conflicts, &[]) {
            Some(Request::ConflictResolve {
                id,
                keep: Keep::Both,
            }) => assert_eq!(id, 7),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn enter_on_an_outbox_row_retries_just_its_remote() {
        let outbox = vec![outbox_row(9, "onedrive-timm")];
        match enter_request(Pane::Outbox, 0, &[], &outbox) {
            Some(Request::OutboxRetry { remote }) => {
                assert_eq!(remote.as_deref(), Some("onedrive-timm"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn enter_does_nothing_on_non_actionable_panes_or_an_empty_selection() {
        assert_eq!(enter_request(Pane::Overview, 0, &[], &[]), None);
        assert_eq!(enter_request(Pane::Pins, 0, &[], &[]), None);
        assert_eq!(enter_request(Pane::Log, 0, &[], &[]), None);
        assert_eq!(enter_request(Pane::Conflicts, 0, &[], &[]), None);
        assert_eq!(
            enter_request(Pane::Outbox, 5, &[], &[outbox_row(1, "od")]),
            None
        );
    }
}
