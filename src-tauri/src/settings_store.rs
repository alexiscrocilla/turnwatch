//! Persistence of the user's settings as JSON in the app config folder.
//!
//! The file may be edited by hand while the app runs. Every save re-reads it
//! and writes only the fields the app itself changed over what the file holds
//! now, so the other fields (unknown keys included) keep their edited values.
//! A file that cannot be parsed is kept as `settings.json.bak` before it is
//! replaced.

use std::fs;
use std::io;
use std::path::Path;

use monitor_core::model::Settings;
use serde_json::{Map, Value};

/// File name inside the app config folder.
pub const FILE_NAME: &str = "settings.json";

/// What the settings file holds.
#[derive(Debug)]
enum OnDisk {
    Missing,
    Parsed(Map<String, Value>),
    /// Read, but not a JSON object: kept as is so it can be backed up.
    Unparsable(Vec<u8>),
    /// Exists but could not be read (locked, no access…).
    Unreadable(io::Error),
}

fn read_disk(path: &Path) -> OnDisk {
    match fs::read(path) {
        Ok(bytes) => match decode(&bytes).and_then(|text| serde_json::from_str(&text).ok()) {
            Some(Value::Object(fields)) => OnDisk::Parsed(fields),
            _ => OnDisk::Unparsable(bytes),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => OnDisk::Missing,
        Err(e) => OnDisk::Unreadable(e),
    }
}

/// The text of the file: UTF-8 with or without a BOM, or UTF-16 with a BOM
/// (what Windows PowerShell 5.1 writes with `>` or `Out-File`).
fn decode(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(rest.to_vec()).ok();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, u16::from_be_bytes);
    }
    String::from_utf8(bytes.to_vec()).ok()
}

fn utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| unit([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

/// Reads the settings. A missing or unreadable file gives the defaults; a
/// field that is missing or has the wrong type keeps its default value.
pub fn load(path: &Path) -> Settings {
    match read_disk(path) {
        OnDisk::Parsed(fields) => from_value_lenient(&Value::Object(fields)),
        _ => Settings::default(),
    }
}

/// Saves the change from `old` (the settings in use) to `wanted`: the fields
/// that differ between them are written over what the file holds now, and
/// the file's other fields win over `old`. Returns the settings now in
/// effect, and whether the file could be written. An unreadable file is
/// left alone; an unparsable one is first copied to `settings.json.bak`.
pub fn save_change(path: &Path, old: &Settings, wanted: &Settings) -> (Settings, io::Result<()>) {
    let disk = read_disk(path);
    let (mut fields, base) = match &disk {
        OnDisk::Parsed(fields) => (
            fields.clone(),
            from_value_lenient(&Value::Object(fields.clone())),
        ),
        _ => (Map::new(), old.clone()),
    };
    let effective = merge_change(old, wanted, &base);
    if let Ok(Value::Object(known)) = serde_json::to_value(&effective) {
        fields.extend(known);
    }
    let written = match disk {
        OnDisk::Unreadable(e) => Err(e),
        OnDisk::Unparsable(bytes) => fs::write(path.with_extension("json.bak"), bytes)
            .and_then(|()| write_atomic(path, &Value::Object(fields))),
        OnDisk::Missing | OnDisk::Parsed(_) => write_atomic(path, &Value::Object(fields)),
    };
    (effective, written)
}

/// `base` with the fields that differ between `old` and `wanted` taken from
/// `wanted`.
fn merge_change(old: &Settings, wanted: &Settings, base: &Settings) -> Settings {
    let (Ok(Value::Object(old)), Ok(Value::Object(wanted_fields)), Ok(Value::Object(mut merged))) = (
        serde_json::to_value(old),
        serde_json::to_value(wanted),
        serde_json::to_value(base),
    ) else {
        return wanted.clone();
    };
    for (name, value) in wanted_fields {
        if old.get(&name) != Some(&value) {
            merged.insert(name, value);
        }
    }
    serde_json::from_value(Value::Object(merged)).unwrap_or_else(|_| wanted.clone())
}

/// Writes the JSON atomically (temporary file, then rename), UTF-8 without BOM.
fn write_atomic(path: &Path, value: &Value) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)
}

/// Overlays the fields of `value` that have the expected JSON type onto the
/// defaults.
fn from_value_lenient(value: &Value) -> Settings {
    let Value::Object(fields) = value else {
        return Settings::default();
    };
    let Ok(Value::Object(mut merged)) = serde_json::to_value(Settings::default()) else {
        return Settings::default();
    };
    for (name, field) in fields {
        let Some(default) = merged.get(name) else {
            continue;
        };
        let same_kind = match default {
            Value::Bool(_) => field.is_boolean(),
            Value::Number(_) => field.is_u64(),
            Value::String(_) => field.is_string(),
            _ => false,
        };
        if same_kind {
            merged.insert(name.clone(), field.clone());
        }
    }
    serde_json::from_value(Value::Object(merged)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("turnwatch-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn file_json(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = scratch_dir("missing");
        assert_eq!(load(&dir.join(FILE_NAME)), Settings::default());
    }

    #[test]
    fn corrupt_file_gives_defaults() {
        let dir = scratch_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        fs::write(&path, b"{\"notifyOnWaiting\": fal").unwrap();
        assert_eq!(load(&path), Settings::default());
        fs::write(&path, b"[1, 2]").unwrap();
        assert_eq!(load(&path), Settings::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_fields_keep_defaults_and_good_ones_apply() {
        let value: Value = serde_json::from_str(
            r#"{"notifyOnWaiting": "yes", "notifyOnFinished": true, "prPollSecs": -5,
                "messageWindowHours": 12, "compact": true, "unknown": 1}"#,
        )
        .unwrap();
        let settings = from_value_lenient(&value);
        let defaults = Settings::default();
        assert_eq!(settings.notify_on_waiting, defaults.notify_on_waiting);
        assert!(settings.notify_on_finished);
        assert_eq!(settings.pr_poll_secs, defaults.pr_poll_secs);
        assert_eq!(settings.message_window_hours, 12);
        assert!(settings.compact);
        assert_eq!(settings.always_on_top, defaults.always_on_top);
    }

    #[test]
    fn files_with_a_bom_or_in_utf16_are_read() {
        let dir = scratch_dir("bom");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let json = r#"{"notifyOnWaiting": false, "prPollSecs": 300, "compact": true}"#;
        let expected = Settings {
            notify_on_waiting: false,
            pr_poll_secs: 300,
            compact: true,
            ..Settings::default()
        };

        let mut utf8_bom = vec![0xEF, 0xBB, 0xBF];
        utf8_bom.extend_from_slice(json.as_bytes());
        fs::write(&path, &utf8_bom).unwrap();
        assert_eq!(load(&path), expected, "UTF-8 with BOM");

        let mut utf16_le = vec![0xFF, 0xFE];
        utf16_le.extend(json.encode_utf16().flat_map(u16::to_le_bytes));
        fs::write(&path, &utf16_le).unwrap();
        assert_eq!(load(&path), expected, "UTF-16 LE (PowerShell 5.1)");

        let mut utf16_be = vec![0xFE, 0xFF];
        utf16_be.extend(json.encode_utf16().flat_map(u16::to_be_bytes));
        fs::write(&path, &utf16_be).unwrap();
        assert_eq!(load(&path), expected, "UTF-16 BE");

        fs::write(&path, [0xFF, 0xFE, 0x7B]).unwrap();
        assert_eq!(load(&path), Settings::default(), "odd UTF-16 length");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_round_trips_in_a_non_ascii_folder() {
        let dir = scratch_dir("Données Utilisateur");
        let path = dir.join("sous dossier").join(FILE_NAME);
        let settings = Settings {
            notify_on_finished: true,
            notify_on_error: false,
            include_cli_sessions: true,
            pr_poll_secs: 120,
            compact: true,
            always_on_top: false,
            ..Settings::default()
        };
        let (effective, written) = save_change(&path, &Settings::default(), &settings);
        written.unwrap();
        assert_eq!(effective, settings);
        let (again, written) = save_change(&path, &settings, &settings);
        written.unwrap();
        assert_eq!(again, settings);
        assert_eq!(load(&path), settings);
        assert!(!path.with_extension("json.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_toggle_keeps_fields_edited_by_hand_meanwhile() {
        let dir = scratch_dir("external-edit");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let in_use = Settings::default();
        // Edited by hand while the app runs, saved with a BOM, plus a key the
        // app does not know.
        let mut edited = vec![0xEF, 0xBB, 0xBF];
        edited.extend_from_slice(
            br#"{"prPollSecs": 300, "notifyOnWaiting": false, "futureKey": "kept"}"#,
        );
        fs::write(&path, edited).unwrap();

        let wanted = Settings {
            always_on_top: !in_use.always_on_top,
            ..in_use.clone()
        };
        let (effective, written) = save_change(&path, &in_use, &wanted);
        written.unwrap();
        assert_eq!(effective.always_on_top, wanted.always_on_top, "the toggle applies");
        assert_eq!(effective.pr_poll_secs, 300, "hand edit kept");
        assert!(!effective.notify_on_waiting, "hand edit kept");
        assert_eq!(effective.compact, in_use.compact);

        let on_disk = file_json(&path);
        assert_eq!(on_disk["prPollSecs"], 300);
        assert_eq!(on_disk["notifyOnWaiting"], false);
        assert_eq!(on_disk["alwaysOnTop"], wanted.always_on_top);
        assert_eq!(on_disk["futureKey"], "kept");
        assert_eq!(load(&path), effective);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_field_wins_over_the_file() {
        let old = Settings::default();
        let wanted = Settings {
            compact: true,
            ..old.clone()
        };
        let base = Settings {
            compact: false,
            pr_poll_secs: 90,
            ..old.clone()
        };
        let merged = merge_change(&old, &wanted, &base);
        assert!(merged.compact);
        assert_eq!(merged.pr_poll_secs, 90);
        assert_eq!(merge_change(&old, &old, &base), base, "no change keeps the file");
    }

    #[test]
    fn an_unparsable_file_is_backed_up_before_it_is_replaced() {
        let dir = scratch_dir("backup");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        fs::write(&path, b"{\"prPollSecs\": 300,").unwrap();
        let in_use = Settings::default();
        let wanted = Settings {
            compact: true,
            ..in_use.clone()
        };
        let (effective, written) = save_change(&path, &in_use, &wanted);
        written.unwrap();
        assert_eq!(effective, wanted);
        assert_eq!(
            fs::read(path.with_extension("json.bak")).unwrap(),
            b"{\"prPollSecs\": 300,"
        );
        assert_eq!(load(&path), wanted);
        let _ = fs::remove_dir_all(&dir);
    }
}
