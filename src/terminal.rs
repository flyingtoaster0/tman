use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use ratatui::prelude::*;

use crate::config::HostConfig;
use crate::tmux::shell_escape;

pub struct EmbeddedTerminal {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    alive: Arc<AtomicBool>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pub session_name: String,
    pub host: String,
}

impl EmbeddedTerminal {
    /// Spawn a PTY running `tmux attach-session -t <name>` (local or remote).
    pub fn spawn(
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

    pub fn resize(&mut self, rows: u16, cols: u16) {
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

    pub fn write_bytes(&mut self, data: &[u8]) {
        let _ = self.writer.write_all(data);
        let _ = self.writer.flush();
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
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

pub fn key_to_pty_bytes(key: &KeyEvent) -> Vec<u8> {
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
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
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
