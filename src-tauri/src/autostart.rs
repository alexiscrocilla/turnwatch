//! Starting with Windows: a string value named after the app's identifier in
//! the per-user `Run` key, holding the quoted path of the running exe.
//! Nothing machine-wide; removing the value undoes it.
//!
//! Windows' Startup apps settings (and Task Manager) list it, but turning it
//! off there keeps the `Run` value: they write a flag under
//! `Explorer\StartupApproved\Run` instead, whose first byte is odd while the
//! app is turned off. So the app starts with Windows when the value exists
//! and that flag does not say off; turning it on here clears the flag, and
//! turning it off removes both.
//!
//! The registry is the only record: the tray check mark is read from it at
//! start. The exe runs unpackaged, often straight from `target\release`:
//! when the value points elsewhere (the exe moved), it is pointed back at the
//! running exe at startup. Debug builds never start with Windows: they load
//! their page from the dev server, which does not run at logon.

use std::path::Path;

use tauri::AppHandle;

use crate::registry;

/// Per-user programs started at logon.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Where Windows' Startup apps settings record that the user turned one off.
pub const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// Starting with Windows can be offered: not in debug builds.
pub const AVAILABLE: bool = !cfg!(debug_assertions);

/// The command line Windows runs: the exe's path, quoted (it may hold spaces).
pub fn command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// A StartupApproved flag that says the user turned the app off: its first
/// byte is odd (03 off, 02 on).
pub fn turned_off(flag: &[u8]) -> bool {
    flag.first().is_some_and(|byte| byte & 1 == 1)
}

/// Name of the value: the app's identifier.
fn value_name(app: &AppHandle) -> String {
    app.config().identifier.clone()
}

/// The app starts with Windows: its `Run` value exists, and Windows'
/// Startup apps settings do not have it turned off.
pub fn is_enabled(app: &AppHandle) -> bool {
    let name = value_name(app);
    registry::read_value(RUN_KEY, &name).is_some()
        && !registry::read_binary(APPROVED_KEY, &name).is_some_and(|flag| turned_off(&flag))
}

/// Turns the start with Windows on (for the running exe) or off.
pub fn set(app: &AppHandle, enabled: bool) -> Result<(), String> {
    if !AVAILABLE {
        let lang = monitor_core::i18n::current();
        return Err(lang
            .pick("unavailable in a development build", "indisponible dans une version de développement")
            .to_owned());
    }
    let name = value_name(app);
    let code = |code: u32| {
        monitor_core::i18n::current()
            .pick(format!("Windows error {code}"), format!("erreur Windows {code}"))
    };
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        registry::write_values(RUN_KEY, &[(name.as_str(), command(&exe))]).map_err(code)?;
    } else {
        registry::delete_value(RUN_KEY, &name).map_err(code)?;
    }
    // Turned on here: Windows' "off" no longer applies. Turned off: nothing
    // left behind.
    registry::delete_value(APPROVED_KEY, &name).map_err(code)
}

/// When the app starts with Windows from another path (the exe moved),
/// points the value at the running exe.
pub fn repair(app: &AppHandle) {
    if !AVAILABLE {
        return;
    }
    let name = value_name(app);
    let Some(current) = registry::read_value(RUN_KEY, &name) else {
        return;
    };
    if let Ok(exe) = std::env::current_exe()
        && current != command(&exe)
    {
        let _ = registry::write_values(RUN_KEY, &[(name.as_str(), command(&exe))]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_the_quoted_exe_path() {
        let exe = Path::new(r"D:\Données Utilisateur\claude-session-monitor.exe");
        assert_eq!(
            command(exe),
            r#""D:\Données Utilisateur\claude-session-monitor.exe""#
        );
    }

    #[test]
    fn an_odd_first_byte_means_turned_off_in_windows() {
        assert!(turned_off(&[3, 0, 0, 0, 0xA0, 0x1B, 0, 0, 0, 0, 0, 0]));
        assert!(!turned_off(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(!turned_off(&[6]));
        assert!(!turned_off(&[]));
    }

    /// Writes, reads and removes test values in the real `Run` and
    /// `StartupApproved\Run` keys (they point nowhere and are gone at the end).
    /// `cargo test autostart::tests::run_key_round_trip -- --ignored`
    #[test]
    #[ignore = "writes to this user's registry"]
    fn run_key_round_trip() {
        let name = "local.claude-session-monitor.test";
        let value = command(Path::new(r"C:\Dossier é\absent.exe"));
        registry::delete_value(RUN_KEY, name).unwrap();
        assert_eq!(registry::read_value(RUN_KEY, name), None);
        registry::write_values(RUN_KEY, &[(name, value.clone())]).unwrap();
        assert_eq!(registry::read_value(RUN_KEY, name), Some(value));
        assert_eq!(registry::read_binary(APPROVED_KEY, name), None);
        registry::delete_value(RUN_KEY, name).unwrap();
        assert_eq!(registry::read_value(RUN_KEY, name), None);
        registry::delete_value(RUN_KEY, name).expect("already gone is fine");
        registry::delete_value(APPROVED_KEY, name).expect("missing is fine");
    }
}
