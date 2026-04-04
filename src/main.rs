//! tmux-pilot v0.3: TUI tmux session manager with persistent sidebar, embedded
//! terminal, and SSH remote session management.
//!
//! The sidebar stays visible at all times. The main area embeds a real PTY
//! running `tmux attach-session`, rendered via a vt100 parser. Keystrokes are
//! forwarded to the PTY in terminal mode; Ctrl+G toggles sidebar focus for
//! navigation.
//!
//! Remote hosts are configured in `~/.config/tmux-pilot/config.toml` and their
//! tmux sessions appear alongside local ones, grouped by host.
//!
//! Usage: `cargo build --release && ./target/release/tmux-pilot`
//!
//! Keybindings:
//!   Ctrl+G          Toggle sidebar/terminal focus
//!   (Sidebar) j/k   Navigate sessions
//!   (Sidebar) Enter  Attach to selected session
//!   (Sidebar) n      Create new session (on selected host)
//!   (Sidebar) x      Kill selected session
//!   (Sidebar) r      Refresh session list
//!   (Sidebar) a      Add SSH host
//!   (Sidebar) d      Remove SSH host (when header selected)
//!   (Sidebar) q      Quit tmux-pilot
//!   (Terminal)       All keys forwarded to tmux session

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
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
use serde::{Deserialize, Serialize};

// ═══════════════════════════════════════════════════════════════════════════════
// CONFIG
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HostConfig {
    name: String,
    address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity_file: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Config {
    #[serde(default)]
    hosts: Vec<HostConfig>,
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("tmux-pilot")
        .join("config.toml")
}

fn load_config() -> Config {
    let path = config_path();
    match fs::read_to_string(&path) {
        Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

fn save_config(config: &Config) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(s) = toml::to_string_pretty(config) {
        let _ = fs::write(&path, s);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TMUX COMMANDS
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug)]
struct TmuxSession {
    name: String,
    windows: u32,
    attached: bool,
    created: u64,
    host: String, // "local" or host config name
}

fn tmux_cmd(args: &[&str]) -> String {
    Command::new("tmux")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn ssh_tmux_cmd(host: &HostConfig, args: &[&str]) -> String {
    let mut cmd = Command::new("ssh");
    cmd.arg("-o")
        .arg("ConnectTimeout=5")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new");
    if let Some(port) = host.port {
        cmd.arg("-p").arg(port.to_string());
    }
    if let Some(ref key) = host.identity_file {
        cmd.arg("-i").arg(key);
    }
    cmd.arg(&host.address);

    // Build the remote tmux command
    let tmux_args: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();
    cmd.arg(format!("tmux {}", tmux_args.join(" ")));

    cmd.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || "-_./:#{}|".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn get_local_sessions() -> Vec<TmuxSession> {
    let fmt = "#{session_name}|#{session_windows}|#{session_attached}|#{session_created}";
    let raw = tmux_cmd(&["list-sessions", "-F", fmt]);
    parse_session_lines(&raw, "local")
}

fn get_remote_sessions(host: &HostConfig) -> std::result::Result<Vec<TmuxSession>, String> {
    let fmt = "#{session_name}|#{session_windows}|#{session_attached}|#{session_created}";
    let mut cmd = Command::new("ssh");
    cmd.arg("-o")
        .arg("ConnectTimeout=5")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new");
    if let Some(port) = host.port {
        cmd.arg("-p").arg(port.to_string());
    }
    if let Some(ref key) = host.identity_file {
        cmd.arg("-i").arg(key);
    }
    cmd.arg(&host.address);
    cmd.arg(format!("tmux list-sessions -F '{}'", fmt));

    match cmd.output() {
        Ok(output) => {
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
                Ok(parse_session_lines(&raw, &host.name))
            } else {
                let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if err.contains("no server running") || err.contains("no sessions") {
                    Ok(vec![])
                } else {
                    Err(err)
                }
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

fn parse_session_lines(raw: &str, host: &str) -> Vec<TmuxSession> {
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
                    host: host.to_string(),
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
    host: String,
}

impl EmbeddedTerminal {
    /// Spawn a PTY running `tmux attach-session -t <name>` (local or remote).
    fn spawn(
        session_name: &str,
        host: &str,
        host_config: Option<&HostConfig>,
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let shell_cmd = if host == "local" {
            format!(
                "unset TMUX; exec tmux attach-session -t '{}'",
                session_name.replace('\'', "'\\''")
            )
        } else if let Some(hc) = host_config {
            let mut ssh = String::from("exec ssh -t");
            if let Some(port) = hc.port {
                ssh.push_str(&format!(" -p {}", port));
            }
            if let Some(ref key) = hc.identity_file {
                ssh.push_str(&format!(" -i {}", shell_escape(key)));
            }
            ssh.push_str(&format!(
                " {} \"tmux attach-session -t '{}'\"",
                shell_escape(&hc.address),
                session_name.replace('\'', "'\\''")
            ));
            ssh
        } else {
            anyhow::bail!("no host config for remote host {}", host);
        };

        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", &shell_cmd]);
        cmd.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 1000)));
        let alive = Arc::new(AtomicBool::new(true));

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
            host: host.to_string(),
        })
    }

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

    fn write_bytes(&mut self, data: &[u8]) {
        let _ = self.writer.write_all(data);
        let _ = self.writer.flush();
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

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
                    continue;
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
// SIDEBAR ENTRIES
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug)]
enum SidebarEntry {
    HostHeader {
        name: String,
        status: HostStatus,
    },
    Session(TmuxSession),
}

#[derive(Clone, Debug, PartialEq)]
enum HostStatus {
    Ok,
    Fetching,
    Error(String),
}

impl SidebarEntry {
    fn is_session(&self) -> bool {
        matches!(self, SidebarEntry::Session(_))
    }

    fn host_name(&self) -> &str {
        match self {
            SidebarEntry::HostHeader { name, .. } => name,
            SidebarEntry::Session(s) => &s.host,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// REMOTE FETCH
// ═══════════════════════════════════════════════════════════════════════════════

struct RemoteResult {
    host_name: String,
    result: std::result::Result<Vec<TmuxSession>, String>,
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
    AddHost(String),
    ConfirmDeleteHost(String),
}

const SIDEBAR_WIDTH: u16 = 28;

struct App {
    config: Config,
    entries: Vec<SidebarEntry>,
    list_state: ListState,
    focus: Focus,
    mode: InputMode,
    terminal: Option<EmbeddedTerminal>,
    last_refresh: Instant,
    should_quit: bool,
    term_rows: u16,
    term_cols: u16,
    // Remote fetch state
    remote_tx: mpsc::Sender<RemoteResult>,
    remote_rx: mpsc::Receiver<RemoteResult>,
    remote_status: HashMap<String, HostStatus>,
    remote_sessions: HashMap<String, Vec<TmuxSession>>,
    remote_last_fetch: HashMap<String, Instant>,
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
        let prev_session = self.selected_session().map(|s| (s.host.clone(), s.name.clone()));
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
            if let Some(idx) = self.entries.iter().position(|e| {
                matches!(e, SidebarEntry::Session(s) if s.host == host && s.name == name)
            }) {
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
            let _ = tx.send(RemoteResult {
                host_name,
                result,
            });
        });
    }

    fn drain_remote_results(&mut self) {
        let mut changed = false;
        while let Ok(result) = self.remote_rx.try_recv() {
            self.remote_last_fetch
                .insert(result.host_name.clone(), Instant::now());
            match result.result {
                Ok(sessions) => {
                    self.remote_status
                        .insert(result.host_name.clone(), HostStatus::Ok);
                    self.remote_sessions
                        .insert(result.host_name, sessions);
                }
                Err(err) => {
                    self.remote_status
                        .insert(result.host_name.clone(), HostStatus::Error(err));
                    self.remote_sessions
                        .insert(result.host_name, vec![]);
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
        if self.terminal.as_ref().is_some_and(|t| {
            t.session_name == session.name && t.host == session.host
        }) {
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
        if let Some(idx) = self.entries.iter().position(|e| {
            matches!(e, SidebarEntry::Session(s) if s.name == name && s.host == host)
        }) {
            self.list_state.select(Some(idx));
            self.attach_selected();
        }
    }

    fn kill_selected(&mut self) {
        let session = match self.selected_session() {
            Some(s) => s.clone(),
            None => return,
        };

        if self.terminal.as_ref().is_some_and(|t| {
            t.session_name == session.name && t.host == session.host
        }) {
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
        if self
            .terminal
            .as_ref()
            .is_some_and(|t| t.host == name)
        {
            self.terminal = None;
        }
        self.config.hosts.retain(|h| h.name != name);
        save_config(&self.config);
        self.remote_status.remove(name);
        self.remote_sessions.remove(name);
        self.remote_last_fetch.remove(name);
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

    match &app.mode {
        InputMode::NewSession(ref input) => render_new_session_dialog(frame, input),
        InputMode::AddHost(ref input) => render_add_host_dialog(frame, input),
        InputMode::ConfirmDeleteHost(ref name) => render_confirm_delete_dialog(frame, name),
        InputMode::Normal => {}
    }
}

fn render_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };
    let active_session = app
        .terminal
        .as_ref()
        .map(|t| (t.host.as_str(), t.session_name.as_str()));

    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|entry| match entry {
            SidebarEntry::HostHeader { name, status } => {
                let status_icon = match status {
                    HostStatus::Ok => {
                        Span::styled(" ◆", Style::default().fg(Color::Green))
                    }
                    HostStatus::Fetching => {
                        Span::styled(" ◇", Style::default().fg(Color::Yellow))
                    }
                    HostStatus::Error(_) => {
                        Span::styled(" ✗", Style::default().fg(Color::Red))
                    }
                };
                let label = format!("── {} ", name);
                let line = Line::from(vec![
                    Span::styled(label, Style::default().fg(Color::DarkGray).bold()),
                    status_icon,
                ]);
                ListItem::new(vec![line])
            }
            SidebarEntry::Session(s) => {
                let is_active =
                    active_session == Some((s.host.as_str(), s.name.as_str()));
                let dot = if s.attached { " ●" } else { "" };
                let name_style = if is_active {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::White)
                };

                let line = Line::from(vec![
                    Span::raw("  "),
                    Span::styled(&s.name, name_style),
                    Span::styled(dot, Style::default().fg(Color::Green)),
                ]);
                let meta = Line::from(Span::styled(
                    format!("  {}w · {}", s.windows, format_timestamp(s.created)),
                    Style::default().fg(Color::DarkGray),
                ));
                ListItem::new(vec![line, meta])
            }
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
        let t = app.terminal.as_ref().unwrap();
        let title = if t.host == "local" {
            format!(" {} ", t.session_name)
        } else {
            format!(" {} [{}] ", t.session_name, t.host)
        };
        let block = Block::default()
            .title(title)
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

        let has_sessions = app.entries.iter().any(|e| e.is_session());
        let msg = if !has_sessions {
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
            ("a", "Host+"),
            ("d", "Host-"),
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

fn render_new_session_dialog(frame: &mut Frame, input: &str) {
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

fn render_add_host_dialog(frame: &mut Frame, input: &str) {
    let a = frame.area();
    let w = 52u16.min(a.width.saturating_sub(4));
    let h = 7u16;
    let dialog = Rect::new((a.width - w) / 2, (a.height - h) / 2, w, h);

    frame.render_widget(Clear, dialog);

    let block = Block::default()
        .title(" Add SSH Host ")
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
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(format!(" SSH: {}▌", input)).style(Style::default().fg(Color::White)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(" Format: user@host or name=user@host:port")
            .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(" Enter: add · Esc: cancel").style(Style::default().fg(Color::DarkGray)),
        rows[4],
    );
}

fn render_confirm_delete_dialog(frame: &mut Frame, name: &str) {
    let a = frame.area();
    let w = 44u16.min(a.width.saturating_sub(4));
    let h = 5u16;
    let dialog = Rect::new((a.width - w) / 2, (a.height - h) / 2, w, h);

    frame.render_widget(Clear, dialog);

    let block = Block::default()
        .title(" Remove Host ")
        .title_style(Style::default().fg(Color::Red).bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));
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
        Paragraph::new(format!(" Remove \"{}\"?", name)).style(Style::default().fg(Color::White)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(" y: confirm · Esc: cancel").style(Style::default().fg(Color::DarkGray)),
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

        // Drain remote results on every tick for responsiveness
        app.drain_remote_results();

        if app.last_refresh.elapsed() > Duration::from_secs(3) {
            app.refresh();
        }

        terminal.draw(|f| ui(f, &mut app))?;
        if app.should_quit {
            break;
        }

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
