//! tman: TUI tmux session manager with persistent sidebar, embedded
//! terminal, and SSH remote session management.
//!
//! The sidebar stays visible at all times. The main area embeds a real PTY
//! running `tmux attach-session`, rendered via a vt100 parser. Keystrokes are
//! forwarded to the PTY in terminal mode; Ctrl+G toggles sidebar focus for
//! navigation.
//!
//! Remote hosts are configured in `~/.config/tman/config.toml` and their
//! tmux sessions appear alongside local ones, grouped by host.
//!
//! Usage: `cargo build --release && ./target/release/tman`
//!
//! Keybindings:
//!   Ctrl+G          Toggle sidebar/terminal focus
//!   (Sidebar) j/k   Navigate sessions
//!   (Sidebar) Enter  Attach to selected session (embedded)
//!   (Sidebar) o      Exit tman and attach directly to selected session
//!   (Sidebar) n      Create new session (on selected host)
//!   (Sidebar) x      Kill selected session
//!   (Sidebar) r      Refresh session list
//!   (Sidebar) a      Add SSH host
//!   (Sidebar) d      Remove SSH host (when header selected)
//!   (Sidebar) q      Quit tman
//!   (Terminal)       All keys forwarded to tmux session

mod config;
mod terminal;
mod tmux;
mod ui;

use std::collections::HashMap;
use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::prelude::*;
use ratatui::widgets::ListState;

use config::{load_config, save_config, Config, HostConfig};
use terminal::{key_to_pty_bytes, paste_to_pty_bytes, EmbeddedTerminal};
use tmux::{
    build_ssh_command, get_local_sessions, get_remote_sessions, ssh_tmux_cmd, tmux_cmd,
    RemoteResult, TmuxSession,
};

struct TerminalUiGuard;

impl TerminalUiGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        io::stdout().execute(EnableBracketedPaste)?;
        io::stdout().execute(EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for TerminalUiGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(DisableBracketedPaste);
        let _ = io::stdout().execute(DisableMouseCapture);
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SIDEBAR ENTRIES
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug)]
pub enum SidebarEntry {
    HostHeader { name: String, status: HostStatus },
    Session(TmuxSession),
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostStatus {
    Ok,
    Fetching,
    Error(String),
}

impl SidebarEntry {
    pub fn is_session(&self) -> bool {
        matches!(self, SidebarEntry::Session(_))
    }

    pub fn host_name(&self) -> &str {
        match self {
            SidebarEntry::HostHeader { name, .. } => name,
            SidebarEntry::Session(s) => &s.host,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// APP STATE
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(PartialEq)]
pub enum Focus {
    Terminal,
    Sidebar,
}

pub enum InputMode {
    Normal,
    NewSession(String),
    AddHost(String),
    ConfirmDeleteHost(String),
}

pub const SIDEBAR_WIDTH: u16 = 28;

pub struct App {
    pub config: Config,
    pub entries: Vec<SidebarEntry>,
    pub list_state: ListState,
    pub focus: Focus,
    pub mode: InputMode,
    pub terminal: Option<EmbeddedTerminal>,
    pub last_refresh: Instant,
    pub should_quit: bool,
    pub quit_and_attach: Option<TmuxSession>,
    pub term_rows: u16,
    pub term_cols: u16,
    // Remote fetch state
    remote_tx: mpsc::Sender<RemoteResult>,
    remote_rx: mpsc::Receiver<RemoteResult>,
    pub remote_status: HashMap<String, HostStatus>,
    pub remote_sessions: HashMap<String, Vec<TmuxSession>>,
    pub remote_last_fetch: HashMap<String, Instant>,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let config = load_config();

        let mut app = Self {
            config,
            entries: vec![],
            list_state: ListState::default(),
            focus: Focus::Sidebar,
            mode: InputMode::Normal,
            terminal: None,
            last_refresh: Instant::now() - Duration::from_secs(10),
            should_quit: false,
            quit_and_attach: None,
            term_rows: 24,
            term_cols: 80,
            remote_tx: tx,
            remote_rx: rx,
            remote_status: HashMap::new(),
            remote_sessions: HashMap::new(),
            remote_last_fetch: HashMap::new(),
        };
        app.refresh();
        if app.entries.iter().any(|e| e.is_session()) {
            // Select first session entry
            app.select_first_session();
            app.attach_selected();
        }
        app
    }

    fn select_first_session(&mut self) {
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.is_session() {
                self.list_state.select(Some(i));
                return;
            }
        }
        self.list_state.select(None);
    }

    fn rebuild_entries(&mut self) {
        let prev_session = self
            .selected_session()
            .map(|s| (s.host.clone(), s.name.clone()));
        self.entries.clear();

        // Local sessions
        let local_sessions = get_local_sessions();
        self.entries.push(SidebarEntry::HostHeader {
            name: "local".to_string(),
            status: HostStatus::Ok,
        });
        for s in local_sessions {
            self.entries.push(SidebarEntry::Session(s));
        }

        // Remote hosts
        for host in &self.config.hosts {
            let status = self
                .remote_status
                .get(&host.name)
                .cloned()
                .unwrap_or(HostStatus::Fetching);
            self.entries.push(SidebarEntry::HostHeader {
                name: host.name.clone(),
                status,
            });
            if let Some(sessions) = self.remote_sessions.get(&host.name) {
                for s in sessions {
                    self.entries.push(SidebarEntry::Session(s.clone()));
                }
            }
        }

        // Restore selection
        if let Some((host, name)) = prev_session {
            if let Some(idx) = self.entries.iter().position(
                |e| matches!(e, SidebarEntry::Session(s) if s.host == host && s.name == name),
            ) {
                self.list_state.select(Some(idx));
                return;
            }
        }

        // If previous selection lost, select first session or first header
        if self.list_state.selected().is_none()
            || self
                .list_state
                .selected()
                .is_some_and(|i| i >= self.entries.len())
        {
            self.select_first_session();
        }
    }

    fn refresh(&mut self) {
        // Drain any pending remote results
        self.drain_remote_results();

        // Rebuild with fresh local data
        self.rebuild_entries();
        self.last_refresh = Instant::now();

        // Trigger remote fetches for hosts that haven't been fetched recently
        for host in &self.config.hosts {
            let should_fetch = self
                .remote_last_fetch
                .get(&host.name)
                .is_none_or(|t| t.elapsed() > Duration::from_secs(5));
            if should_fetch {
                self.remote_status
                    .insert(host.name.clone(), HostStatus::Fetching);
                self.spawn_remote_fetch(host.clone());
            }
        }
    }

    fn spawn_remote_fetch(&self, host: HostConfig) {
        let tx = self.remote_tx.clone();
        let host_name = host.name.clone();
        thread::spawn(move || {
            let result = get_remote_sessions(&host);
            let _ = tx.send(RemoteResult { host_name, result });
        });
    }

    pub fn drain_remote_results(&mut self) {
        let mut changed = false;
        while let Ok(result) = self.remote_rx.try_recv() {
            self.remote_last_fetch
                .insert(result.host_name.clone(), Instant::now());
            match result.result {
                Ok(sessions) => {
                    self.remote_status
                        .insert(result.host_name.clone(), HostStatus::Ok);
                    self.remote_sessions.insert(result.host_name, sessions);
                }
                Err(err) => {
                    self.remote_status
                        .insert(result.host_name.clone(), HostStatus::Error(err));
                    self.remote_sessions.insert(result.host_name, vec![]);
                }
            }
            changed = true;
        }
        if changed {
            self.rebuild_entries();
        }
    }

    fn selected_session(&self) -> Option<&TmuxSession> {
        let idx = self.list_state.selected()?;
        match self.entries.get(idx)? {
            SidebarEntry::Session(s) => Some(s),
            _ => None,
        }
    }

    fn selected_host_header(&self) -> Option<&str> {
        let idx = self.list_state.selected()?;
        match self.entries.get(idx)? {
            SidebarEntry::HostHeader { name, .. } => Some(name),
            _ => None,
        }
    }

    /// Get the host context for the current selection (session's host, or header's host).
    fn selected_host_context(&self) -> Option<String> {
        let idx = self.list_state.selected()?;
        Some(self.entries.get(idx)?.host_name().to_string())
    }

    fn host_config(&self, name: &str) -> Option<&HostConfig> {
        self.config.hosts.iter().find(|h| h.name == name)
    }

    fn next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len();
        let start = self.list_state.selected().map_or(0, |i| (i + 1) % len);
        // Find next entry (allow landing on headers too for host management)
        self.list_state.select(Some(start));
    }

    fn previous(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len();
        let start = self
            .list_state
            .selected()
            .map_or(0, |i| if i == 0 { len - 1 } else { i - 1 });
        self.list_state.select(Some(start));
    }

    fn attach_selected(&mut self) {
        let session = match self.selected_session() {
            Some(s) => s.clone(),
            None => return,
        };

        // Don't re-attach if already on this session
        if self
            .terminal
            .as_ref()
            .is_some_and(|t| t.session_name == session.name && t.host == session.host)
        {
            self.focus = Focus::Terminal;
            return;
        }

        self.terminal = None; // drop old PTY
        let hc = self.host_config(&session.host).cloned();
        if let Ok(t) = EmbeddedTerminal::spawn(
            &session.name,
            &session.host,
            hc.as_ref(),
            self.term_rows,
            self.term_cols,
        ) {
            self.terminal = Some(t);
            self.focus = Focus::Terminal;
        }
    }

    fn create_session(&mut self, name: &str, host: &str) {
        if name.is_empty() {
            return;
        }
        if host == "local" {
            tmux_cmd(&["new-session", "-d", "-s", name]);
        } else if let Some(hc) = self.host_config(host).cloned() {
            ssh_tmux_cmd(&hc, &["new-session", "-d", "-s", name]);
        }
        self.refresh();
        // Select and attach the new session
        if let Some(idx) = self
            .entries
            .iter()
            .position(|e| matches!(e, SidebarEntry::Session(s) if s.name == name && s.host == host))
        {
            self.list_state.select(Some(idx));
            self.attach_selected();
        }
    }

    fn kill_selected(&mut self) {
        let session = match self.selected_session() {
            Some(s) => s.clone(),
            None => return,
        };

        if self
            .terminal
            .as_ref()
            .is_some_and(|t| t.session_name == session.name && t.host == session.host)
        {
            self.terminal = None;
        }

        if session.host == "local" {
            tmux_cmd(&["kill-session", "-t", &session.name]);
        } else if let Some(hc) = self.host_config(&session.host).cloned() {
            ssh_tmux_cmd(&hc, &["kill-session", "-t", &session.name]);
        }
        self.refresh();
    }

    fn add_host(&mut self, input: &str) {
        let input = input.trim();
        if input.is_empty() {
            return;
        }

        // Parse: [name=]address[:port]
        let (name, rest) = if let Some(idx) = input.find('=') {
            (input[..idx].to_string(), &input[idx + 1..])
        } else {
            // Derive name from address
            let addr = input;
            let display = addr
                .split('@')
                .next_back()
                .unwrap_or(addr)
                .split(':')
                .next()
                .unwrap_or(addr);
            (display.to_string(), input)
        };

        let (address, port) = if let Some(idx) = rest.rfind(':') {
            if let Ok(p) = rest[idx + 1..].parse::<u16>() {
                (rest[..idx].to_string(), Some(p))
            } else {
                (rest.to_string(), None)
            }
        } else {
            (rest.to_string(), None)
        };

        // Don't add duplicates
        if self.config.hosts.iter().any(|h| h.name == name) {
            return;
        }

        self.config.hosts.push(HostConfig {
            name: name.clone(),
            address,
            port,
            identity_file: None,
        });
        save_config(&self.config);
        // Clear stale state and refetch
        self.remote_status.remove(&name);
        self.remote_sessions.remove(&name);
        self.remote_last_fetch.remove(&name);
        self.refresh();
    }

    fn remove_host(&mut self, name: &str) {
        if name == "local" {
            return;
        }
        // Drop terminal if attached to a session on this host
        if self.terminal.as_ref().is_some_and(|t| t.host == name) {
            self.terminal = None;
        }
        self.config.hosts.retain(|h| h.name != name);
        save_config(&self.config);
        self.remote_status.remove(name);
        self.remote_sessions.remove(name);
        self.remote_last_fetch.remove(name);
        self.refresh();
    }

    pub fn update_term_size(&mut self, rows: u16, cols: u16) {
        if rows > 0 && cols > 0 && (rows != self.term_rows || cols != self.term_cols) {
            self.term_rows = rows;
            self.term_cols = cols;
            if let Some(t) = &mut self.terminal {
                t.resize(rows, cols);
            }
        }
    }

    fn attach_selected_and_quit(&mut self) {
        if let Some(session) = self.selected_session() {
            self.quit_and_attach = Some(session.clone());
            self.should_quit = true;
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
// MAIN
// ═══════════════════════════════════════════════════════════════════════════════

fn main() -> Result<()> {
    if tmux_cmd(&["-V"]).is_empty() {
        eprintln!("Error: tmux not found in PATH");
        std::process::exit(1);
    }

    let _ui_guard = TerminalUiGuard::enter()?;

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App::new();

    loop {
        app.check_terminal_alive();

        // Drain remote results on every tick for responsiveness
        app.drain_remote_results();

        if app.last_refresh.elapsed() > Duration::from_secs(3) {
            app.refresh();
        }

        terminal.draw(|f| ui::ui(f, &mut app))?;
        if app.should_quit {
            break;
        }

        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => handle_key(&mut app, key),
            Event::Paste(text) => handle_paste(&mut app, &text),
            Event::Mouse(mouse) => handle_mouse(&mut app, mouse),
            _ => {}
        }
    }

    let quit_and_attach = app.quit_and_attach.take();
    let quit_host_config = quit_and_attach
        .as_ref()
        .and_then(|s| app.host_config(&s.host).cloned());
    drop(app.terminal);
    drop(terminal);

    if let Some(session) = quit_and_attach {
        use std::os::unix::process::CommandExt;
        let err = if session.host == "local" {
            std::process::Command::new("tmux")
                .args(["attach-session", "-t", &session.name])
                .exec()
        } else if let Some(hc) = quit_host_config {
            let mut cmd = build_ssh_command(&hc);
            cmd.arg("-t");
            cmd.arg(format!(
                "tmux attach-session -t {}",
                tmux::shell_escape(&session.name)
            ));
            cmd.exec()
        } else {
            return Err(anyhow::anyhow!("unknown host: {}", session.host));
        };
        return Err(anyhow::anyhow!("exec failed: {}", err));
    }

    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // Dialog input takes priority
    match &mut app.mode {
        InputMode::NewSession(ref mut input) => {
            match key.code {
                KeyCode::Enter => {
                    let name = input.clone();
                    let host = app.selected_host_context().unwrap_or("local".to_string());
                    app.mode = InputMode::Normal;
                    app.create_session(&name, &host);
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
        InputMode::AddHost(ref mut input) => {
            match key.code {
                KeyCode::Enter => {
                    let val = input.clone();
                    app.mode = InputMode::Normal;
                    app.add_host(&val);
                }
                KeyCode::Esc => app.mode = InputMode::Normal,
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            }
            return;
        }
        InputMode::ConfirmDeleteHost(ref name) => {
            let name = name.clone();
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    app.mode = InputMode::Normal;
                    app.remove_host(&name);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    app.mode = InputMode::Normal;
                }
                _ => {}
            }
            return;
        }
        InputMode::Normal => {}
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
            KeyCode::Char('o') => app.attach_selected_and_quit(),
            KeyCode::Char('n') => app.mode = InputMode::NewSession(String::new()),
            KeyCode::Char('x') => app.kill_selected(),
            KeyCode::Char('r') => {
                // Force re-fetch all remotes
                app.remote_last_fetch.clear();
                app.refresh();
            }
            KeyCode::Char('a') => app.mode = InputMode::AddHost(String::new()),
            KeyCode::Char('d') => {
                if let Some(name) = app.selected_host_header() {
                    if name != "local" {
                        app.mode = InputMode::ConfirmDeleteHost(name.to_string());
                    }
                }
            }
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Esc if app.terminal.is_some() => app.focus = Focus::Terminal,
            _ => {}
        },
    }
}

fn handle_paste(app: &mut App, text: &str) {
    match &mut app.mode {
        InputMode::NewSession(input) => {
            input.extend(
                text.chars()
                    .filter(|c| c.is_alphanumeric() || "-_.".contains(*c)),
            );
        }
        InputMode::AddHost(input) => {
            input.extend(text.chars().filter(|c| *c != '\r' && *c != '\n'));
        }
        InputMode::ConfirmDeleteHost(_) => {}
        InputMode::Normal => match app.focus {
            Focus::Terminal => {
                if let Some(t) = &mut app.terminal {
                    let bytes = paste_to_pty_bytes(text);
                    if !bytes.is_empty() {
                        t.write_bytes(&bytes);
                    }
                }
            }
            Focus::Sidebar => {}
        },
    }
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    let scroll_button = match mouse.kind {
        MouseEventKind::ScrollUp => Some(64),
        MouseEventKind::ScrollDown => Some(65),
        _ => None,
    };

    if let Some(button) = scroll_button {
        match app.focus {
            Focus::Terminal => {
                if let Some(t) = &mut app.terminal {
                    let col = mouse.column.saturating_add(1);
                    let row = mouse.row.saturating_add(1);
                    let seq = format!("\x1b[<{};{};{}M", button, col, row);
                    t.write_bytes(seq.as_bytes());
                }
            }
            Focus::Sidebar => {
                if button == 64 {
                    app.previous()
                } else {
                    app.next()
                }
            }
        }
    }
}
