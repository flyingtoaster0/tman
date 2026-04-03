//! tmux-pilot v0.2: TUI tmux session manager with persistent sidebar + embedded
//! terminal.
//!
//! The sidebar stays visible at all times. The main area embeds a real PTY
//! running `tmux attach-session`, rendered via a vt100 parser. Keystrokes are
//! forwarded to the PTY in terminal mode; Ctrl+G toggles sidebar focus for
//! navigation.
//!
//! Usage: `cargo build --release && ./target/release/tmux-pilot`
//!
//! Keybindings:
//!   Ctrl+G          Toggle sidebar/terminal focus
//!   (Sidebar) j/k   Navigate sessions
//!   (Sidebar) Enter  Attach to selected session
//!   (Sidebar) n      Create new session
//!   (Sidebar) x      Kill selected session
//!   (Sidebar) r      Refresh session list
//!   (Sidebar) q      Quit tmux-pilot
//!   (Terminal)       All keys forwarded to tmux session

use std::io::{self, Read, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph},
};

// ═══════════════════════════════════════════════════════════════════════════════
// TMUX COMMANDS
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug)]
struct TmuxSession {
    name: String,
    windows: u32,
    attached: bool,
    created: u64,
}

fn tmux_cmd(args: &[&str]) -> String {
    Command::new("tmux")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn get_sessions() -> Vec<TmuxSession> {
    let fmt = "#{session_name}|#{session_windows}|#{session_attached}|#{session_created}";
    let raw = tmux_cmd(&["list-sessions", "-F", fmt]);
    if raw.is_empty() {
        return vec![];
    }
    raw.lines()
        .filter_map(|line| {
            let p: Vec<&str> = line.split('|').collect();
            if p.len() >= 4 {
                Some(TmuxSession {
                    name: p[0].to_string(),
                    windows: p[1].parse().unwrap_or(0),
                    attached: p[2] != "0",
                    created: p[3].parse().unwrap_or(0),
                })
            } else {
                None
            }
        })
        .collect()
}

fn format_timestamp(ts: u64) -> String {
    if ts == 0 {
        return "?".to_string();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let diff = now.saturating_sub(ts);
    if diff < 60 {
        "now".to_string()
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86400 {
        format!("{}h", diff / 3600)
    } else {
        format!("{}d", diff / 86400)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// EMBEDDED TERMINAL (PTY + vt100 rendering)
// ═══════════════════════════════════════════════════════════════════════════════

struct EmbeddedTerminal {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    alive: Arc<AtomicBool>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    session_name: String,
}

impl EmbeddedTerminal {
    /// Spawn a PTY running `tmux attach-session -t <name>`.
    fn spawn(session_name: &str, rows: u16, cols: u16) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new("sh");
        cmd.args([
            "-c",
            &format!(
                "unset TMUX; exec tmux attach-session -t '{}'",
                session_name.replace('\'', "'\\''")
            ),
        ]);
        cmd.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 1000)));
        let alive = Arc::new(AtomicBool::new(true));

        // Reader thread: feed PTY output into vt100 parser
        let parser_clone = Arc::clone(&parser);
        let alive_clone = Arc::clone(&alive);
        thread::spawn(move || {
            pty_reader_loop(reader, parser_clone, alive_clone);
        });

        Ok(Self {
            parser,
            writer,
            alive,
            master: pair.master,
            child,
            session_name: session_name.to_string(),
        })
    }

    /// Resize the PTY and vt100 parser.
    fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut parser) = self.parser.lock() {
            parser.set_size(rows, cols);
        }
    }

    /// Send raw bytes to the PTY (keyboard input).
    fn write_bytes(&mut self, data: &[u8]) {
        let _ = self.writer.write_all(data);
        let _ = self.writer.flush();
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Render the vt100 screen into a ratatui buffer region.
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let parser = match self.parser.lock() {
            Ok(p) => p,
            Err(_) => return,
        };
        let screen = parser.screen();

        for row in 0..area.height {
            for col in 0..area.width {
                let buf_x = area.x + col;
                let buf_y = area.y + row;
                if buf_x >= area.right() || buf_y >= area.bottom() {
                    continue;
                }

                let Some(ratatui_cell) = buf.cell_mut(Position::new(buf_x, buf_y)) else {
                    continue;
                };

                let Some(vt_cell) = screen.cell(row, col) else {
                    ratatui_cell.set_char(' ');
                    continue;
                };

                let contents = vt_cell.contents();
                if contents.is_empty() {
                    continue; // wide-char continuation
                }

                let ch = contents.chars().next().unwrap_or(' ');
                let fg = convert_vt_color(vt_cell.fgcolor());
                let bg = convert_vt_color(vt_cell.bgcolor());
                let mut style = Style::default().fg(fg).bg(bg);

                if vt_cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if vt_cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if vt_cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if vt_cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                ratatui_cell.set_char(ch);
                ratatui_cell.set_style(style);
            }
        }

        // Show cursor position
        let cursor = screen.cursor_position();
        let cx = area.x + cursor.1;
        let cy = area.y + cursor.0;
        if cx < area.right() && cy < area.bottom() {
            if let Some(cell) = buf.cell_mut(Position::new(cx, cy)) {
                cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

impl Drop for EmbeddedTerminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.try_wait();
    }
}

fn pty_reader_loop(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    alive: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Ok(mut p) = parser.lock() {
                    p.process(&buf[..n]);
                }
            }
            Err(_) => break,
        }
    }
    alive.store(false, Ordering::Relaxed);
}

fn convert_vt_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Convert a crossterm KeyEvent to bytes for a PTY.
fn key_to_pty_bytes(key: &KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Char(c) if ctrl => {
            let byte = (c.to_ascii_lowercase() as u8)
                .wrapping_sub(b'a')
                .wrapping_add(1);
            if alt {
                vec![0x1b, byte]
            } else {
                vec![byte]
            }
        }
        KeyCode::Char(c) => {
            let mut tmp = [0u8; 4];
            let s = c.encode_utf8(&mut tmp);
            if alt {
                let mut v = vec![0x1b];
                v.extend_from_slice(s.as_bytes());
                v
            } else {
                s.as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![127],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => vec![0x1b, b'[', b'Z'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::F(1) => vec![0x1b, b'O', b'P'],
        KeyCode::F(2) => vec![0x1b, b'O', b'Q'],
        KeyCode::F(3) => vec![0x1b, b'O', b'R'],
        KeyCode::F(4) => vec![0x1b, b'O', b'S'],
        KeyCode::F(5) => vec![0x1b, b'[', b'1', b'5', b'~'],
        KeyCode::F(6) => vec![0x1b, b'[', b'1', b'7', b'~'],
        KeyCode::F(7) => vec![0x1b, b'[', b'1', b'8', b'~'],
        KeyCode::F(8) => vec![0x1b, b'[', b'1', b'9', b'~'],
        KeyCode::F(9) => vec![0x1b, b'[', b'2', b'0', b'~'],
        KeyCode::F(10) => vec![0x1b, b'[', b'2', b'1', b'~'],
        KeyCode::F(11) => vec![0x1b, b'[', b'2', b'3', b'~'],
        KeyCode::F(12) => vec![0x1b, b'[', b'2', b'4', b'~'],
        _ => vec![],
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// APP STATE
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(PartialEq)]
enum Focus {
    Terminal,
    Sidebar,
}

enum InputMode {
    Normal,
    NewSession(String),
}

const SIDEBAR_WIDTH: u16 = 24;

struct App {
    sessions: Vec<TmuxSession>,
    list_state: ListState,
    focus: Focus,
    mode: InputMode,
    terminal: Option<EmbeddedTerminal>,
    last_refresh: Instant,
    should_quit: bool,
    term_rows: u16,
    term_cols: u16,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            sessions: vec![],
            list_state: ListState::default(),
            focus: Focus::Sidebar,
            mode: InputMode::Normal,
            terminal: None,
            last_refresh: Instant::now() - Duration::from_secs(10),
            should_quit: false,
            term_rows: 24,
            term_cols: 80,
        };
        app.refresh();
        if !app.sessions.is_empty() {
            app.list_state.select(Some(0));
            app.attach_selected();
        }
        app
    }

    fn refresh(&mut self) {
        let prev = self.selected_session_name();
        self.sessions = get_sessions();
        self.last_refresh = Instant::now();

        if self.sessions.is_empty() {
            self.list_state.select(None);
        } else if let Some(name) = prev {
            let idx = self
                .sessions
                .iter()
                .position(|s| s.name == name)
                .unwrap_or(0);
            self.list_state.select(Some(idx));
        } else if self.list_state.selected().is_none() {
            self.list_state.select(Some(0));
        }
    }

    fn selected_session_name(&self) -> Option<String> {
        self.list_state
            .selected()
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.name.clone())
    }

    fn next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let i = self
            .list_state
            .selected()
            .map_or(0, |i| (i + 1) % self.sessions.len());
        self.list_state.select(Some(i));
    }

    fn previous(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let len = self.sessions.len();
        let i = self
            .list_state
            .selected()
            .map_or(0, |i| if i == 0 { len - 1 } else { i - 1 });
        self.list_state.select(Some(i));
    }

    fn attach_selected(&mut self) {
        let Some(name) = self.selected_session_name() else {
            return;
        };

        // Don't re-attach if already on this session
        if self
            .terminal
            .as_ref()
            .is_some_and(|t| t.session_name == name)
        {
            self.focus = Focus::Terminal;
            return;
        }

        self.terminal = None; // drop old PTY
        if let Ok(t) = EmbeddedTerminal::spawn(&name, self.term_rows, self.term_cols) {
            self.terminal = Some(t);
            self.focus = Focus::Terminal;
        }
    }

    fn create_session(&mut self, name: &str) {
        if name.is_empty() {
            return;
        }
        tmux_cmd(&["new-session", "-d", "-s", name]);
        self.refresh();
        if let Some(idx) = self.sessions.iter().position(|s| s.name == name) {
            self.list_state.select(Some(idx));
            self.attach_selected();
        }
    }

    fn kill_selected(&mut self) {
        let Some(name) = self.selected_session_name() else {
            return;
        };
        if self
            .terminal
            .as_ref()
            .is_some_and(|t| t.session_name == name)
        {
            self.terminal = None;
        }
        tmux_cmd(&["kill-session", "-t", &name]);
        self.refresh();
    }

    fn update_term_size(&mut self, rows: u16, cols: u16) {
        if rows > 0 && cols > 0 && (rows != self.term_rows || cols != self.term_cols) {
            self.term_rows = rows;
            self.term_cols = cols;
            if let Some(t) = &mut self.terminal {
                t.resize(rows, cols);
            }
        }
    }

    fn check_terminal_alive(&mut self) {
        if self.terminal.as_ref().is_some_and(|t| !t.is_alive()) {
            self.terminal = None;
            self.focus = Focus::Sidebar;
            self.refresh();
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// UI
// ═══════════════════════════════════════════════════════════════════════════════

fn ui(frame: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(20)])
        .split(frame.area());

    let sidebar_area = Rect {
        height: outer[0].height.saturating_sub(1),
        ..outer[0]
    };
    let main_area = Rect {
        height: outer[1].height.saturating_sub(1),
        ..outer[1]
    };
    let status_area = Rect {
        x: 0,
        y: frame.area().height.saturating_sub(1),
        width: frame.area().width,
        height: 1,
    };

    render_sidebar(frame, app, sidebar_area);
    render_main(frame, app, main_area);
    render_status(frame, app, status_area);

    if let InputMode::NewSession(ref input) = app.mode {
        render_dialog(frame, input);
    }
}

fn render_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };
    let active_name = app.terminal.as_ref().map(|t| t.session_name.as_str());

    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            let is_active = active_name == Some(s.name.as_str());
            let dot = if s.attached { " ●" } else { "" };
            let name_style = if is_active {
                Style::default().fg(Color::Cyan).bold()
            } else {
                Style::default().fg(Color::White)
            };

            let line = Line::from(vec![
                Span::raw(" "),
                Span::styled(&s.name, name_style),
                Span::styled(dot, Style::default().fg(Color::Green)),
            ]);
            let meta = Line::from(Span::styled(
                format!(" {}w · {}", s.windows, format_timestamp(s.created)),
                Style::default().fg(Color::DarkGray),
            ));
            ListItem::new(vec![line, meta])
        })
        .collect();

    let title = if focused { " ⣿ PILOT " } else { " ⣿ pilot " };
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .title_style(Style::default().fg(Color::Cyan).bold())
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .bold(),
        )
        .highlight_symbol("▸")
        .highlight_spacing(HighlightSpacing::Always);

    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_main(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Terminal;
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };

    if app.terminal.is_some() {
        let session_name = app.terminal.as_ref().unwrap().session_name.clone();
        let block = Block::default()
            .title(format!(" {} ", session_name))
            .title_style(Style::default().fg(Color::Cyan).bold())
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        app.update_term_size(inner.height, inner.width);
        app.terminal.as_ref().unwrap().render(inner, frame.buffer_mut());
    } else {
        let block = Block::default()
            .title(" terminal ")
            .title_style(Style::default().fg(Color::DarkGray))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let msg = if app.sessions.is_empty() {
            "No tmux sessions.\n\nCtrl+G then 'n' to create one."
        } else {
            "Select a session and press Enter."
        };
        let p = Paragraph::new(msg)
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray));

        let offset = inner.height / 3;
        frame.render_widget(
            p,
            Rect {
                y: inner.y + offset,
                height: inner.height.saturating_sub(offset),
                ..inner
            },
        );
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let (label, color) = match app.focus {
        Focus::Terminal => (" TERMINAL ", Color::Cyan),
        Focus::Sidebar => (" SIDEBAR ", Color::Yellow),
    };

    let mut spans: Vec<Span> = vec![
        Span::styled(label, Style::default().bg(color).fg(Color::Black).bold()),
        Span::raw(" "),
    ];

    let keys: &[(&str, &str)] = match app.focus {
        Focus::Terminal => &[("^G", "Sidebar")],
        Focus::Sidebar => &[
            ("Ret", "Attach"),
            ("n", "New"),
            ("x", "Kill"),
            ("r", "Refresh"),
            ("q", "Quit"),
            ("^G", "Terminal"),
        ],
    };

    for (k, d) in keys {
        spans.push(Span::styled(
            format!(" {k} "),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ));
        spans.push(Span::styled(
            format!(" {d} "),
            Style::default().fg(Color::DarkGray),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_dialog(frame: &mut Frame, input: &str) {
    let a = frame.area();
    let w = 44u16.min(a.width.saturating_sub(4));
    let h = 5u16;
    let dialog = Rect::new((a.width - w) / 2, (a.height - h) / 2, w, h);

    frame.render_widget(Clear, dialog);

    let block = Block::default()
        .title(" New Session ")
        .title_style(Style::default().fg(Color::Cyan).bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(format!(" Name: {}▌", input)).style(Style::default().fg(Color::White)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(" Enter: create · Esc: cancel").style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// MAIN
// ═══════════════════════════════════════════════════════════════════════════════

fn main() -> Result<()> {
    if tmux_cmd(&["-V"]).is_empty() {
        eprintln!("Error: tmux not found in PATH");
        std::process::exit(1);
    }

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App::new();

    loop {
        app.check_terminal_alive();
        if app.last_refresh.elapsed() > Duration::from_secs(3) {
            app.refresh();
        }

        terminal.draw(|f| ui(f, &mut app))?;
        if app.should_quit {
            break;
        }

        // 16ms poll = ~60fps rendering for smooth PTY output
        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            handle_key(&mut app, key);
        }
    }

    drop(app.terminal);
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // Dialog input takes priority
    if let InputMode::NewSession(ref mut input) = app.mode {
        match key.code {
            KeyCode::Enter => {
                let name = input.clone();
                app.mode = InputMode::Normal;
                app.create_session(&name);
            }
            KeyCode::Esc => app.mode = InputMode::Normal,
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) if c.is_alphanumeric() || "-_.".contains(c) => input.push(c),
            _ => {}
        }
        return;
    }

    // Ctrl+G toggles focus (global keybinding)
    if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.focus = match app.focus {
            Focus::Terminal => Focus::Sidebar,
            Focus::Sidebar if app.terminal.is_some() => Focus::Terminal,
            _ => Focus::Sidebar,
        };
        return;
    }

    match app.focus {
        Focus::Terminal => {
            if let Some(t) = &mut app.terminal {
                let bytes = key_to_pty_bytes(&key);
                if !bytes.is_empty() {
                    t.write_bytes(&bytes);
                }
            }
        }
        Focus::Sidebar => match key.code {
            KeyCode::Down | KeyCode::Char('j') => app.next(),
            KeyCode::Up | KeyCode::Char('k') => app.previous(),
            KeyCode::Enter => app.attach_selected(),
            KeyCode::Char('n') => app.mode = InputMode::NewSession(String::new()),
            KeyCode::Char('x') => app.kill_selected(),
            KeyCode::Char('r') => app.refresh(),
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Esc if app.terminal.is_some() => app.focus = Focus::Terminal,
            _ => {}
        },
    }
}
