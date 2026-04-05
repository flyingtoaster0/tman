use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::HostConfig;

#[derive(Clone, Debug)]
pub struct TmuxSession {
    pub name: String,
    pub windows: u32,
    pub attached: bool,
    pub created: u64,
    pub host: String, // "local" or host config name
}

pub struct RemoteResult {
    pub host_name: String,
    pub result: std::result::Result<Vec<TmuxSession>, String>,
}

pub fn tmux_cmd(args: &[&str]) -> String {
    Command::new("tmux")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn build_ssh_command(host: &HostConfig) -> Command {
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
    cmd
}

pub fn ssh_tmux_cmd(host: &HostConfig, args: &[&str]) -> String {
    let mut cmd = build_ssh_command(host);
    let tmux_args: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();
    cmd.arg(format!("tmux {}", tmux_args.join(" ")));

    cmd.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || "-_./:#{}|".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn get_local_sessions() -> Vec<TmuxSession> {
    let fmt = "#{session_name}|#{session_windows}|#{session_attached}|#{session_created}";
    let raw = tmux_cmd(&["list-sessions", "-F", fmt]);
    parse_session_lines(&raw, "local")
}

pub fn get_remote_sessions(host: &HostConfig) -> std::result::Result<Vec<TmuxSession>, String> {
    let fmt = "#{session_name}|#{session_windows}|#{session_attached}|#{session_created}";
    let mut cmd = build_ssh_command(host);
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

pub fn format_timestamp(ts: u64) -> String {
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
