//! Locations of the files the monitor reads.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// `CLAUDE_CONFIG_DIR` if set, else `%USERPROFILE%\.claude`.
pub fn claude_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .unwrap_or_default();
    PathBuf::from(home).join(".claude")
}

/// Registry of live processes: `<claude_home>\sessions`.
pub fn sessions_dir() -> PathBuf {
    claude_home().join("sessions")
}

/// Transcripts: `<claude_home>\projects`.
pub fn projects_dir() -> PathBuf {
    claude_home().join("projects")
}

/// Claude Desktop's data folder: `%APPDATA%\Claude`.
pub fn desktop_data_dir() -> PathBuf {
    let appdata = std::env::var_os("APPDATA").unwrap_or_default();
    PathBuf::from(appdata).join("Claude")
}

/// Desktop's per-session files: `%APPDATA%\Claude\claude-code-sessions`.
pub fn desktop_sessions_root() -> PathBuf {
    desktop_data_dir().join("claude-code-sessions")
}

/// Desktop's preferences, which record the sessions shown in its panes:
/// `%APPDATA%\Claude\claude_desktop_config.json`.
pub fn desktop_config_file() -> PathBuf {
    desktop_data_dir().join("claude_desktop_config.json")
}

/// Plan usage samples Desktop records:
/// `%APPDATA%\Claude\plan-usage-history.json`.
pub fn usage_history_file() -> PathBuf {
    desktop_data_dir().join("plan-usage-history.json")
}

/// What the `monitor-compagnon` mod writes: `<claude_home>\session-monitor`.
pub fn companion_dir() -> PathBuf {
    claude_home().join("session-monitor")
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
