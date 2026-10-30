//! Tray icon: a colored dot summarizing every session, a tooltip and the
//! settings menu, both in the language in use.

use std::sync::Mutex;

use monitor_core::i18n::{self, Lang};
use monitor_core::model::{Counts, Quota, QuotaWindow, Settings};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
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
pub const MENU_AUTO_UPDATE: &str = "auto_update";
pub const MENU_CHECK_UPDATE: &str = "check_update";
const MENU_LANGUAGE: &str = "language";
pub const MENU_LANGUAGE_AUTO: &str = "language_auto";
pub const MENU_LANGUAGE_EN: &str = "language_en";
pub const MENU_LANGUAGE_FR: &str = "language_fr";
pub const MENU_QUIT: &str = "quit";

const AMBER: [u8; 3] = [0xF5, 0x9E, 0x0B];
const RED: [u8; 3] = [0xEF, 0x44, 0x44];
const BLUE: [u8; 3] = [0x3B, 0x82, 0xF6];
const GREEN: [u8; 3] = [0x22, 0xC5, 0x5E];
const GRAY: [u8; 3] = [0x9C, 0xA3, 0xAF];

/// Handles kept to update the tray after it is built.
pub struct TrayHandles {
    toggle: MenuItem<Wry>,
    compact: CheckMenuItem<Wry>,
    always_on_top: CheckMenuItem<Wry>,
    notify_waiting: CheckMenuItem<Wry>,
    notify_finished: CheckMenuItem<Wry>,
    notify_error: CheckMenuItem<Wry>,
    notify_quota: CheckMenuItem<Wry>,
    include_cli: CheckMenuItem<Wry>,
    autostart: CheckMenuItem<Wry>,
    auto_update: CheckMenuItem<Wry>,
    check_update: MenuItem<Wry>,
    language: Submenu<Wry>,
    language_auto: CheckMenuItem<Wry>,
    language_en: CheckMenuItem<Wry>,
    language_fr: CheckMenuItem<Wry>,
    quit: MenuItem<Wry>,
    /// Color and tooltip currently shown, to skip redundant updates.
    shown: Mutex<Option<([u8; 3], String)>>,
}

/// Builds the tray icon and its menu, and registers [`TrayHandles`].
/// `autostart`: whether the app starts with Windows now; None when that
/// cannot be offered (the item is greyed out). `updates`: whether this copy
/// can update itself; the update items are greyed out otherwise.
pub fn create(
    app: &AppHandle,
    settings: &Settings,
    autostart: Option<bool>,
    updates: bool,
    on_menu: impl Fn(&AppHandle, MenuEvent) + Send + Sync + 'static,
    on_left_click: impl Fn(&AppHandle) + Send + Sync + 'static,
) -> tauri::Result<TrayIcon> {
    let toggle = MenuItem::with_id(app, MENU_TOGGLE, label(MENU_TOGGLE), true, None::<&str>)?;
    let compact = check(app, MENU_COMPACT, settings.compact)?;
    let always_on_top = check(app, MENU_ALWAYS_ON_TOP, settings.always_on_top)?;
    let notify_waiting = check(app, MENU_NOTIFY_WAITING, settings.notify_on_waiting)?;
    let notify_finished = check(app, MENU_NOTIFY_FINISHED, settings.notify_on_finished)?;
    let notify_error = check(app, MENU_NOTIFY_ERROR, settings.notify_on_error)?;
    let notify_quota = check(app, MENU_NOTIFY_QUOTA, settings.notify_on_quota)?;
    let include_cli = check(app, MENU_INCLUDE_CLI, settings.include_cli_sessions)?;
    let autostart = CheckMenuItem::with_id(
        app,
        MENU_AUTOSTART,
        label(MENU_AUTOSTART),
        autostart.is_some(),
        autostart.unwrap_or(false),
        None::<&str>,
    )?;
    let auto_update = CheckMenuItem::with_id(
        app,
        MENU_AUTO_UPDATE,
        label(MENU_AUTO_UPDATE),
        updates,
        settings.auto_update,
        None::<&str>,
    )?;
    let check_update = MenuItem::with_id(
        app,
        MENU_CHECK_UPDATE,
        label(MENU_CHECK_UPDATE),
        updates,
        None::<&str>,
    )?;
    let (auto, en, fr) = language_marks(settings);
    let language_auto = check(app, MENU_LANGUAGE_AUTO, auto)?;
    let language_en = check(app, MENU_LANGUAGE_EN, en)?;
    let language_fr = check(app, MENU_LANGUAGE_FR, fr)?;
    let language = Submenu::with_id_and_items(
        app,
        MENU_LANGUAGE,
        label(MENU_LANGUAGE),
        true,
        &[&language_auto, &language_en, &language_fr],
    )?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, MENU_QUIT, label(MENU_QUIT), true, None::<&str>)?;
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
            &auto_update,
            &check_update,
            &language,
            &separator,
            &quit,
        ],
    )?;

    let empty = Counts::default();
    let color = dot_color(&empty);
    let tooltip_text = tooltip(&empty, None, i18n::current());
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
        toggle,
        compact,
        always_on_top,
        notify_waiting,
        notify_finished,
        notify_error,
        notify_quota,
        include_cli,
        autostart,
        auto_update,
        check_update,
        language,
        language_auto,
        language_en,
        language_fr,
        quit,
        shown: Mutex::new(Some((color, tooltip_text))),
    });
    Ok(tray)
}

/// Shows or hides the icon.
pub fn set_visible(app: &AppHandle, visible: bool) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_visible(visible);
    }
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

fn check(app: &AppHandle, id: &str, checked: bool) -> tauri::Result<CheckMenuItem<Wry>> {
    CheckMenuItem::with_id(app, id, label(id), true, checked, None::<&str>)
}

/// Text of a menu item, in the language in use. A language keeps its own name.
fn label(id: &str) -> &'static str {
    let (en, fr) = match id {
        MENU_TOGGLE => ("Show / hide", "Afficher / masquer"),
        MENU_COMPACT => ("Compact mode", "Mode compact"),
        MENU_ALWAYS_ON_TOP => ("Always on top", "Toujours au premier plan"),
        MENU_NOTIFY_WAITING => ("Notify when a session is waiting", "Notifier quand une session attend"),
        MENU_NOTIFY_FINISHED => ("Notify when a session finishes", "Notifier quand une session termine"),
        MENU_NOTIFY_ERROR => ("Notify errors", "Notifier les erreurs"),
        MENU_NOTIFY_QUOTA => (
            "Notify the quota (5 h: 80%, 95%; week: 90%)",
            "Notifier le quota (5 h : 80 %, 95 % ; semaine : 90 %)",
        ),
        MENU_INCLUDE_CLI => ("Include sessions outside Desktop", "Inclure les sessions hors Desktop"),
        MENU_AUTOSTART => ("Start with Windows", "Lancer au démarrage de Windows"),
        MENU_AUTO_UPDATE => ("Update automatically", "Mettre à jour automatiquement"),
        MENU_CHECK_UPDATE => ("Check for updates", "Rechercher une mise à jour"),
        MENU_LANGUAGE => ("Language", "Langue"),
        MENU_LANGUAGE_AUTO => ("Same as Windows", "Comme Windows"),
        MENU_LANGUAGE_EN => ("English", "English"),
        MENU_LANGUAGE_FR => ("Français", "Français"),
        MENU_QUIT => ("Quit", "Quitter"),
        _ => ("", ""),
    };
    i18n::current().pick(en, fr)
}

/// Check marks of the language items: automatic, English, French. An unknown
/// value follows Windows, as [`i18n::resolve`] does.
fn language_marks(settings: &Settings) -> (bool, bool, bool) {
    let (en, fr) = (settings.language == "en", settings.language == "fr");
    (!en && !fr, en, fr)
}

/// Sets the menu's text to the language in use. The tooltip follows with the
/// next snapshot, which brings the language to the window too.
pub fn relabel(app: &AppHandle) {
    let Some(handles) = app.try_state::<TrayHandles>() else {
        return;
    };
    let _ = handles.toggle.set_text(label(MENU_TOGGLE));
    let _ = handles.compact.set_text(label(MENU_COMPACT));
    let _ = handles.always_on_top.set_text(label(MENU_ALWAYS_ON_TOP));
    let _ = handles.notify_waiting.set_text(label(MENU_NOTIFY_WAITING));
    let _ = handles.notify_finished.set_text(label(MENU_NOTIFY_FINISHED));
    let _ = handles.notify_error.set_text(label(MENU_NOTIFY_ERROR));
    let _ = handles.notify_quota.set_text(label(MENU_NOTIFY_QUOTA));
    let _ = handles.include_cli.set_text(label(MENU_INCLUDE_CLI));
    let _ = handles.autostart.set_text(label(MENU_AUTOSTART));
    let _ = handles.auto_update.set_text(label(MENU_AUTO_UPDATE));
    let _ = handles.check_update.set_text(label(MENU_CHECK_UPDATE));
    let _ = handles.language.set_text(label(MENU_LANGUAGE));
    let _ = handles.language_auto.set_text(label(MENU_LANGUAGE_AUTO));
    let _ = handles.quit.set_text(label(MENU_QUIT));
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
    let _ = handles.auto_update.set_checked(settings.auto_update);
    let (auto, en, fr) = language_marks(settings);
    let _ = handles.language_auto.set_checked(auto);
    let _ = handles.language_en.set_checked(en);
    let _ = handles.language_fr.set_checked(fr);
}

/// Updates the icon color and the tooltip. Never blocks: the change is posted
/// to the main thread.
pub fn update(app: &AppHandle, counts: &Counts, quota: Option<&Quota>) {
    let color = dot_color(counts);
    let text = tooltip(counts, quota, i18n::current());
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

/// Summary, at most 127 characters: the sessions, then the quota on a second
/// line when Desktop recorded one.
pub fn tooltip(counts: &Counts, quota: Option<&Quota>, lang: Lang) -> String {
    let sessions = sessions_line(counts, lang);
    let text = match quota.and_then(|quota| quota_line(quota, lang)) {
        Some(quota) => format!("{sessions}\n{quota}"),
        None => sessions,
    };
    truncate_chars(&text, TOOLTIP_MAX_CHARS)
}

/// "Quota: 5 h 19% · 7 d 28%", the windows that reset since the latest sample
/// left out.
fn quota_line(quota: &Quota, lang: Lang) -> Option<String> {
    let part = |label: &str, window: Option<&QuotaWindow>| {
        window
            .filter(|w| !w.expired)
            .map(|w| format!("{label} {}", lang.percent(w.percent.round())))
    };
    let parts: Vec<String> = [
        part("5 h", quota.five_hour.as_ref()),
        part(lang.pick("7 d", "7 j"), quota.seven_day.as_ref()),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| {
        let parts = parts.join(" · ");
        lang.pick(format!("Quota: {parts}"), format!("Quota : {parts}"))
    })
}

fn sessions_line(counts: &Counts, lang: Lang) -> String {
    if counts.total == 0 {
        return lang.pick("No sessions", "Aucune session").to_owned();
    }
    let mut parts = Vec::new();
    if counts.waiting > 0 {
        let fr = if counts.waiting == 1 { "t'attend" } else { "t'attendent" };
        parts.push(format!("{} {}", counts.waiting, lang.pick("waiting", fr)));
    }
    if counts.error > 0 {
        let en = if counts.error == 1 { "error" } else { "errors" };
        parts.push(format!("{} {}", counts.error, lang.pick(en, "en erreur")));
    }
    if counts.working > 0 {
        parts.push(format!("{} {}", counts.working, lang.pick("working", "au travail")));
    }
    if counts.unseen > 0 {
        let fr = if counts.unseen == 1 { "non vue" } else { "non vues" };
        parts.push(format!("{} {}", counts.unseen, lang.pick("unseen", fr)));
    }
    if counts.pr_open > 0 {
        parts.push(format!("{} PR", counts.pr_open));
    }
    if parts.is_empty() {
        let (en, fr) = if counts.total == 1 {
            ("idle session", "session au repos")
        } else {
            ("idle sessions", "sessions au repos")
        };
        parts.push(format!("{} {}", counts.total, lang.pick(en, fr)));
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
    fn tooltip_speaks_english() {
        let en = |c: &Counts| tooltip(c, None, Lang::En);
        assert_eq!(en(&Counts::default()), "No sessions");
        let mut c = counts(1, 1, 2, 4, 3);
        c.pr_open = 2;
        assert_eq!(en(&c), "1 waiting · 1 error · 4 working · 2 unseen · 2 PR");
        assert_eq!(en(&counts(0, 2, 0, 0, 0)), "2 errors");
        assert_eq!(en(&counts(0, 0, 0, 0, 3)), "3 idle sessions");
        assert_eq!(en(&counts(0, 0, 0, 0, 1)), "1 idle session");
        let quota = Quota {
            sampled_at: 1,
            five_hour: window(19.4, false),
            seven_day: window(27.6, false),
            fed_by_sessions: false,
        };
        assert_eq!(
            tooltip(&counts(0, 0, 0, 1, 0), Some(&quota), Lang::En),
            "1 working\nQuota: 5 h 19% · 7 d 28%"
        );
    }

    #[test]
    fn every_menu_item_has_a_label_in_both_languages() {
        for id in [
            MENU_TOGGLE,
            MENU_COMPACT,
            MENU_ALWAYS_ON_TOP,
            MENU_NOTIFY_WAITING,
            MENU_NOTIFY_FINISHED,
            MENU_NOTIFY_ERROR,
            MENU_NOTIFY_QUOTA,
            MENU_INCLUDE_CLI,
            MENU_AUTOSTART,
            MENU_AUTO_UPDATE,
            MENU_CHECK_UPDATE,
            MENU_LANGUAGE,
            MENU_LANGUAGE_AUTO,
            MENU_LANGUAGE_EN,
            MENU_LANGUAGE_FR,
            MENU_QUIT,
        ] {
            assert!(!label(id).is_empty(), "{id}");
        }
    }

    #[test]
    fn the_language_marks_follow_the_setting() {
        let marks = |language: &str| {
            language_marks(&Settings {
                language: language.to_owned(),
                ..Settings::default()
            })
        };
        assert_eq!(marks("auto"), (true, false, false));
        assert_eq!(marks("en"), (false, true, false));
        assert_eq!(marks("fr"), (false, false, true));
        assert_eq!(marks("de"), (true, false, false));
    }

    #[test]
    fn tooltip_matches_examples() {
        assert_eq!(tooltip(&Counts::default(), None, Lang::Fr), "Aucune session");
        let mut c = counts(1, 0, 2, 4, 3);
        c.pr_open = 2;
        assert_eq!(tooltip(&c, None, Lang::Fr), "1 t'attend · 4 au travail · 2 non vues · 2 PR");
        assert_eq!(tooltip(&counts(2, 1, 1, 0, 0), None, Lang::Fr), "2 t'attendent · 1 en erreur · 1 non vue");
        assert_eq!(tooltip(&counts(0, 0, 0, 0, 3), None, Lang::Fr), "3 sessions au repos");
        assert_eq!(tooltip(&counts(0, 0, 0, 0, 1), None, Lang::Fr), "1 session au repos");
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
            fed_by_sessions: false,
        };
        let c = counts(0, 0, 0, 1, 0);
        assert_eq!(tooltip(&c, Some(&quota), Lang::Fr), "1 au travail\nQuota : 5 h 19 % · 7 j 28 %");
        // A window that reset since the latest sample is left out.
        quota.five_hour = window(82.0, true);
        assert_eq!(tooltip(&c, Some(&quota), Lang::Fr), "1 au travail\nQuota : 7 j 28 %");
        quota.seven_day = None;
        assert_eq!(tooltip(&c, Some(&quota), Lang::Fr), "1 au travail");
    }

    #[test]
    fn tooltip_is_capped() {
        let mut c = counts(4_000_000_000, 0, 0, 0, 0);
        c.total = u32::MAX;
        let long = truncate_chars(&"é".repeat(300), TOOLTIP_MAX_CHARS);
        assert_eq!(long.chars().count(), TOOLTIP_MAX_CHARS);
        assert!(tooltip(&c, None, Lang::Fr).chars().count() <= TOOLTIP_MAX_CHARS);
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
