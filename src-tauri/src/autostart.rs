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
//!
//! On macOS the same is a launch agent: `~/Library/LaunchAgents/<identifier>.plist`,
//! which runs the app's executable at login. Nothing machine-wide; removing
//! the file undoes it, and it is pointed back at the running app at startup
//! too. macOS lists it in Login Items (Allow in the Background), where
//! switching it off keeps the file: the check mark then still says on.

use std::path::Path;

use tauri::AppHandle;

#[cfg(not(target_os = "macos"))]
use crate::registry;

/// Per-user programs started at logon.
#[cfg(not(target_os = "macos"))]
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Where Windows' Startup apps settings record that the user turned one off.
#[cfg(not(target_os = "macos"))]
pub const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// Starting with Windows can be offered: not in debug builds.
pub const AVAILABLE: bool = !cfg!(debug_assertions);

/// The tray item, the title of its error toast and what it turns on, in
/// English and in French.
#[cfg(not(target_os = "macos"))]
pub const ITEM: (&str, &str) = ("Start with Windows", "Lancer au démarrage de Windows");
#[cfg(not(target_os = "macos"))]
pub const TITLE: (&str, &str) = ("Start with Windows", "Démarrage avec Windows");
#[cfg(not(target_os = "macos"))]
pub const WHAT: (&str, &str) = ("starting with Windows", "le lancement au démarrage");
#[cfg(target_os = "macos")]
pub const ITEM: (&str, &str) = ("Open at login", "Ouvrir à la connexion");
#[cfg(target_os = "macos")]
pub const TITLE: (&str, &str) = ("Open at login", "Ouverture à la connexion");
#[cfg(target_os = "macos")]
pub const WHAT: (&str, &str) = ("opening at login", "l'ouverture à la connexion");

/// The command line Windows runs: the exe's path, quoted (it may hold spaces).
#[cfg(not(target_os = "macos"))]
pub fn command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// A StartupApproved flag that says the user turned the app off: its first
/// byte is odd (03 off, 02 on).
#[cfg(not(target_os = "macos"))]
pub fn turned_off(flag: &[u8]) -> bool {
    flag.first().is_some_and(|byte| byte & 1 == 1)
}

/// Name of the value: the app's identifier.
fn value_name(app: &AppHandle) -> String {
    app.config().identifier.clone()
}

/// The app starts with Windows: its `Run` value exists, and Windows'
/// Startup apps settings do not have it turned off.
#[cfg(not(target_os = "macos"))]
pub fn is_enabled(app: &AppHandle) -> bool {
    let name = value_name(app);
    registry::read_value(RUN_KEY, &name).is_some()
        && !registry::read_binary(APPROVED_KEY, &name).is_some_and(|flag| turned_off(&flag))
}

/// Turns the start with Windows on (for the running exe) or off.
#[cfg(not(target_os = "macos"))]
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
#[cfg(not(target_os = "macos"))]
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

/// The app opens at login: its launch agent exists.
#[cfg(target_os = "macos")]
pub fn is_enabled(app: &AppHandle) -> bool {
    agent_file(app).is_some_and(|file| file.is_file())
}

/// Turns the opening at login on (for the running app) or off.
#[cfg(target_os = "macos")]
pub fn set(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let lang = monitor_core::i18n::current();
    if !AVAILABLE {
        return Err(lang
            .pick("unavailable in a development build", "indisponible dans une version de développement")
            .to_owned());
    }
    let file = agent_file(app).ok_or_else(|| lang.pick("no home folder", "pas de dossier personnel").to_owned())?;
    if !enabled {
        return match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if translocated(&exe) {
        return Err(lang
            .pick(
                "move the app to the Applications folder first",
                "déplace d'abord l'application dans le dossier Applications",
            )
            .to_owned());
    }
    write_agent(&file, &value_name(app), &exe).map_err(|e| e.to_string())
}

/// When the launch agent runs the app from another path (the app moved),
/// points it at the running app.
#[cfg(target_os = "macos")]
pub fn repair(app: &AppHandle) {
    if !AVAILABLE {
        return;
    }
    let Some(file) = agent_file(app) else {
        return;
    };
    let (Ok(current), Ok(exe)) = (std::fs::read_to_string(&file), std::env::current_exe()) else {
        return;
    };
    let wanted = agent_plist(&value_name(app), &exe);
    if current != wanted && !translocated(&exe) {
        let _ = std::fs::write(&file, wanted);
    }
}

/// `~/Library/LaunchAgents/<identifier>.plist`.
#[cfg(target_os = "macos")]
fn agent_file(app: &AppHandle) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    let name = format!("{}.plist", value_name(app));
    Some(Path::new(&home).join("Library").join("LaunchAgents").join(name))
}

#[cfg(target_os = "macos")]
fn write_agent(file: &Path, label: &str, exe: &Path) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, agent_plist(label, exe))
}

/// A launch agent that runs `exe` once at login, in the user's graphical
/// session only.
#[cfg(target_os = "macos")]
pub fn agent_plist(label: &str, exe: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#,
        xml_text(label),
        xml_text(&exe.to_string_lossy()),
    )
}

/// Escapes text for a plist string.
#[cfg(target_os = "macos")]
fn xml_text(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// An app run from where it was downloaded, still in quarantine: macOS runs
/// it from a random read-only copy, gone once it quits.
#[cfg(target_os = "macos")]
pub fn translocated(exe: &Path) -> bool {
    exe.components().any(|part| part.as_os_str() == "AppTranslocation")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn the_launch_agent_runs_the_exe_at_login() {
        let exe = Path::new("/Applications/Turn & <watch>.app/Contents/MacOS/turnwatch");
        let plist = agent_plist("local.turnwatch", exe);
        assert!(plist.contains("<string>local.turnwatch</string>"));
        assert!(plist.contains(
            "<string>/Applications/Turn &amp; &lt;watch&gt;.app/Contents/MacOS/turnwatch</string>"
        ));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_translocated_app_is_recognized() {
        assert!(translocated(Path::new(
            "/private/var/folders/xy/abc/T/AppTranslocation/0A1B/d/Turnwatch.app/Contents/MacOS/turnwatch"
        )));
        assert!(!translocated(Path::new("/Applications/Turnwatch.app/Contents/MacOS/turnwatch")));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_command_is_the_quoted_exe_path() {
        let exe = Path::new(r"D:\Données Utilisateur\turnwatch.exe");
        assert_eq!(
            command(exe),
            r#""D:\Données Utilisateur\turnwatch.exe""#
        );
    }

    #[cfg(not(target_os = "macos"))]
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
    #[cfg(not(target_os = "macos"))]
    #[test]
    #[ignore = "writes to this user's registry"]
    fn run_key_round_trip() {
        let name = "local.turnwatch.test";
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
