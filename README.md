# tman

A server-side TUI tmux session manager with a **persistent sidebar and embedded terminal**, inspired by cmux's vertical tab layout. SSH in from any device and get a visual session picker alongside your live tmux session.

## How it works

The sidebar is **always visible**. The main area is a real embedded terminal running `tmux attach`, rendered via a vt100 parser and PTY.

1. SSH into your server from any device (tablet, phone, laptop)
2. tman launches, shows sessions in the sidebar, auto-attaches to the first
3. **Ctrl+G** toggles focus between sidebar and terminal
4. In sidebar: j/k to navigate, Enter to switch sessions, n/x to create/kill
5. In terminal: all keystrokes go to tmux — it's a fully functional terminal
6. If you detach inside tmux (Ctrl+B d), the PTY exits and you return to the sidebar

## Architecture

```
┌───────────┐     ┌──────────────────────────────────────┐
│  Sidebar  │     │  Embedded Terminal                    │
│  (ratatui │     │  ┌─────────────────────────────────┐  │
│   widgets)│     │  │ PTY ──► vt100::Parser            │  │
│           │     │  │  └─ tmux attach -t <session>     │  │
│           │     │  │                                   │  │
│  Focus:   │     │  │ Keystrokes ◄── crossterm events  │  │
│  Ctrl+G   │     │  └─────────────────────────────────┘  │
└───────────┘     └──────────────────────────────────────┘
```

- **portable-pty** spawns `tmux attach -t <session>` in a pseudo-terminal
- A background thread reads PTY output into a **vt100::Parser**
- The main thread renders the parser's screen state into the ratatui buffer at ~60fps
- Keyboard input is converted to PTY bytes and written to the PTY's stdin
- The sidebar runs standard ratatui widgets alongside the terminal

## Installation

### Homebrew

```bash
brew tap flyingtoaster0/tman
brew install tman
```

### Cargo

```bash
cargo install tman
```

### From source

```bash
cargo build --release
./target/release/tman
```

Single static binary — scp it anywhere.

## Keybindings

| Context  | Key            | Action                          |
|----------|----------------|---------------------------------|
| Global   | Ctrl+G         | Toggle sidebar/terminal focus   |
| Sidebar  | j/k, ↑/↓       | Navigate sessions               |
| Sidebar  | Enter          | Attach to selected session      |
| Sidebar  | n              | Create new session              |
| Sidebar  | x              | Kill selected session           |
| Sidebar  | r              | Refresh session list            |
| Sidebar  | a              | Add SSH host                    |
| Sidebar  | d              | Remove SSH host                 |
| Sidebar  | q              | Quit tman                       |
| Sidebar  | Esc            | Return to terminal              |
| Terminal | (all keys)     | Forwarded to tmux               |

## Making it your SSH landing page

```bash
# Option 1: ~/.bashrc (server-side)
if [[ -n "$SSH_CONNECTION" && -z "$TMUX" ]]; then
    exec ~/bin/tman
fi

# Option 2: ~/.ssh/config (client-side)
Host devbox
    HostName your-server
    RemoteCommand ~/bin/tman
    RequestTTY yes
```

## Configuration

Remote hosts are configured in `~/.config/tman/config.toml`. You can also add/remove hosts interactively with `a` and `d` in the sidebar.

## Dependencies

| Crate        | Purpose                              |
|--------------|--------------------------------------|
| ratatui      | TUI framework                        |
| crossterm    | Terminal backend + event handling     |
| vt100        | VT100/xterm escape sequence parser   |
| portable-pty | Cross-platform pseudo-terminal       |
| anyhow       | Error handling                       |

## Notes

- **TMUX nesting**: The PTY unsets `$TMUX` before running `tmux attach`, so it works even if you launched tman from inside tmux.
- **Resize handling**: Terminal resize events propagate to the PTY and vt100 parser automatically.
- **Session list**: Auto-refreshes every 3 seconds without interrupting the terminal.
- **Detach detection**: If the `tmux attach` process exits (e.g., `Ctrl+B d`), the app detects it and returns to the sidebar.
