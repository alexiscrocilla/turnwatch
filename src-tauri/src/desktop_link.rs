//! Brings a session forward in Claude Desktop.
//!
//! Claude Desktop (3.1623) handles `claude://code/continue?session=<local_id>`:
//! it looks the id up among its non-archived Code sessions and navigates its
//! main window to that session (falling back to the Code home page). Desktop's
//! own Windows jump list builds the same link. The link only accepts ids that
//! match `^local_[A-Za-z0-9-]{1,64}$`.
//!
//! Opening a `claude://` link starts Desktop when it is not running, so the
//! link is only used when a Desktop process already exists. The main window is
//! then raised directly as well, since Windows may not let Desktop take the
//! foreground on its own.

use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

/// Deep link that opens a Desktop Code session.
const CONTINUE_LINK: &str = "claude://code/continue?session=";

/// True for a Desktop session id accepted by the deep link.
pub fn is_link_session_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("local_") else {
        return false;
    };
    (1..=64).contains(&rest.len())
        && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The deep link for a session key, when the key is a Desktop session id.
pub fn session_link(key: &str) -> Option<String> {
    is_link_session_id(key).then(|| format!("{CONTINUE_LINK}{key}"))
}

/// Error for a session Claude Desktop cannot show (started in a terminal).
pub const NOT_IN_DESKTOP: &str = "Cette session ne tourne pas dans Claude Desktop.";
const DESKTOP_CLOSED: &str = "Claude Desktop n'est pas ouvert.";
const NOT_SHOWN: &str = "Impossible d'afficher la fenêtre de Claude Desktop.";

/// Shows the session `key` in Claude Desktop: hands its deep link to Desktop
/// and raises Desktop's main window. `before_link` runs right before the link
/// is opened. Returns true when Desktop was asked to show the session (the
/// link was handed over), false when only its window could be raised. A key
/// that is not a Desktop id fails without touching Desktop. Errors are French
/// messages meant for the UI.
pub fn open_session(
    app: &AppHandle,
    key: &str,
    before_link: impl FnOnce(),
) -> Result<bool, String> {
    let link = link_for(key)?;
    let windows = win::desktop_windows();
    if windows.is_empty() {
        return Err(DESKTOP_CLOSED.into());
    }

    before_link();
    win::allow_any_foreground();
    let linked = app.opener().open_url(link, None::<&str>).is_ok();
    let focused = win::focus_main(&windows);
    outcome(linked, focused)
}

/// The deep link to open for `key`, or the error for a session Desktop
/// cannot show. Checked before anything touches Desktop.
fn link_for(key: &str) -> Result<String, String> {
    session_link(key).ok_or_else(|| NOT_IN_DESKTOP.to_owned())
}

/// What [`open_session`] reports once the link was tried and the window
/// raised (or not).
fn outcome(linked: bool, focused: bool) -> Result<bool, String> {
    if linked {
        Ok(true)
    } else if focused {
        Ok(false)
    } else {
        Err(NOT_SHOWN.into())
    }
}

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        ASFW_ANY, AllowSetForegroundWindow, EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow,
        GetWindowLongPtrW, GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible, SW_RESTORE, SetForegroundWindow, ShowWindow, WS_EX_TOOLWINDOW,
    };
    use windows_sys::core::BOOL;

    /// A top-level window owned by a Claude Desktop process.
    pub struct DesktopWindow {
        hwnd: HWND,
        /// Visible, titled, unowned and not a tool window.
        pub(super) main_candidate: bool,
    }

    /// Every top-level window (hidden ones included) of the Claude Desktop
    /// processes. Empty when Desktop is not running.
    pub fn desktop_windows() -> Vec<DesktopWindow> {
        let mut handles: Vec<HWND> = Vec::new();
        // SAFETY: the callback only pushes into the Vec passed through `lparam`,
        // which outlives the synchronous EnumWindows call.
        unsafe {
            EnumWindows(Some(collect), &mut handles as *mut Vec<HWND> as LPARAM);
        }

        let mut is_desktop: HashMap<u32, bool> = HashMap::new();
        handles
            .into_iter()
            .filter_map(|hwnd| {
                let mut pid = 0u32;
                // SAFETY: plain Win32 query on a window handle from EnumWindows.
                unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
                if pid == 0 {
                    return None;
                }
                let desktop = *is_desktop
                    .entry(pid)
                    .or_insert_with(|| process_image(pid).is_some_and(|p| is_desktop_image(&p)));
                desktop.then(|| DesktopWindow {
                    hwnd,
                    main_candidate: is_main_candidate(hwnd),
                })
            })
            .collect()
    }

    /// Restores and raises Desktop's main window. False when there is no
    /// visible window or Windows refused the focus change.
    pub fn focus_main(windows: &[DesktopWindow]) -> bool {
        let Some(window) = windows.iter().find(|w| w.main_candidate) else {
            return false;
        };
        // SAFETY: plain Win32 calls on a window handle from EnumWindows; a stale
        // handle makes them fail, not misbehave.
        unsafe {
            if IsIconic(window.hwnd) != 0 {
                ShowWindow(window.hwnd, SW_RESTORE);
            }
            SetForegroundWindow(window.hwnd) != 0
        }
    }

    /// Lets the process that handles the deep link take the foreground.
    pub fn allow_any_foreground() {
        // SAFETY: no pointer arguments.
        unsafe {
            AllowSetForegroundWindow(ASFW_ANY);
        }
    }

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `&mut Vec<HWND>` given to EnumWindows above.
        let handles = unsafe { &mut *(lparam as *mut Vec<HWND>) };
        handles.push(hwnd);
        1
    }

    fn is_main_candidate(hwnd: HWND) -> bool {
        // SAFETY: plain Win32 queries on a window handle from EnumWindows.
        unsafe {
            IsWindowVisible(hwnd) != 0
                && GetWindowTextLengthW(hwnd) > 0
                && GetWindow(hwnd, GW_OWNER).is_null()
                && (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW) == 0
        }
    }

    /// Full path of a process's executable, if it can be queried.
    fn process_image(pid: u32) -> Option<OsString> {
        // SAFETY: the handle is checked for null and always closed; the buffer
        // length passed matches the buffer.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return None;
            }
            let mut buffer = vec![0u16; 32 * 1024];
            let mut len = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                buffer.as_mut_ptr(),
                &mut len,
            );
            CloseHandle(process);
            (ok != 0).then(|| OsString::from_wide(&buffer[..len as usize]))
        }
    }

    /// Claude Desktop's own executable: the Store package (`WindowsApps\Claude_…`)
    /// or the Squirrel install (`AnthropicClaude\`). Excludes the Claude Code
    /// CLI binaries that Desktop runs from `%APPDATA%\Claude\claude-code\`.
    pub(super) fn is_desktop_image(path: &std::ffi::OsStr) -> bool {
        let lower = path.to_string_lossy().to_lowercase();
        lower.ends_with("\\claude.exe")
            && (lower.contains("\\windowsapps\\claude_") || lower.contains("\\anthropicclaude\\"))
    }
}

#[cfg(not(windows))]
mod win {
    pub struct DesktopWindow;

    pub fn desktop_windows() -> Vec<DesktopWindow> {
        Vec::new()
    }

    pub fn focus_main(_windows: &[DesktopWindow]) -> bool {
        false
    }

    pub fn allow_any_foreground() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_desktop_ids_only() {
        assert!(is_link_session_id("local_cc5b463c-4454-4fdc-b373-a46663f78260"));
        assert!(!is_link_session_id("local_"));
        assert!(!is_link_session_id("cli:0171a9ec-7b19-487a-9495-893e41e304de"));
        assert!(!is_link_session_id("local_abc&x=1"));
        assert!(!is_link_session_id("local_abc def"));
        assert!(!is_link_session_id(&format!("local_{}", "a".repeat(65))));
        assert!(is_link_session_id(&format!("local_{}", "a".repeat(64))));
    }

    #[test]
    fn only_a_handed_over_link_counts_as_shown() {
        assert_eq!(outcome(true, true), Ok(true));
        assert_eq!(outcome(true, false), Ok(true), "Desktop may focus itself");
        assert_eq!(outcome(false, true), Ok(false), "raised, but not on the session");
        assert_eq!(outcome(false, false), Err(NOT_SHOWN.to_owned()));
    }

    #[test]
    fn terminal_sessions_never_touch_desktop() {
        // `open_session` checks this first, before enumerating Desktop's
        // windows, opening a link or running `before_link`.
        for key in ["cli:0171a9ec-7b19-487a-9495-893e41e304de", "local_", "", "local_a b"] {
            assert_eq!(link_for(key), Err(NOT_IN_DESKTOP.to_owned()), "{key}");
        }
        assert_eq!(
            link_for("local_cc5b463c-4454").as_deref(),
            Ok("claude://code/continue?session=local_cc5b463c-4454")
        );
    }

    #[test]
    fn builds_continue_link() {
        assert_eq!(
            session_link("local_cc5b463c-4454").as_deref(),
            Some("claude://code/continue?session=local_cc5b463c-4454")
        );
        assert_eq!(session_link("cli:abc"), None);
    }

    #[cfg(windows)]
    #[test]
    fn recognizes_desktop_executables() {
        use std::ffi::OsStr;
        assert!(win::is_desktop_image(OsStr::new(
            r"C:\Program Files\WindowsApps\Claude_3.1623.4.0_x64__pzs8sxrjxfjjc\app\claude.exe"
        )));
        assert!(win::is_desktop_image(OsStr::new(
            r"C:\Users\É Ü\AppData\Local\AnthropicClaude\app-0.9.0\claude.exe"
        )));
        assert!(!win::is_desktop_image(OsStr::new(
            r"C:\Users\A\AppData\Roaming\Claude\claude-code\2.1.308\c17a819afb40\claude.exe"
        )));
        assert!(!win::is_desktop_image(OsStr::new(
            r"C:\Program Files\WindowsApps\Claude_3.1623.4.0_x64__pzs8sxrjxfjjc\app\chrome-native-host.exe"
        )));
    }

    /// Read-only: enumerates windows and queries process paths.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs a running Claude Desktop"]
    fn live_finds_running_desktop() {
        let windows = win::desktop_windows();
        assert!(!windows.is_empty(), "no Claude Desktop window found");
        assert!(
            windows.iter().any(|w| w.main_candidate),
            "no visible main window among {} Desktop windows",
            windows.len()
        );
    }
}
