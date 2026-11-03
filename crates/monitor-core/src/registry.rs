//! Live processes from `~/.claude/sessions/<pid>.json`.

use std::path::Path;

use serde_json::{Map, Value};

use crate::i18n;
use crate::model::RegistryEntry;

/// Largest accepted gap, in FILETIME units (100 ns), between the registry's
/// `procStart` and the creation time Windows reports for the pid.
///
/// On this machine the two were identical for every live session, so the
/// tolerance only absorbs a writer that would round the value (2 ms). A
/// reused pid belongs to a process created at least several milliseconds
/// later, far outside this window.
pub const PROC_START_TOLERANCE: u64 = 20_000;

/// Start values above this are .NET `DateTime` ticks (100 ns since year 1,
/// local time), not FILETIME: the CLI tells the two scales apart the same way.
const TICKS_SCALE_MIN: u64 = 300_000_000_000_000_000;
/// .NET ticks at 1601-01-01, the FILETIME epoch.
const TICKS_AT_FILETIME_EPOCH: u64 = 504_911_232_000_000_000;
/// Fifteen minutes in 100 ns units: every UTC offset is a multiple of it.
#[cfg(any(windows, test))]
const QUARTER_HOUR: u64 = 9_000_000_000;
/// Largest UTC offset (14 h), in 100 ns units.
#[cfg(any(windows, test))]
const MAX_UTC_OFFSET: u64 = 56 * QUARTER_HOUR;

/// Largest accepted gap, in seconds, between the registry's `procStart` on
/// macOS and the start time macOS reports for the pid. Both count whole
/// seconds, so on this machine they were equal for every live session.
#[cfg(target_os = "macos")]
const LSTART_TOLERANCE_SECS: u64 = 1;
/// Fifteen minutes in seconds.
#[cfg(target_os = "macos")]
const QUARTER_HOUR_SECS: u64 = 15 * 60;
/// Largest UTC offset (14 h), in seconds.
#[cfg(target_os = "macos")]
const MAX_UTC_OFFSET_SECS: u64 = 56 * QUARTER_HOUR_SECS;
/// Month names as `ps -o lstart` writes them in the C locale.
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Parses one registry file. None when the JSON is not a session entry.
pub fn parse_entry(json: &str) -> Option<RegistryEntry> {
    let json = json.strip_prefix('\u{feff}').unwrap_or(json);
    let value: Value = serde_json::from_str(json).ok()?;
    let obj = value.as_object()?;

    let pid = obj.get("pid").and_then(as_pid)?;
    let session_id = text(obj, "sessionId")?;

    Some(RegistryEntry {
        pid,
        session_id,
        cwd: text(obj, "cwd").unwrap_or_default(),
        started_at: timestamp(obj, "startedAt"),
        // The CLI reads `procStartFt ?? procStart`; newer builds write only
        // `procStartFt`.
        proc_start: obj
            .get("procStartFt")
            .and_then(as_filetime_string)
            .or_else(|| obj.get("procStart").and_then(as_filetime_string)),
        version: text(obj, "version"),
        kind: text(obj, "kind"),
        entrypoint: text(obj, "entrypoint"),
        host_session_id: text(obj, "hostSessionId"),
        name: text(obj, "name"),
        status: text(obj, "status"),
        waiting_for: text(obj, "waitingFor"),
        updated_at: timestamp(obj, "updatedAt"),
        status_updated_at: timestamp(obj, "statusUpdatedAt"),
    })
}

/// Reads every `<pid>.json` in `dir` and keeps the entries whose process is
/// still the one that wrote the file (see [`is_alive`]).
///
/// Files that cannot be read or parsed (for instance while being rewritten)
/// are skipped. Fails only when `dir` itself cannot be listed. The result is
/// sorted by pid, then session id.
pub fn read_live(dir: &Path) -> Result<Vec<RegistryEntry>, String> {
    read_live_after(dir, &[])
}

/// [`read_live`] for a caller that read the registry before: a file that
/// cannot be read or parsed now keeps the entry `previous` has for its pid,
/// so a process caught rewriting its file does not look gone. A file removed
/// takes its entry with it, and a kept entry must still be alive.
pub fn read_live_after(
    dir: &Path,
    previous: &[RegistryEntry],
) -> Result<Vec<RegistryEntry>, String> {
    let listing = std::fs::read_dir(dir).map_err(|e| {
        let dir = dir.display();
        i18n::current().pick(
            format!("unreadable registry ({dir}): {e}"),
            format!("registre illisible ({dir}) : {e}"),
        )
    })?;

    let mut entries: Vec<RegistryEntry> = listing
        .filter_map(Result::ok)
        .filter(|item| is_registry_file_name(&item.file_name()))
        .flat_map(|item| {
            let entry = std::fs::read(item.path())
                .ok()
                .and_then(|bytes| parse_entry(&String::from_utf8_lossy(&bytes)));
            match entry {
                Some(entry) => vec![entry],
                None => entries_of_file(previous, &item.file_name()),
            }
        })
        .filter(|entry| is_alive(entry.pid, entry.proc_start.as_deref()))
        .collect();

    entries.sort_by(|a, b| {
        a.pid
            .cmp(&b.pid)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    Ok(entries)
}

/// The entries of `previous` for the pid a `<pid>.json` file is named after.
fn entries_of_file(previous: &[RegistryEntry], file_name: &std::ffi::OsStr) -> Vec<RegistryEntry> {
    let pid = file_name
        .to_str()
        .and_then(|name| name.split('.').next())
        .and_then(|stem| stem.parse::<u32>().ok());
    previous
        .iter()
        .filter(|entry| Some(entry.pid) == pid)
        .cloned()
        .collect()
}

/// True when `pid` is running and, when `proc_start` is given, its creation
/// time matches, which rules out a reused pid. On Windows `proc_start` is a
/// FILETIME or .NET ticks, in decimal; on macOS it is `ps -o lstart` text in
/// UTC ([`lstart_as_unix_secs`]).
///
/// Claude Code runs under the user's own account, so its processes can always
/// be opened and their creation time read. When `proc_start` is known, a
/// process that cannot be opened (access denied) or whose creation time
/// cannot be read therefore belongs to someone else: the pid was reused.
/// Without `proc_start` (or with one in another format) such a process
/// counts as alive. Outside Windows and macOS, always true.
pub fn is_alive(pid: u32, proc_start: Option<&str>) -> bool {
    #[cfg(windows)]
    {
        let expected = proc_start.and_then(parse_filetime);
        match win::probe(pid) {
            win::Probe::Gone | win::Probe::Exited => false,
            win::Probe::Inaccessible | win::Probe::Running { created: None } => {
                expected.is_none()
            }
            win::Probe::Running {
                created: Some(created),
            } => expected.is_none_or(|expected| start_matches(expected, created)),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let expected = proc_start.and_then(lstart_as_unix_secs);
        match mac::probe(pid) {
            mac::Probe::Gone | mac::Probe::Exited => false,
            mac::Probe::Inaccessible | mac::Probe::Running { started: None } => {
                expected.is_none()
            }
            mac::Probe::Running {
                started: Some(started),
            } => expected.is_none_or(|expected| lstart_matches(expected, started)),
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (pid, proc_start);
        true
    }
}

/// Creation time of `pid` as a Windows FILETIME, when the process can be
/// queried. Always None outside Windows.
pub fn process_creation_filetime(pid: u32) -> Option<u64> {
    #[cfg(windows)]
    {
        match win::probe(pid) {
            win::Probe::Running { created } => created,
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

/// `<digits>.json`, which excludes the `<pid>.<hash>.key` files.
pub(crate) fn is_registry_file_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(dot) = name.rfind('.') else {
        return false;
    };
    let (stem, ext) = name.split_at(dot);
    ext.eq_ignore_ascii_case(".json")
        && !stem.is_empty()
        && stem.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(any(windows, test))]
fn parse_filetime(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok()
}

/// A registry start value on the FILETIME scale. .NET ticks stay in local
/// time: the result is off by the UTC offset, enough to order two processes
/// but not to compare it with a creation time ([`start_matches`] does that).
pub(crate) fn start_as_filetime(start: u64) -> Option<u64> {
    if start <= TICKS_SCALE_MIN {
        Some(start)
    } else {
        start.checked_sub(TICKS_AT_FILETIME_EPOCH)
    }
}

/// True when a registry start value matches the creation FILETIME Windows
/// reports. A FILETIME must match within [`PROC_START_TOLERANCE`]. .NET ticks
/// are local time, so they may differ from it by a whole number of quarter
/// hours (the UTC offset), within the same tolerance.
#[cfg(any(windows, test))]
fn start_matches(expected: u64, created: u64) -> bool {
    if expected <= TICKS_SCALE_MIN {
        return created.abs_diff(expected) <= PROC_START_TOLERANCE;
    }
    let Some(naive) = start_as_filetime(expected) else {
        return false;
    };
    let diff = naive.abs_diff(created);
    if diff > MAX_UTC_OFFSET + PROC_START_TOLERANCE {
        return false;
    }
    let rest = diff % QUARTER_HOUR;
    rest <= PROC_START_TOLERANCE || QUARTER_HOUR - rest <= PROC_START_TOLERANCE
}

/// `procStart` as the CLI writes it on macOS, `ps -o lstart` text in UTC such
/// as `Tue Nov  3 21:34:47 2026`, in seconds since the Unix epoch. None for
/// any other text. The weekday is not checked.
pub(crate) fn lstart_as_unix_secs(text: &str) -> Option<i64> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [_weekday, month, day, time, year] = parts[..] else {
        return None;
    };
    let month = MONTHS.iter().position(|name| *name == month)? as i64 + 1;
    let day: i64 = day.parse().ok().filter(|day| (1..=31).contains(day))?;
    let year: i64 = year.parse().ok().filter(|year| (1970..=9999).contains(year))?;
    let mut clock = time.split(':').map(|part| part.parse::<i64>().ok());
    let (Some(Some(hours)), Some(Some(minutes)), Some(Some(seconds)), None) =
        (clock.next(), clock.next(), clock.next(), clock.next())
    else {
        return None;
    };
    if !(0..24).contains(&hours) || !(0..60).contains(&minutes) || !(0..=60).contains(&seconds) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hours * 3_600 + minutes * 60 + seconds)
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar
/// (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    // Months counted from March, so that February ends the year.
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// True when a registry start value read as UTC ([`lstart_as_unix_secs`])
/// matches the start time macOS reports, in Unix seconds. Within
/// [`LSTART_TOLERANCE_SECS`], or a whole number of quarter hours off: a
/// build of the CLI writing local time would be off by the UTC offset.
#[cfg(target_os = "macos")]
fn lstart_matches(expected: i64, started: i64) -> bool {
    let diff = expected.abs_diff(started);
    if diff > MAX_UTC_OFFSET_SECS + LSTART_TOLERANCE_SECS {
        return false;
    }
    let rest = diff % QUARTER_HOUR_SECS;
    rest <= LSTART_TOLERANCE_SECS || QUARTER_HOUR_SECS - rest <= LSTART_TOLERANCE_SECS
}

/// Non-blank string field.
fn text(obj: &Map<String, Value>, key: &str) -> Option<String> {
    match obj.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Epoch milliseconds from an integer, a float or a numeric string.
fn timestamp(obj: &Map<String, Value>, key: &str) -> Option<i64> {
    match obj.get(key)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().and_then(float_to_i64)),
        Value::String(s) => {
            let s = s.trim();
            s.parse::<i64>()
                .ok()
                .or_else(|| s.parse::<f64>().ok().and_then(float_to_i64))
        }
        _ => None,
    }
}

fn float_to_i64(f: f64) -> Option<i64> {
    let t = f.trunc();
    (t.is_finite() && t >= i64::MIN as f64 && t < i64::MAX as f64).then_some(t as i64)
}

/// A positive pid that fits in `u32`, from an integer, an integral float or
/// a decimal string.
fn as_pid(v: &Value) -> Option<u32> {
    let wide: u64 = match v {
        Value::Number(n) => n.as_u64().or_else(|| {
            let f = n.as_f64()?;
            let integral = f.is_finite() && f.fract() == 0.0;
            (integral && (0.0..=f64::from(u32::MAX)).contains(&f)).then_some(f as u64)
        })?,
        Value::String(s) => s.trim().parse::<u64>().ok()?,
        _ => return None,
    };
    u32::try_from(wide).ok().filter(|&pid| pid != 0)
}

/// `procStart` / `procStartFt` are written as decimal strings; also accept a
/// bare integer.
fn as_filetime_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Value::Number(n) => n.as_u64().map(|u| u.to_string()),
        _ => None,
    }
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, FILETIME, GetLastError, HANDLE, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// What Windows says about a pid.
    pub(super) enum Probe {
        /// Running; creation FILETIME when readable.
        Running { created: Option<u64> },
        /// The handle opened but the process has exited.
        Exited,
        /// No such process (invalid parameter or any error but access denied).
        Gone,
        /// The process exists but cannot be opened.
        Inaccessible,
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: the handle came from a successful OpenProcess and is
            // closed exactly once, here.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub(super) fn probe(pid: u32) -> Probe {
        // SAFETY: plain Win32 call with no pointer arguments; a null handle
        // is handled right below.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if raw.is_null() {
            // SAFETY: reads the calling thread's last-error value.
            let err = unsafe { GetLastError() };
            return if err == ERROR_ACCESS_DENIED {
                Probe::Inaccessible
            } else {
                Probe::Gone
            };
        }
        let handle = OwnedHandle(raw);

        let mut code = 0u32;
        // SAFETY: valid process handle and a valid out pointer.
        let ok = unsafe { GetExitCodeProcess(handle.0, &mut code) };
        if ok != 0 && code != STILL_ACTIVE as u32 {
            return Probe::Exited;
        }

        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: valid process handle and four valid out pointers.
        let ok = unsafe {
            GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user)
        };
        let created = (ok != 0).then(|| {
            (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime)
        });
        Probe::Running { created }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    /// What macOS says about a pid.
    pub(super) enum Probe {
        /// Running; start time in Unix seconds when readable.
        Running { started: Option<i64> },
        /// Ended, but not yet reaped by its parent.
        Exited,
        /// No such process.
        Gone,
        /// The process exists but cannot be queried.
        Inaccessible,
    }

    /// `SZOMB` in `<sys/proc.h>`: a process that has ended.
    const SZOMB: u32 = 5;

    pub(super) fn probe(pid: u32) -> Probe {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return Probe::Gone;
        };
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is a writable buffer of `size` bytes for the
        // duration of the call.
        let written = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size)
        };
        if written == size {
            // SAFETY: the call filled the whole structure, which was zeroed
            // before and holds only integers and byte arrays anyway.
            let info = unsafe { info.assume_init() };
            if info.pbi_status == SZOMB {
                return Probe::Exited;
            }
            return Probe::Running {
                started: i64::try_from(info.pbi_start_tvsec).ok(),
            };
        }
        // Signal 0 only checks the pid: EPERM means it belongs to someone else.
        // SAFETY: no pointer arguments.
        let found = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if found {
            Probe::Inaccessible
        } else {
            Probe::Gone
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const FULL: &str = include_str!("../tests/fixtures/registry/full.json");
    const MINIMAL: &str = include_str!("../tests/fixtures/registry/minimal.json");
    const PARTIAL: &str = include_str!("../tests/fixtures/registry/partial.json");
    const ODD_TYPES: &str = include_str!("../tests/fixtures/registry/odd_types.json");
    const PROC_START_FT: &str = include_str!("../tests/fixtures/registry/proc_start_ft.json");
    const PROC_START_BOTH: &str = include_str!("../tests/fixtures/registry/proc_start_both.json");

    /// A pid Windows never hands out (pids are multiples of 4).
    const DEAD_PID: u32 = 4_294_967_290;

    #[test]
    fn parses_full_entry() {
        let e = parse_entry(FULL).expect("full entry");
        assert_eq!(
            e,
            RegistryEntry {
                pid: 4242,
                session_id: "00000000-0000-4000-8000-000000000001".into(),
                cwd: "D:\\Données Utilisateur\\Projets\\demo app".into(),
                started_at: Some(1_700_000_000_000),
                proc_start: Some("133444736000000000".into()),
                version: Some("9.9.9".into()),
                kind: Some("interactive".into()),
                entrypoint: Some("claude-desktop".into()),
                host_session_id: Some("local_00000000-0000-4000-8000-0000000000aa".into()),
                name: Some("Session de démonstration".into()),
                status: Some("waiting".into()),
                waiting_for: Some("permission prompt".into()),
                updated_at: Some(1_700_000_005_000),
                status_updated_at: Some(1_700_000_004_000),
            }
        );
    }

    #[test]
    fn parses_minimal_entry() {
        let e = parse_entry(MINIMAL).expect("minimal entry");
        assert_eq!(e.pid, 7);
        assert_eq!(e.session_id, "00000000-0000-4000-8000-000000000002");
        assert_eq!(e.cwd, "");
        assert_eq!(e.status, None);
        assert_eq!(e.proc_start, None);
        assert_eq!(e.host_session_id, None);
    }

    #[test]
    fn accepts_bom() {
        let with_bom = format!("\u{feff}{MINIMAL}");
        assert_eq!(parse_entry(&with_bom).map(|e| e.pid), Some(7));
    }

    #[test]
    fn rejects_missing_or_invalid_identity() {
        assert!(parse_entry(r#"{"sessionId":"s"}"#).is_none());
        assert!(parse_entry(r#"{"pid":12}"#).is_none());
        assert!(parse_entry(r#"{"pid":12,"sessionId":""}"#).is_none());
        assert!(parse_entry(r#"{"pid":12,"sessionId":"   "}"#).is_none());
        assert!(parse_entry(r#"{"pid":12,"sessionId":42}"#).is_none());
        assert!(parse_entry(r#"{"pid":0,"sessionId":"s"}"#).is_none());
        assert!(parse_entry(r#"{"pid":-4,"sessionId":"s"}"#).is_none());
        assert!(parse_entry(r#"{"pid":1.5,"sessionId":"s"}"#).is_none());
        assert!(parse_entry(r#"{"pid":4294967296,"sessionId":"s"}"#).is_none());
        assert!(parse_entry(r#"{"pid":"abc","sessionId":"s"}"#).is_none());
        assert!(parse_entry(r#"{"pid":null,"sessionId":"s"}"#).is_none());
        assert!(parse_entry(r#"{"pid":[12],"sessionId":"s"}"#).is_none());
    }

    #[test]
    fn rejects_garbage() {
        for input in ["", "   ", "not json", "null", "42", "\"text\"", "[]", "[{\"pid\":1}]", "{"] {
            assert!(parse_entry(input).is_none(), "accepted {input:?}");
        }
    }

    #[test]
    fn rejects_partial_json() {
        assert!(parse_entry(PARTIAL).is_none());
        for cut in 0..FULL.trim_end().len() {
            if FULL.is_char_boundary(cut) {
                assert!(parse_entry(&FULL[..cut]).is_none(), "accepted prefix {cut}");
            }
        }
    }

    #[test]
    fn tolerates_unexpected_types() {
        let e = parse_entry(ODD_TYPES).expect("odd entry");
        assert_eq!(e.pid, 4242);
        assert_eq!(e.cwd, "");
        assert_eq!(e.started_at, Some(1_700_000_000_000));
        assert_eq!(e.proc_start.as_deref(), Some("133444736000000000"));
        assert_eq!(e.version, None);
        assert_eq!(e.kind, None);
        assert_eq!(e.entrypoint, None);
        assert_eq!(e.host_session_id, None);
        assert_eq!(e.name, None);
        assert_eq!(e.status, None);
        assert_eq!(e.waiting_for.as_deref(), Some("dialog open"));
        assert_eq!(e.updated_at, Some(1_700_000_005_000));
        assert_eq!(e.status_updated_at, None);
    }

    #[test]
    fn accepts_numbers_as_floats_or_strings() {
        let e = parse_entry(concat!(
            r#"{"pid":4242.0,"sessionId":"s","startedAt":1.7e12,"#,
            r#""updatedAt":" 1700000000123 ","statusUpdatedAt":"1700000000123.9"}"#,
        ))
        .expect("entry");
        assert_eq!(e.pid, 4242);
        assert_eq!(e.started_at, Some(1_700_000_000_000));
        assert_eq!(e.updated_at, Some(1_700_000_000_123));
        assert_eq!(e.status_updated_at, Some(1_700_000_000_123));

        let e = parse_entry(r#"{"pid":" 99 ","sessionId":"s","procStart":-5}"#).expect("entry");
        assert_eq!(e.pid, 99);
        assert_eq!(e.proc_start, None);
    }

    #[test]
    fn reads_proc_start_ft_first() {
        let only_ft = parse_entry(PROC_START_FT).expect("entry with procStartFt");
        assert_eq!(only_ft.proc_start.as_deref(), Some("133444736000000123"));
        assert_eq!(only_ft.status.as_deref(), Some("busy"));

        let both = parse_entry(PROC_START_BOTH).expect("entry with both fields");
        assert_eq!(both.proc_start.as_deref(), Some("133444736000000456"));

        // An unusable procStartFt falls back to procStart.
        let e = parse_entry(r#"{"pid":5,"sessionId":"s","procStartFt":"","procStart":"77"}"#)
            .expect("entry");
        assert_eq!(e.proc_start.as_deref(), Some("77"));
        let e = parse_entry(r#"{"pid":5,"sessionId":"s","procStartFt":null}"#).expect("entry");
        assert_eq!(e.proc_start, None);
    }

    #[test]
    fn start_values_on_both_scales() {
        let created: u64 = 134_371_537_278_822_954;
        // FILETIME: exact within the tolerance.
        assert!(start_matches(created, created));
        assert!(start_matches(created + PROC_START_TOLERANCE, created));
        assert!(!start_matches(created + PROC_START_TOLERANCE + 1, created));
        assert!(!start_matches(1, created));

        // .NET ticks, UTC or shifted by a whole UTC offset.
        let ticks = created + TICKS_AT_FILETIME_EPOCH;
        assert!(ticks > TICKS_SCALE_MIN);
        assert!(start_matches(ticks, created));
        assert!(start_matches(ticks + 8 * QUARTER_HOUR, created), "UTC+2");
        assert!(start_matches(ticks - 20 * QUARTER_HOUR + 1_000, created), "UTC-5");
        assert!(start_matches(ticks + 22 * QUARTER_HOUR, created), "UTC+5:30");
        assert!(!start_matches(ticks + 8 * QUARTER_HOUR + 10_000_000, created), "1 s off");
        assert!(!start_matches(ticks + 60 * QUARTER_HOUR, created), "beyond 14 h");
        assert!(!start_matches(TICKS_SCALE_MIN + 1, created));
    }

    #[test]
    fn registry_file_names() {
        let ok = |n: &str| is_registry_file_name(std::ffi::OsStr::new(n));
        assert!(ok("36220.json"));
        assert!(ok("4.JSON"));
        assert!(!ok("36220.0a1b.key"));
        assert!(!ok("36220.json.key"));
        assert!(!ok(".json"));
        assert!(!ok("12a.json"));
        assert!(!ok("abc.json"));
        assert!(!ok("36220.json.tmp"));
        assert!(!ok("36220"));
        assert!(!ok("readme.txt"));
    }

    #[test]
    fn current_process_is_alive() {
        assert!(is_alive(std::process::id(), None));
    }

    #[cfg(windows)]
    #[test]
    fn checks_process_on_windows() {
        let me = std::process::id();
        assert!(!is_alive(DEAD_PID, None));
        assert!(!is_alive(0, None));

        let created = process_creation_filetime(me).expect("own creation time");
        assert!(is_alive(me, Some(&created.to_string())));
        assert!(is_alive(me, Some(&(created + PROC_START_TOLERANCE).to_string())));
        assert!(!is_alive(me, Some(&(created + PROC_START_TOLERANCE + 1).to_string())));
        assert!(!is_alive(me, Some("1")));
        assert!(is_alive(me, Some("not a filetime")));
        let ticks = created + TICKS_AT_FILETIME_EPOCH;
        assert!(is_alive(me, Some(&ticks.to_string())));

        // The System process always exists, whether or not it can be opened.
        assert!(is_alive(4, None));
        // With a start time, a process of another account is a reused pid:
        // Claude Code could always open its own processes.
        assert!(!is_alive(4, Some("1")));
        assert!(!is_alive(4, Some(&created.to_string())));
    }

    #[test]
    fn reads_lstart_text_as_utc() {
        assert_eq!(lstart_as_unix_secs("Thu Jan  1 00:00:00 1970"), Some(0));
        assert_eq!(lstart_as_unix_secs("Wed Mar  1 00:00:00 2000"), Some(951_868_800));
        assert_eq!(lstart_as_unix_secs("Tue Feb 29 23:59:59 2028"), Some(1_835_481_599));
        assert_eq!(lstart_as_unix_secs(" Tue Nov 3 21:34:47 2026 "), Some(1_793_741_687));
        for text in [
            "",
            "133444736000000000",
            "Tue Nov  3 21:34:47",
            "Tue Nov  3 21:34 2026",
            "Tue Nov  3 21:34:47:00 2026",
            "Tue Nov  3 24:00:00 2026",
            "Tue Nov 32 21:34:47 2026",
            "mar. nov.  3 21:34:47 2026",
            "Tue Nov  3 21:34:47 2026 UTC",
        ] {
            assert_eq!(lstart_as_unix_secs(text), None, "{text:?}");
        }
    }

    /// `secs` as `ps -o lstart` writes it in UTC.
    #[cfg(target_os = "macos")]
    fn lstart_text(secs: i64) -> String {
        const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
        let days = secs.div_euclid(86_400);
        let time = secs.rem_euclid(86_400);
        let (year, month, day) = (1970..)
            .flat_map(|year| (1..=12).map(move |month| (year, month)))
            .find_map(|(year, month)| {
                let first = days_from_civil(year, month, 1);
                let next = if month == 12 {
                    days_from_civil(year + 1, 1, 1)
                } else {
                    days_from_civil(year, month + 1, 1)
                };
                (first..next).contains(&days).then(|| (year, month, days - first + 1))
            })
            .expect("a date after 1970");
        format!(
            "{} {} {day:>2} {:02}:{:02}:{:02} {year}",
            DAYS[days.rem_euclid(7) as usize],
            MONTHS[month as usize - 1],
            time / 3_600,
            time / 60 % 60,
            time % 60,
        )
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn lstart_text_round_trips() {
        assert_eq!(lstart_text(1_793_741_687), "Tue Nov  3 21:34:47 2026");
        assert_eq!(lstart_text(1_835_481_599), "Tue Feb 29 23:59:59 2028");
        for secs in [0, 951_868_800, 1_793_741_687, 1_835_481_599] {
            assert_eq!(lstart_as_unix_secs(&lstart_text(secs)), Some(secs));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn checks_process_on_macos() {
        let me = std::process::id();
        assert!(!is_alive(DEAD_PID, None));

        let mac::Probe::Running {
            started: Some(started),
        } = mac::probe(me)
        else {
            panic!("own start time");
        };
        assert!(is_alive(me, Some(&lstart_text(started))));
        assert!(is_alive(me, Some(&lstart_text(started + 1))));
        assert!(!is_alive(me, Some(&lstart_text(started + 2))));
        assert!(!is_alive(me, Some("Thu Jan  1 00:00:01 1970")));
        assert!(is_alive(me, Some("not a start time")));
        assert!(is_alive(me, Some(&lstart_text(started + 3_600))), "local time, UTC+1");
        assert!(is_alive(me, Some(&lstart_text(started - 19_800))), "local time, UTC-5:30");
        assert!(!is_alive(me, Some(&lstart_text(started + 15 * 3_600))), "beyond 14 h");

        // launchd always runs, as root.
        assert!(is_alive(1, None));
        // With a start time, a process of another account is a reused pid.
        assert!(!is_alive(1, Some("Thu Jan  1 00:00:01 1970")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn read_live_rejects_reused_pid_on_macos() {
        let me = std::process::id();
        let mac::Probe::Running {
            started: Some(started),
        } = mac::probe(me)
        else {
            panic!("own start time");
        };
        let dir = TempDir::new("reuse-mac");
        dir.write("1.json", &entry_json(me, "current", Some(&lstart_text(started))));
        dir.write("2.json", &entry_json(me, "previous-owner", Some("Fri Jan  2 10:00:00 1970")));

        let live = read_live(&dir.0).expect("read_live");
        let ids: Vec<&str> = live.iter().map(|e| e.session_id.as_str()).collect();
        assert_eq!(ids, ["current"]);
    }

    /// Fresh scratch directory under the system temp folder, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "monitor-core-registry-{tag}-{}-{nanos} é",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn write(&self, name: &str, content: &str) {
            std::fs::write(self.0.join(name), content).expect("write fixture");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry_json(pid: u32, session_id: &str, proc_start: Option<&str>) -> String {
        let mut obj = serde_json::json!({
            "pid": pid,
            "sessionId": session_id,
            "cwd": "D:\\Données Utilisateur\\Projets\\demo app",
            "hostSessionId": "local_00000000-0000-4000-8000-0000000000aa",
            "status": "busy",
        });
        if let Some(p) = proc_start {
            obj["procStart"] = Value::String(p.to_owned());
        }
        obj.to_string()
    }

    #[test]
    fn read_live_filters_files_and_dead_processes() {
        let me = std::process::id();
        let dir = TempDir::new("live");

        dir.write(&format!("{me}.json"), &entry_json(me, "a-alive", None));
        dir.write(&format!("{DEAD_PID}.json"), &entry_json(DEAD_PID, "dead", None));
        dir.write(&format!("{me}.0123456789abcdef.key"), "opaque");
        dir.write("readme.txt", "hello");
        dir.write("abc.json", &entry_json(me, "wrong-name", None));
        dir.write("12a.json", &entry_json(me, "wrong-name", None));
        dir.write("7.json", PARTIAL);
        dir.write("8.json", "\u{0}\u{0}\u{0}");
        std::fs::create_dir(dir.0.join("9.json")).expect("create dir");

        let live = read_live(&dir.0).expect("read_live");
        let ids: Vec<&str> = live.iter().map(|e| e.session_id.as_str()).collect();
        assert_eq!(ids, ["a-alive"]);
        assert_eq!(live[0].pid, me);
        assert_eq!(live[0].cwd, "D:\\Données Utilisateur\\Projets\\demo app");
    }

    #[test]
    fn read_live_keeps_duplicates_sorted() {
        let me = std::process::id();
        let dir = TempDir::new("sort");

        // Same host session written twice (process restart): both kept,
        // in a stable order whatever the directory order.
        dir.write("1.json", &entry_json(me, "z-second", None));
        dir.write("2.json", &entry_json(me, "a-first", None));

        let live = read_live(&dir.0).expect("read_live");
        let ids: Vec<&str> = live.iter().map(|e| e.session_id.as_str()).collect();
        assert_eq!(ids, ["a-first", "z-second"]);
    }

    #[cfg(windows)]
    #[test]
    fn read_live_rejects_reused_pid() {
        let me = std::process::id();
        let created = process_creation_filetime(me).expect("own creation time");
        let dir = TempDir::new("reuse");

        dir.write("1.json", &entry_json(me, "current", Some(&created.to_string())));
        dir.write("2.json", &entry_json(me, "previous-owner", Some("130000000000000000")));
        // A stale file whose pid now belongs to a system process.
        dir.write("4.json", &entry_json(4, "system-reuse", Some(&created.to_string())));

        let live = read_live(&dir.0).expect("read_live");
        let ids: Vec<&str> = live.iter().map(|e| e.session_id.as_str()).collect();
        assert_eq!(ids, ["current"]);
    }

    #[test]
    fn a_file_being_rewritten_keeps_its_previous_entry() {
        let me = std::process::id();
        let dir = TempDir::new("rewrite");
        let name = format!("{me}.json");
        dir.write(&name, &entry_json(me, "kept", None));
        let before = read_live(&dir.0).expect("read_live");
        assert_eq!(before.len(), 1);

        dir.write(&name, "{\"pid\":");
        assert!(read_live(&dir.0).expect("read_live").is_empty(), "nothing to fall back on");
        assert_eq!(read_live_after(&dir.0, &before).expect("read_live_after"), before);

        std::fs::remove_file(dir.0.join(&name)).expect("remove");
        let after = read_live_after(&dir.0, &before).expect("read_live_after");
        assert!(after.is_empty(), "a file removed takes its entry with it");
    }

    #[test]
    fn a_dead_process_keeps_no_entry() {
        let dir = TempDir::new("rewrite-dead");
        dir.write(&format!("{DEAD_PID}.json"), "");
        let previous = [RegistryEntry {
            pid: DEAD_PID,
            session_id: "dead".into(),
            ..RegistryEntry::default()
        }];
        assert!(read_live_after(&dir.0, &previous).expect("read_live_after").is_empty());
    }

    #[test]
    fn start_values_reach_the_filetime_scale() {
        assert_eq!(start_as_filetime(134_444_736_000_000_000), Some(134_444_736_000_000_000));
        assert_eq!(start_as_filetime(639_355_968_000_000_000), Some(134_444_736_000_000_000));
        assert_eq!(start_as_filetime(TICKS_SCALE_MIN + 1), None);
    }

    #[test]
    fn read_live_fails_on_missing_dir() {
        let dir = TempDir::new("missing");
        let missing = dir.0.join("does-not-exist");
        assert!(read_live(&missing).is_err());
    }

    /// Read-only check against the real registry. Prints pids, statuses and
    /// the gap between `procStart` and the real creation time; no names.
    /// Run with `cargo test registry::tests::live_registry -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_registry() {
        let dir = crate::paths::sessions_dir();
        let mut all = Vec::new();
        for item in std::fs::read_dir(&dir).expect("sessions dir").flatten() {
            if !is_registry_file_name(&item.file_name()) {
                continue;
            }
            let Ok(bytes) = std::fs::read(item.path()) else {
                continue;
            };
            if let Some(e) = parse_entry(&String::from_utf8_lossy(&bytes)) {
                all.push(e);
            }
        }
        all.sort_by_key(|e| e.pid);
        for e in &all {
            let expected = e.proc_start.as_deref().and_then(parse_filetime);
            let actual = process_creation_filetime(e.pid);
            let gap = match (expected, actual) {
                (Some(x), Some(a)) => format!("{}", i128::from(a) - i128::from(x)),
                _ => "-".into(),
            };
            #[cfg(target_os = "macos")]
            let gap = match (e.proc_start.as_deref().and_then(lstart_as_unix_secs), mac::probe(e.pid)) {
                (Some(x), mac::Probe::Running { started: Some(a) }) => format!("{} s", a - x),
                _ => gap,
            };
            println!(
                "file pid={} status={:?} alive={} procStart-gap={gap}",
                e.pid,
                e.status,
                is_alive(e.pid, e.proc_start.as_deref())
            );
        }
        let live = read_live(&dir).expect("read_live");
        for e in &live {
            println!(
                "live pid={} status={} waitingFor={}",
                e.pid,
                e.status.as_deref().unwrap_or("-"),
                e.waiting_for.as_deref().unwrap_or("-")
            );
        }
        println!("live pids: {:?}", live.iter().map(|e| e.pid).collect::<Vec<_>>());
    }
}
