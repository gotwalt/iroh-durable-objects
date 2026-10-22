//! Terminal UI for `chat`: a header, the message pane, a member sidebar and
//! an input line. It talks to [`crate::client::Client`] through channels only;
//! the network task never touches the terminal.

use std::{
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
};
use n0_error::{Result, StdResultExt};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthChar;

use crate::{
    client::{Client, Command, ConnState, MAX_HISTORY, Options, Origin, Update},
    net::Target,
    room::{Event, HELP, Input, Member},
};

/// Below this width the member sidebar is hidden.
const SIDEBAR_MIN_WIDTH: u16 = 70;
const SIDEBAR_WIDTH: u16 = 22;
/// At or above this width the header shows the full room id.
const FULL_ID_WIDTH: u16 = 100;
const WHEEL_ROWS: usize = 3;

/// How a pane line is styled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Scrollback: dimmed.
    Hist,
    /// Delivered live.
    Live,
    /// Our own request.
    Own,
    /// Local information and `/who`.
    Notice,
    /// A refusal from the room or a local failure.
    Error,
}

/// One logical line in the message pane, before wrapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneLine {
    /// Sequence number of the event, if the line shows one.
    pub seq: Option<u64>,
    /// Rendered text.
    pub text: String,
    /// Style class.
    pub kind: Kind,
}

/// The input line: single-line editing plus the history of sent lines.
#[derive(Debug, Default)]
pub struct Editor {
    buf: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    /// Index into `history` while browsing it with Up/Down.
    browsing: Option<usize>,
    draft: Vec<char>,
}

impl Editor {
    /// Current text.
    pub fn text(&self) -> String {
        self.buf.iter().collect()
    }

    /// Applies one key. Returns the line on Enter.
    pub fn key(&mut self, key: KeyEvent) -> Option<String> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => {
                let line = self.text();
                if !line.trim().is_empty() && self.history.last() != Some(&line) {
                    self.history.push(line.clone());
                }
                self.buf.clear();
                self.cursor = 0;
                self.browsing = None;
                return Some(line);
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.buf.len(),
            KeyCode::Char('u') if ctrl => {
                self.buf.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.buf.insert(self.cursor, c);
                self.cursor += 1;
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.buf.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.buf.len(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.buf.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.buf.len() => {
                self.buf.remove(self.cursor);
            }
            KeyCode::Up if !self.history.is_empty() => {
                let i = match self.browsing {
                    None => {
                        self.draft = self.buf.clone();
                        self.history.len() - 1
                    }
                    Some(i) => i.saturating_sub(1),
                };
                self.browsing = Some(i);
                self.set(self.history[i].chars().collect());
            }
            KeyCode::Down => match self.browsing {
                Some(i) if i + 1 < self.history.len() => {
                    self.browsing = Some(i + 1);
                    self.set(self.history[i + 1].chars().collect());
                }
                Some(_) => {
                    self.browsing = None;
                    let draft = std::mem::take(&mut self.draft);
                    self.set(draft);
                }
                None => {}
            },
            _ => {}
        }
        None
    }

    fn set(&mut self, buf: Vec<char>) {
        self.buf = buf;
        self.cursor = self.buf.len();
    }

    /// Display width of the text before the cursor.
    fn cursor_col(&self) -> usize {
        self.buf[..self.cursor]
            .iter()
            .map(|c| c.width().unwrap_or(0))
            .sum()
    }
}

/// What the event loop does after a key.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing beyond redrawing.
    None,
    /// Exit without leaving the room.
    Exit,
    /// Send to the client.
    Command(CommandKind),
}

/// The subset of [`Command`] the UI issues, comparable in tests.
#[derive(Debug, PartialEq, Eq)]
pub enum CommandKind {
    /// A typed line.
    Send(Input),
    /// Fetch older scrollback.
    Older {
        /// Events loaded.
        have: usize,
        /// Oldest loaded sequence number.
        before: u64,
    },
}

/// UI state.
#[derive(Debug)]
pub struct App {
    room: String,
    nick: String,
    conn: ConnState,
    relay: bool,
    lines: Vec<PaneLine>,
    members: Vec<Member>,
    editor: Editor,
    /// Rows scrolled up from the bottom; 0 follows new lines.
    scroll: usize,
    /// Pane width at the last draw, for counting wrapped rows.
    pane_width: usize,
    /// Pane height at the last draw.
    pane_height: usize,
    older_pending: bool,
    older_done: bool,
    left: bool,
}

impl App {
    /// A new UI for room `room` (z-base-32).
    pub fn new(room: String) -> Self {
        Self {
            room,
            nick: String::new(),
            conn: ConnState::Connecting,
            relay: false,
            lines: Vec::new(),
            members: Vec::new(),
            editor: Editor::default(),
            scroll: 0,
            pane_width: 80,
            pane_height: 20,
            older_pending: false,
            older_done: false,
            left: false,
        }
    }

    /// Whether the member left with `/quit`.
    pub fn left(&self) -> bool {
        self.left
    }

    /// The message pane's logical lines.
    #[cfg(test)]
    pub fn lines(&self) -> &[PaneLine] {
        &self.lines
    }

    fn push(&mut self, line: PaneLine) {
        if self.scroll > 0 {
            // Keep the view where the user scrolled it.
            self.scroll += wrap(&line.text, self.pane_width).len();
        }
        self.lines.push(line);
    }

    fn notice(&mut self, text: impl Into<String>, kind: Kind) {
        self.push(PaneLine {
            seq: None,
            text: text.into(),
            kind,
        });
    }

    /// Applies one update from the client.
    pub fn update(&mut self, u: Update) {
        let tz = chrono::Local;
        let today = chrono::Local::now().date_naive();
        match u {
            Update::Event { ev, origin, .. } => {
                let kind = match (&ev, origin) {
                    (Event::Err { .. }, _) => Kind::Error,
                    (Event::Who { .. }, _) => Kind::Notice,
                    (_, Origin::Hist) => Kind::Hist,
                    (_, Origin::Live) => Kind::Live,
                    (_, Origin::Own) => Kind::Own,
                };
                if let Some(text) = ev.render(&tz, today) {
                    self.push(PaneLine {
                        seq: ev.seq(),
                        text,
                        kind,
                    });
                }
            }
            Update::Older(events) => {
                self.older_pending = false;
                let loaded = self.lines.iter().filter(|l| l.seq.is_some()).count();
                if events.is_empty() || loaded + events.len() >= MAX_HISTORY {
                    self.older_done = true;
                }
                let older: Vec<PaneLine> = events
                    .iter()
                    .filter_map(|ev| {
                        ev.render(&tz, today).map(|text| PaneLine {
                            seq: ev.seq(),
                            text,
                            kind: Kind::Hist,
                        })
                    })
                    .collect();
                if older.is_empty() {
                    self.lines.insert(
                        0,
                        PaneLine {
                            seq: None,
                            text: "-- start of loaded history".into(),
                            kind: Kind::Notice,
                        },
                    );
                }
                self.lines.splice(0..0, older);
            }
            Update::Members(m) => self.members = m,
            Update::Joined { nick } => {
                self.nick = nick.clone();
                self.notice(
                    format!("-- joined as {nick}; /help lists commands"),
                    Kind::Notice,
                );
            }
            Update::Nick(n) => self.nick = n,
            Update::Conn(c) => self.conn = c,
            Update::Relay(r) => self.relay = r,
            Update::Error(e) => self.notice(format!("!! {e}"), Kind::Error),
            Update::Left => self.left = true,
        }
    }

    fn total_rows(&self) -> usize {
        self.lines
            .iter()
            .map(|l| wrap(&l.text, self.pane_width).len())
            .sum()
    }

    fn max_scroll(&self) -> usize {
        self.total_rows().saturating_sub(self.pane_height)
    }

    /// Scrolls up by `rows`; asks for older history at the top.
    fn scroll_up(&mut self, rows: usize) -> Action {
        let max = self.max_scroll();
        let at_top = self.scroll >= max;
        self.scroll = (self.scroll + rows).min(max);
        if at_top && !self.older_pending && !self.older_done {
            let seqs = self.lines.iter().filter_map(|l| l.seq);
            let have = seqs.clone().count();
            match seqs.min() {
                Some(before) => {
                    self.older_pending = true;
                    return Action::Command(CommandKind::Older { have, before });
                }
                None => self.older_done = true,
            }
        }
        Action::None
    }

    /// Applies one key.
    pub fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Action::Exit,
            KeyCode::Char('c' | 'd') if ctrl => return Action::Exit,
            KeyCode::PageUp => return self.scroll_up(self.pane_height.max(2) - 1),
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(self.pane_height.max(2) - 1);
                return Action::None;
            }
            _ => {}
        }
        let Some(line) = self.editor.key(key) else {
            return Action::None;
        };
        match Input::parse(&line) {
            Ok(Input::Empty) => Action::None,
            Ok(Input::Help) => {
                for l in HELP.lines() {
                    self.notice(l, Kind::Notice);
                }
                Action::None
            }
            Ok(input) => {
                self.scroll = 0;
                Action::Command(CommandKind::Send(input))
            }
            Err(msg) => {
                self.notice(format!("!! {msg}"), Kind::Error);
                Action::None
            }
        }
    }

    /// Applies a mouse wheel event.
    fn wheel(&mut self, up: bool) -> Action {
        if up {
            self.scroll_up(WHEEL_ROWS)
        } else {
            self.scroll = self.scroll.saturating_sub(WHEEL_ROWS);
            Action::None
        }
    }

    fn header(&self, width: u16) -> Line<'static> {
        let id = if width >= FULL_ID_WIDTH {
            self.room.clone()
        } else {
            format!("{}…", &self.room[..self.room.len().min(10)])
        };
        let conn = match self.conn {
            ConnState::Connecting => "connecting",
            ConnState::Joined => "joined",
            ConnState::Reconnecting => "reconnecting",
        };
        let relay = if self.relay {
            Span::raw("room reachable").green()
        } else {
            Span::raw("room unreachable").red()
        };
        Line::from(vec![
            Span::raw(format!(" room {id}  ")).bold(),
            Span::raw(format!("nick {}  ", self.nick)),
            Span::raw(format!("{conn}  ")),
            relay,
        ])
        .reversed()
    }

    /// Draws the whole screen.
    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let [head, body, input] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);
        f.render_widget(
            Paragraph::new(self.header(area.width)).style(Style::new().reversed()),
            head,
        );
        let (pane, side) = if area.width >= SIDEBAR_MIN_WIDTH {
            let [p, s] =
                Layout::horizontal([Constraint::Min(1), Constraint::Length(SIDEBAR_WIDTH)])
                    .areas(body);
            (p, Some(s))
        } else {
            (body, None)
        };
        self.draw_pane(f, pane);
        if let Some(side) = side {
            self.draw_members(f, side);
        }
        self.draw_input(f, input);
    }

    fn draw_pane(&mut self, f: &mut Frame, area: Rect) {
        self.pane_width = usize::from(area.width).max(1);
        self.pane_height = usize::from(area.height);
        let mut rows: Vec<Line> = Vec::new();
        for l in &self.lines {
            let style = match l.kind {
                Kind::Hist => Style::new().dim(),
                Kind::Live => Style::new(),
                Kind::Own => Style::new().cyan(),
                Kind::Notice => Style::new().yellow(),
                Kind::Error => Style::new().red(),
            };
            rows.extend(
                wrap(&l.text, self.pane_width)
                    .into_iter()
                    .map(|r| Line::styled(r, style)),
            );
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(self.pane_height));
        let end = rows.len() - self.scroll;
        let start = end.saturating_sub(self.pane_height);
        let visible: Vec<Line> = rows.drain(start..end).collect();
        // Bottom-align, as a terminal does.
        let pad = self.pane_height.saturating_sub(visible.len());
        let mut text = vec![Line::raw(""); pad];
        text.extend(visible);
        f.render_widget(Paragraph::new(text), area);
    }

    fn draw_members(&self, f: &mut Frame, area: Rect) {
        let mut members: Vec<&Member> = self.members.iter().collect();
        members.sort_by_key(|m| (!m.present, m.nick.to_lowercase()));
        let lines: Vec<Line> = members
            .into_iter()
            .map(|m| {
                let style = if m.present {
                    Style::new()
                } else {
                    Style::new().add_modifier(Modifier::DIM)
                };
                Line::styled(format!(" {}", m.nick), style)
            })
            .collect();
        let block = Block::new()
            .borders(Borders::LEFT)
            .title(format!(" members ({})", self.members.len()));
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let prompt = "> ";
        let width = usize::from(area.width)
            .saturating_sub(prompt.len() + 1)
            .max(1);
        let col = self.editor.cursor_col();
        // Scroll the input horizontally to keep the cursor visible.
        let skip = col.saturating_sub(width);
        let mut shown = String::new();
        let mut w = 0;
        for c in self.editor.buf.iter() {
            let cw = c.width().unwrap_or(0);
            if w >= skip {
                shown.push(*c);
            }
            w += cw;
        }
        f.render_widget(Paragraph::new(format!("{prompt}{shown}")), area);
        let x = area.x + (prompt.len() + col - skip) as u16;
        f.set_cursor_position(Position::new(x.min(area.right().saturating_sub(1)), area.y));
    }
}

/// Splits `text` into rows at most `width` columns wide.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut w = 0;
    for c in text.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width {
            rows.push(String::new());
            w = 0;
        }
        rows.last_mut().expect("non-empty").push(c);
        w += cw;
    }
    rows
}

/// Restores the terminal, also on panic.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        ratatui::restore();
    }
}

/// Runs the interactive client until the user exits or leaves.
pub async fn run(target: &Target, opts: Options) -> Result<ExitCode> {
    let (up_tx, mut updates) = mpsc::unbounded_channel();
    let (cmd_tx, commands) = mpsc::unbounded_channel();
    let net_target = target.clone();
    let net = tokio::spawn(async move {
        match Client::start(&net_target, &opts, up_tx.clone()).await {
            Ok(mut client) => {
                client.run(commands).await;
                client.endpoint().close().await;
            }
            Err(err) => {
                let _ = up_tx.send(Update::Error(format!("{err:#}; Esc exits")));
            }
        }
    });

    let mut terminal = ratatui::try_init().std_context("initializing the terminal")?;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        ratatui::restore();
        prev(info);
    }));
    let _restore = Restore;
    execute!(std::io::stdout(), EnableMouseCapture).std_context("enabling mouse capture")?;

    // crossterm's `read` blocks, so terminal events come from a thread.
    let stop = Arc::new(AtomicBool::new(false));
    let (term_tx, mut term_events) = mpsc::unbounded_channel();
    let reader = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if term_tx.send(ev).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        })
    };

    let mut app = App::new(target.z32());
    loop {
        terminal
            .draw(|f| app.draw(f))
            .std_context("drawing the terminal")?;
        let action = tokio::select! {
            u = updates.recv() => match u {
                Some(u) => {
                    app.update(u);
                    Action::None
                }
                // The network task ended; keep the screen until the user exits.
                None => match term_events.recv().await {
                    Some(ev) => term_action(&mut app, ev),
                    None => Action::Exit,
                },
            },
            ev = term_events.recv() => match ev {
                Some(ev) => term_action(&mut app, ev),
                None => Action::Exit,
            },
        };
        match action {
            Action::None => {}
            Action::Exit => break,
            Action::Command(c) => {
                let cmd = match c {
                    CommandKind::Send(i) => Command::Send(i),
                    CommandKind::Older { have, before } => Command::Older { have, before },
                };
                if cmd_tx.send(cmd).is_err() {
                    app.update(Update::Error("not connected to the room".into()));
                }
            }
        }
        if app.left() {
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    drop(cmd_tx);
    let _ = reader.join();
    drop(_restore);
    // Closing the endpoint closes the relay socket, which ends presence.
    if tokio::time::timeout(Duration::from_secs(3), net)
        .await
        .is_err()
    {
        eprintln!("warning: the endpoint did not close within 3 s");
    }
    Ok(ExitCode::SUCCESS)
}

fn term_action(app: &mut App, ev: TermEvent) -> Action {
    match ev {
        TermEvent::Key(k) => app.key(k),
        TermEvent::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => app.wheel(true),
            MouseEventKind::ScrollDown => app.wheel(false),
            _ => Action::None,
        },
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(app: &mut App, s: &str) -> Action {
        for c in s.chars() {
            app.key(press(KeyCode::Char(c)));
        }
        app.key(press(KeyCode::Enter))
    }

    #[test]
    fn editor_edits_and_recalls() {
        let mut e = Editor::default();
        for c in "helo".chars() {
            e.key(press(KeyCode::Char(c)));
        }
        e.key(press(KeyCode::Left));
        e.key(press(KeyCode::Char('l')));
        e.key(press(KeyCode::Home));
        e.key(press(KeyCode::Delete));
        e.key(press(KeyCode::Char('H')));
        e.key(press(KeyCode::End));
        e.key(press(KeyCode::Backspace));
        assert_eq!(e.text(), "Hell");
        assert_eq!(e.key(press(KeyCode::Enter)).as_deref(), Some("Hell"));
        e.key(press(KeyCode::Char('x')));
        e.key(press(KeyCode::Up));
        assert_eq!(e.text(), "Hell");
        e.key(press(KeyCode::Down));
        assert_eq!(e.text(), "x");
    }

    #[test]
    fn keys_map_to_actions() {
        let mut app = App::new("room".into());
        assert_eq!(
            typed(&mut app, "hi"),
            Action::Command(CommandKind::Send(Input::Say("hi".into())))
        );
        assert_eq!(
            typed(&mut app, "/quit bye"),
            Action::Command(CommandKind::Send(Input::Quit(Some("bye".into()))))
        );
        assert_eq!(typed(&mut app, "/help"), Action::None);
        assert!(app.lines().iter().any(|l| l.text.contains("/history")));
        assert_eq!(typed(&mut app, "/bogus"), Action::None);
        assert_eq!(app.lines().last().unwrap().kind, Kind::Error);
        assert_eq!(app.key(press(KeyCode::Esc)), Action::Exit);
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Exit
        );
    }

    #[test]
    fn scrolling_past_the_top_asks_for_older_history() {
        let mut app = App::new("room".into());
        app.pane_height = 2;
        for seq in 10..15 {
            app.update(Update::Event {
                ev: Event::Msg {
                    seq,
                    prev: seq - 1,
                    ts: 0,
                    peer: "p".into(),
                    nick: "ana".into(),
                    text: "x".into(),
                },
                raw: String::new(),
                origin: Origin::Hist,
            });
        }
        assert_eq!(app.scroll_up(10), Action::None);
        assert_eq!(
            app.scroll_up(1),
            Action::Command(CommandKind::Older {
                have: 5,
                before: 10
            })
        );
        assert_eq!(app.scroll_up(1), Action::None);
        app.update(Update::Older(Vec::new()));
        assert_eq!(app.scroll_up(1), Action::None);
        assert_eq!(app.lines()[0].kind, Kind::Notice);
    }

    #[test]
    fn wraps_by_display_width() {
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(wrap("", 4), vec![""]);
        assert_eq!(wrap("日本語", 4), vec!["日本", "語"]);
    }
}
