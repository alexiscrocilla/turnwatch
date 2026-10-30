//! Registers the app's AppUserModelID (AUMID) for the current user, so that
//! Windows labels its toasts with the app's name and icon. The installer only
//! sets the AUMID on its shortcuts, and the exe may also run straight from
//! `target\release`: without this, the toasts would be shown under Windows
//! PowerShell's.
//!
//! This is Microsoft's documented registration for unpackaged apps. At
//! startup, and only when the value differs from what is there:
//! - registry key `HKEY_CURRENT_USER\Software\Classes\AppUserModelId\<identifier>`
//!   gets the string values `DisplayName` (the product name) and `IconUri`
//!   (the path of the icon file below);
//! - the file `<local app data>\<identifier>\toast-icon.png` gets the app icon.
//!
//! Nothing machine-wide, no shortcut, no COM server. Deleting the key undoes
//! it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

use crate::registry;

/// File name of the toast icon, in the app's local data folder.
pub const ICON_FILE: &str = "toast-icon.png";
/// The app icon shown on toasts.
static ICON_PNG: &[u8] = include_bytes!("../icons/128x128.png");
/// Windows rejects longer AUMIDs.
const APP_ID_MAX_LEN: usize = 129;

/// The AUMID this app's toasts are shown with, once registered.
#[derive(Debug, Default)]
pub struct ToastIdentity(pub Option<String>);

/// Registry path of the AUMID, under `HKEY_CURRENT_USER`.
pub fn key_path(app_id: &str) -> String {
    format!(r"Software\Classes\AppUserModelId\{app_id}")
}

/// True for an id usable as an AUMID: 1 to 129 characters, no spaces or
/// path separators.
pub fn is_valid_app_id(id: &str) -> bool {
    (1..=APP_ID_MAX_LEN).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The registry values to write: the display name, and the icon when there
/// is one.
pub fn values(display_name: &str, icon: Option<&Path>) -> Vec<(&'static str, String)> {
    let mut values = vec![("DisplayName", display_name.to_owned())];
    if let Some(icon) = icon {
        values.push(("IconUri", icon.display().to_string()));
    }
    values
}

/// Writes the toast icon into `dir` unless it is already there, and returns
/// its path.
pub fn write_icon(dir: &Path) -> io::Result<PathBuf> {
    let path = dir.join(ICON_FILE);
    if fs::read(&path).ok().as_deref() != Some(ICON_PNG) {
        fs::create_dir_all(dir)?;
        fs::write(&path, ICON_PNG)?;
    }
    Ok(path)
}

/// Registers the app's AUMID (its Tauri identifier) and returns it, or None
/// when it cannot be registered: the toasts then fall back to the
/// notification plugin.
pub fn register(app: &AppHandle) -> Option<String> {
    let config = app.config();
    let id = config.identifier.clone();
    if !is_valid_app_id(&id) {
        return None;
    }
    let name = config
        .product_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| id.clone());
    // The name alone is enough for Windows; a missing icon only looks plainer.
    let icon = app
        .path()
        .app_local_data_dir()
        .ok()
        .and_then(|dir| write_icon(&dir).ok());
    registry::write_values(&key_path(&id), &values(&name, icon.as_deref()))
        .ok()
        .map(|()| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_path_is_per_user_under_classes() {
        assert_eq!(
            key_path("local.turnwatch"),
            r"Software\Classes\AppUserModelId\local.turnwatch"
        );
    }

    #[test]
    fn app_ids_are_checked() {
        assert!(is_valid_app_id("local.turnwatch"));
        assert!(is_valid_app_id("local.turnwatch.dev"));
        assert!(!is_valid_app_id(""));
        assert!(!is_valid_app_id("with space"));
        assert!(!is_valid_app_id(r"a\b"));
        assert!(!is_valid_app_id(&"a".repeat(APP_ID_MAX_LEN + 1)));
        assert!(is_valid_app_id(&"a".repeat(APP_ID_MAX_LEN)));
    }

    #[test]
    fn values_name_the_app_and_its_icon() {
        let icon = Path::new(r"C:\Users\É\AppData\Local\local.turnwatch\toast-icon.png");
        assert_eq!(
            values("Turnwatch", Some(icon)),
            vec![
                ("DisplayName", "Turnwatch".to_owned()),
                ("IconUri", icon.display().to_string()),
            ]
        );
        assert_eq!(
            values("Turnwatch", None),
            vec![("DisplayName", "Turnwatch".to_owned())]
        );
    }

    #[test]
    fn icon_is_written_once_and_repaired() {
        let dir = std::env::temp_dir()
            .join("turnwatch-tests")
            .join(format!("icon dossier é-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = write_icon(&dir).unwrap();
        assert_eq!(path, dir.join(ICON_FILE));
        assert_eq!(fs::read(&path).unwrap(), ICON_PNG);
        assert!(ICON_PNG.starts_with(b"\x89PNG"), "a PNG, as IconUri expects");

        fs::write(&path, b"broken").unwrap();
        write_icon(&dir).unwrap();
        assert_eq!(fs::read(&path).unwrap(), ICON_PNG);
        let _ = fs::remove_dir_all(&dir);
    }
}
