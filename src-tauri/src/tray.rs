//! Tray icon: a colored dot summarizing every session, a French tooltip and
//! the settings menu.

use std::sync::Mutex;

use monitor_core::model::{Counts, Quota, QuotaWindow, Settings};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

const TRAY_ID: &str = "main";
const ICON_SIZE: u32 = 32;
/// Windows truncates longer tooltips.
const TOOLTIP_MAX_CHARS: usize = 127;

pub const MENU_TOGGLE: &str = "toggle";
pub const MENU_COMPACT: &str = "compact";
pub const MENU_ALWAYS_ON_TOP: &str = "always_on_top";
pub const MENU_NOTIFY_WAITING: &str = "notify_on_waiting";
pub const MENU_NOTIFY_FINISHED: &str = "notify_on_finished";
pub const MENU_NOTIFY_ERROR: &str = "notify_on_error";
pub const MENU_NOTIFY_QUOTA: &str = "notify_on_quota";
pub const MENU_INCLUDE_CLI: &str = "include_cli_sessions";
/// Not a setting: the registry says whether the app starts with Windows.
pub const MENU_AUTOSTART: &str = "autostart";
pub const MENU_QUIT: &str = "quit";

const AMBER: [u8; 3] = [0xF5, 0x9E, 0x0B];
const RED: [u8; 3] = [0xEF, 0x44, 0x44];
const BLUE: [u8; 3] = [0x3B, 0x82, 0xF6];
const GREEN: [u8; 3] = [0x22, 0xC5, 0x5E];
const GRAY: [u8; 3] = [0x9C, 0xA3, 0xAF];

/// Handles kept to update the tray after it is built.
pub struct TrayHandles {
    compact: CheckMenuItem<Wry>,
    always_on_top: CheckMenuItem<Wry>,
    notify_waiting: CheckMenuItem<Wry>,
    notify_finished: CheckMenuItem<Wry>,
    notify_error: CheckMenuItem<Wry>,
    notify_quota: CheckMenuItem<Wry>,
    include_cli: CheckMenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
    /// Color and tooltip currently shown, to skip redundant updates.
    shown: Mutex<Option<([u8; 3], String)>>,
}

/// Builds the tray icon and its menu, and registers [`TrayHandles`].
/// `autostart`: whether the app starts with Windows now; None when that
/// cannot be offered (the item is greyed out).
pub fn create(
    app: &AppHandle,
    settings: &Settings,
    autostart: Option<bool>,
    on_menu: impl Fn(&AppHandle, MenuEvent) + Send + Sync + 'static,
    on_left_click: impl Fn(&AppHandle) + Send + Sync + 'static,
) -> tauri::Result<TrayIcon> {
    let toggle = MenuItem::with_id(app, MENU_TOGGLE, "Afficher / masquer", true, None::<&str>)?;
    let compact = check(app, MENU_COMPACT, "Mode compact", settings.compact)?;
    let always_on_top = check(
        app,
        MENU_ALWAYS_ON_TOP,
        "Toujours au premier plan",
        settings.always_on_top,
    )?;
    let notify_waiting = check(
        app,
        MENU_NOTIFY_WAITING,
        "Notifier quand une session attend",
        settings.notify_on_waiting,
    )?;
    let notify_finished = check(
        app,
        MENU_NOTIFY_FINISHED,
        "Notifier quand une session termine",
        settings.notify_on_finished,
    )?;
    let notify_error = check(
        app,
        MENU_NOTIFY_ERROR,
        "Notifier les erreurs",
        settings.notify_on_error,
    )?;
    let notify_quota = check(
        app,
        MENU_NOTIFY_QUOTA,
        "Notifier le quota (5 h : 80 %, 95 % ; semaine : 90 %)",
        settings.notify_on_quota,
    )?;
    let include_cli = check(
        app,
        MENU_INCLUDE_CLI,
        "Inclure les sessions hors Desktop",
        settings.include_cli_sessions,
    )?;
    let autostart = CheckMenuItem::with_id(
        app,
        MENU_AUTOSTART,
        "Lancer au démarrage de Windows",
        autostart.is_some(),
        autostart.unwrap_or(false),
        None::<&str>,
    )?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "Quitter", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &toggle,
            &compact,
            &always_on_top,
            &notify_waiting,
            &notify_finished,
            &notify_error,
            &notify_quota,
            &include_cli,
            &autostart,
            &separator,
            &quit,
        ],
    )?;

    let empty = Counts::default();
    let color = dot_color(&empty);
    let tooltip_text = tooltip(&empty, None);
    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(dot_icon(color))
        .tooltip(&tooltip_text)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu)
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                on_left_click(tray.app_handle());
            }
        })
        .build(app)?;

    app.manage(TrayHandles {
        compact,
        always_on_top,
        notify_waiting,
        notify_finished,
        notify_error,
        notify_quota,
        include_cli,
        autostart,
        shown: Mutex::new(Some((color, tooltip_text))),
    });
    Ok(tray)
}

/// Sets the "start with Windows" check mark to what the registry says.
pub fn sync_autostart(app: &AppHandle, enabled: bool) {
    if let Some(handles) = app.try_state::<TrayHandles>() {
        let _ = handles.autostart.set_checked(enabled);
    }
}

/// The "start with Windows" check mark. A click flips it before the menu
/// event arrives, so in the event it is what the user asked for.
pub fn autostart_checked(app: &AppHandle) -> Option<bool> {
    app.try_state::<TrayHandles>()?.autostart.is_checked().ok()
}

fn check(app: &AppHandle, id: &str, text: &str, checked: bool) -> tauri::Result<CheckMenuItem<Wry>> {
    CheckMenuItem::with_id(app, id, text, true, checked, None::<&str>)
}

/// Sets the menu check marks from the settings. Call off the main thread or on
/// it; never while the main thread waits on the caller.
pub fn sync_checks(app: &AppHandle, settings: &Settings) {
    let Some(handles) = app.try_state::<TrayHandles>() else {
        return;
    };
    let _ = handles.compact.set_checked(settings.compact);
    let _ = handles.always_on_top.set_checked(settings.always_on_top);
    let _ = handles.notify_waiting.set_checked(settings.notify_on_waiting);
    let _ = handles.notify_finished.set_checked(settings.notify_on_finished);
    let _ = handles.notify_error.set_checked(settings.notify_on_error);
    let _ = handles.notify_quota.set_checked(settings.notify_on_quota);
    let _ = handles.include_cli.set_checked(settings.include_cli_sessions);
}

/// Updates the icon color and the tooltip. Never blocks: the change is posted
/// to the main thread.
pub fn update(app: &AppHandle, counts: &Counts, quota: Option<&Quota>) {
    let color = dot_color(counts);
    let text = tooltip(counts, quota);
    {
        let Some(handles) = app.try_state::<TrayHandles>() else {
            return;
        };
        let mut shown = handles.shown.lock().unwrap_or_else(|e| e.into_inner());
        let unchanged = shown.as_ref().is_some_and(|(c, t)| *c == color && *t == text);
        if unchanged {
            return;
        }
        *shown = Some((color, text.clone()));
    }
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = handle.tray_by_id(TRAY_ID) {
            let _ = tray.set_icon(Some(dot_icon(color)));
            let _ = tray.set_tooltip(Some(text));
        }
    });
}

/// Icon color, by priority: waiting (permission, question, or a turn that
/// ended on a decision to make), error, unseen, working, else gray.
pub fn dot_color(counts: &Counts) -> [u8; 3] {
    if counts.waiting > 0 {
        AMBER
    } else if counts.error > 0 {
        RED
    } else if counts.unseen > 0 {
        BLUE
    } else if counts.working > 0 {
        GREEN
    } else {
        GRAY
    }
}

/// French summary, at most 127 characters: the sessions, then the quota on
/// a second line when Desktop recorded one.
pub fn tooltip(counts: &Counts, quota: Option<&Quota>) -> String {
    let sessions = sessions_line(counts);
    let text = match quota.and_then(quota_line) {
        Some(quota) => format!("{sessions}\n{quota}"),
        None => sessions,
    };
    truncate_chars(&text, TOOLTIP_MAX_CHARS)
}

/// "Quota : 5 h 19 % · 7 j 28 %", the windows that reset since the latest
/// sample left out.
fn quota_line(quota: &Quota) -> Option<String> {
    let part = |label: &str, window: Option<&QuotaWindow>| {
        window
            .filter(|w| !w.expired)
            .map(|w| format!("{label} {} %", w.percent.round()))
    };
    let parts: Vec<String> = [
        part("5 h", quota.five_hour.as_ref()),
        part("7 j", quota.seven_day.as_ref()),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| format!("Quota : {}", parts.join(" · ")))
}

fn sessions_line(counts: &Counts) -> String {
    if counts.total == 0 {
        return "Aucune session".to_owned();
    }
    let mut parts = Vec::new();
    if counts.waiting > 0 {
        let verb = if counts.waiting == 1 { "t'attend" } else { "t'attendent" };
        parts.push(format!("{} {verb}", counts.waiting));
    }
    if counts.error > 0 {
        parts.push(format!("{} en erreur", counts.error));
    }
    if counts.working > 0 {
        parts.push(format!("{} au travail", counts.working));
    }
    if counts.unseen > 0 {
        let label = if counts.unseen == 1 { "non vue" } else { "non vues" };
        parts.push(format!("{} {label}", counts.unseen));
    }
    if counts.pr_open > 0 {
        parts.push(format!("{} PR", counts.pr_open));
    }
    if parts.is_empty() {
        let label = if counts.total == 1 {
            "session au repos"
        } else {
            "sessions au repos"
        };
        parts.push(format!("{} {label}", counts.total));
    }
    parts.join(" · ")
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A filled, anti-aliased circle of `color` on a transparent 32x32 square.
pub fn dot_icon(color: [u8; 3]) -> Image<'static> {
    Image::new_owned(dot_rgba(color, ICON_SIZE), ICON_SIZE, ICON_SIZE)
}

fn dot_rgba(color: [u8; 3], size: u32) -> Vec<u8> {
    let center = size as f32 / 2.0;
    let radius = center - 2.0;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
            rgba.extend_from_slice(&color);
            rgba.push((coverage * 255.0).round() as u8);
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(waiting: u32, error: u32, unseen: u32, working: u32, idle: u32) -> Counts {
        Counts {
            total: waiting + error + unseen + working + idle,
            waiting,
            error,
            unseen,
            working,
            idle,
            ..Counts::default()
        }
    }

    #[test]
    fn color_follows_priority() {
        assert_eq!(dot_color(&counts(1, 1, 1, 1, 1)), AMBER);
        assert_eq!(dot_color(&counts(0, 1, 1, 1, 1)), RED);
        assert_eq!(dot_color(&counts(0, 0, 1, 1, 1)), BLUE);
        assert_eq!(dot_color(&counts(0, 0, 0, 1, 1)), GREEN);
        assert_eq!(dot_color(&counts(0, 0, 0, 0, 1)), GRAY);
        assert_eq!(dot_color(&Counts::default()), GRAY);
    }

    #[test]
    fn tooltip_matches_examples() {
        assert_eq!(tooltip(&Counts::default(), None), "Aucune session");
        let mut c = counts(1, 0, 2, 4, 3);
        c.pr_open = 2;
        assert_eq!(tooltip(&c, None), "1 t'attend · 4 au travail · 2 non vues · 2 PR");
        assert_eq!(tooltip(&counts(2, 1, 1, 0, 0), None), "2 t'attendent · 1 en erreur · 1 non vue");
        assert_eq!(tooltip(&counts(0, 0, 0, 0, 3), None), "3 sessions au repos");
        assert_eq!(tooltip(&counts(0, 0, 0, 0, 1), None), "1 session au repos");
    }

    fn window(percent: f64, expired: bool) -> Option<QuotaWindow> {
        Some(QuotaWindow {
            percent,
            expired,
            ..QuotaWindow::default()
        })
    }

    #[test]
    fn tooltip_adds_the_quota_on_a_second_line() {
        let mut quota = Quota {
            sampled_at: 1,
            five_hour: window(19.4, false),
            seven_day: window(27.6, false),
        };
        let c = counts(0, 0, 0, 1, 0);
        assert_eq!(tooltip(&c, Some(&quota)), "1 au travail\nQuota : 5 h 19 % · 7 j 28 %");
        // A window that reset since the latest sample is left out.
        quota.five_hour = window(82.0, true);
        assert_eq!(tooltip(&c, Some(&quota)), "1 au travail\nQuota : 7 j 28 %");
        quota.seven_day = None;
        assert_eq!(tooltip(&c, Some(&quota)), "1 au travail");
    }

    #[test]
    fn tooltip_is_capped() {
        let mut c = counts(4_000_000_000, 0, 0, 0, 0);
        c.total = u32::MAX;
        let long = truncate_chars(&"é".repeat(300), TOOLTIP_MAX_CHARS);
        assert_eq!(long.chars().count(), TOOLTIP_MAX_CHARS);
        assert!(tooltip(&c, None).chars().count() <= TOOLTIP_MAX_CHARS);
    }

    #[test]
    fn dot_is_round_and_smooth() {
        let rgba = dot_rgba(AMBER, ICON_SIZE);
        assert_eq!(rgba.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        let alpha = |x: u32, y: u32| rgba[((y * ICON_SIZE + x) * 4 + 3) as usize];
        assert_eq!(alpha(16, 16), 255);
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(31, 31), 0);
        let edge = (0..ICON_SIZE).map(|x| alpha(x, 16)).collect::<Vec<_>>();
        assert!(edge.iter().any(|a| *a > 0 && *a < 255), "edge is anti-aliased");
        assert_eq!(&rgba[..3], &AMBER);
    }
}
