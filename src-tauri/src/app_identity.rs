//! Registers the app's AppUserModelID (AUMID) for the current user, so that
//! Windows labels its toasts with the app's name and icon. The exe runs
//! unpackaged (often straight from `target\release`), so no installer or
//! shortcut does it, and without an AUMID the toasts would be shown under
//! Windows PowerShell's.
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
    win::write_values(&key_path(&id), &values(&name, icon.as_deref()))
        .ok()
        .map(|()| id)
}

#[cfg(windows)]
mod win {
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
        RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegGetValueW, RegSetValueExW,
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// Creates `HKEY_CURRENT_USER\<subkey>` if needed and sets each string
    /// value that differs. Returns the first Win32 error.
    pub fn write_values(subkey: &str, values: &[(&str, String)]) -> Result<(), u32> {
        let subkey = wide(subkey);
        let mut key: HKEY = null_mut();
        // SAFETY: every pointer is a live null-terminated buffer or an out
        // parameter for the duration of the call; the key is closed below.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                null(),
                &mut key,
                null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(status);
        }
        let mut result = Ok(());
        for (name, value) in values {
            if read_string(key, name).as_deref() == Some(value.as_str()) {
                continue;
            }
            let name = wide(name);
            let data = wide(value);
            let Ok(bytes) = u32::try_from(data.len() * 2) else {
                continue;
            };
            // SAFETY: `data` holds `bytes` bytes, null terminator included.
            let status = unsafe {
                RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), bytes)
            };
            if status != ERROR_SUCCESS && result.is_ok() {
                result = Err(status);
            }
        }
        // SAFETY: `key` was opened above and is closed once.
        unsafe { RegCloseKey(key) };
        result
    }

    /// A string value of an open key, if present.
    fn read_string(key: HKEY, name: &str) -> Option<String> {
        let name = wide(name);
        let mut size = 0u32;
        // SAFETY: size query; no data buffer is passed.
        let status = unsafe {
            RegGetValueW(key, null(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), &mut size)
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2).max(1)];
        let mut size = u32::try_from(buffer.len() * 2).ok()?;
        // SAFETY: `buffer` holds `size` bytes.
        let status = unsafe {
            RegGetValueW(
                key,
                null(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        buffer.truncate((size as usize / 2).min(buffer.len()));
        while buffer.last() == Some(&0) {
            buffer.pop();
        }
        String::from_utf16(&buffer).ok()
    }
}

#[cfg(not(windows))]
mod win {
    pub fn write_values(_subkey: &str, _values: &[(&str, String)]) -> Result<(), u32> {
        Err(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_path_is_per_user_under_classes() {
        assert_eq!(
            key_path("local.claude-session-monitor"),
            r"Software\Classes\AppUserModelId\local.claude-session-monitor"
        );
    }

    #[test]
    fn app_ids_are_checked() {
        assert!(is_valid_app_id("local.claude-session-monitor"));
        assert!(is_valid_app_id("local.claude-session-monitor.dev"));
        assert!(!is_valid_app_id(""));
        assert!(!is_valid_app_id("with space"));
        assert!(!is_valid_app_id(r"a\b"));
        assert!(!is_valid_app_id(&"a".repeat(APP_ID_MAX_LEN + 1)));
        assert!(is_valid_app_id(&"a".repeat(APP_ID_MAX_LEN)));
    }

    #[test]
    fn values_name_the_app_and_its_icon() {
        let icon = Path::new(r"C:\Users\É\AppData\Local\local.claude-session-monitor\toast-icon.png");
        assert_eq!(
            values("Claude Session Monitor", Some(icon)),
            vec![
                ("DisplayName", "Claude Session Monitor".to_owned()),
                ("IconUri", icon.display().to_string()),
            ]
        );
        assert_eq!(
            values("Claude Session Monitor", None),
            vec![("DisplayName", "Claude Session Monitor".to_owned())]
        );
    }

    #[test]
    fn icon_is_written_once_and_repaired() {
        let dir = std::env::temp_dir()
            .join("claude-session-monitor-tests")
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
