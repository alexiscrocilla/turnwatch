//! Tauri shell of Claude Session Monitor: wires the monitor-core engine to the
//! window, the tray icon and Windows toasts.

mod app_identity;
mod autostart;
pub mod companion_install;
mod desktop_link;
mod quota_alerts;
mod registry;
mod settings_store;
mod toasts;
mod tray;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use monitor_core::engine::{Engine, SnapshotSink};
use monitor_core::i18n;
use monitor_core::model::{Settings, Snapshot};
use monitor_core::paths::now_ms;
use tauri::menu::MenuEvent;
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, RunEvent, State, Url,
    WebviewWindow, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

/// Label of the only window, as in `tauri.conf.json`.
const MAIN_WINDOW: &str = "main";
/// Event names shared with `src/types.ts`.
const EVENT_SNAPSHOT: &str = "snapshot";
const EVENT_SETTINGS: &str = "settings";
/// Payload: true when the window was shown, false when hidden to the tray.
const EVENT_VISIBILITY: &str = "visibility";
/// Window height in compact mode, in logical pixels.
const COMPACT_HEIGHT: f64 = 44.0;
/// Height restored when leaving compact mode with nothing remembered.
const DEFAULT_EXPANDED_HEIGHT: f64 = 560.0;
/// A window at most this tall (logical pixels) shows only the compact bar,
/// whatever the setting says.
const BAR_ONLY_HEIGHT: f64 = 72.0;
/// Shortest height remembered or restored for the expanded window.
const MIN_EXPANDED_HEIGHT: f64 = 200.0;
/// How often held-back waits are checked for a reminder toast.
const TOAST_TICK: Duration = Duration::from_secs(5);

/// Error of the commands called before the shell is ready.
fn starting() -> String {
    i18n::current()
        .pick("The app is still starting.", "L'application démarre encore.")
        .to_owned()
}

/// Title of the main window, which Windows shows in the taskbar.
fn window_title() -> &'static str {
    i18n::current().pick("Claude Sessions", "Sessions Claude")
}

/// State shared by the commands, the tray and the engine sink.
struct Shell {
    engine: Engine,
    /// Source of truth for the settings; the sink reads it for toasts.
    settings: Arc<Mutex<Settings>>,
    settings_path: Option<PathBuf>,
    /// Height of the window before it went compact.
    expanded_height: Mutex<Option<f64>>,
    /// Where the compact bar was before the window was last expanded.
    compact_return: Mutex<Option<CompactReturn>>,
    /// Serializes settings changes: each one reads, changes and writes the
    /// settings under it.
    apply_lock: Mutex<()>,
    feed: Arc<Feed>,
}

impl Shell {
    fn current_settings(&self) -> Settings {
        lock(&self.settings).clone()
    }
}

/// Locks a mutex, recovering the data if a panicking thread poisoned it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut context = tauri::generate_context!();
    if cfg!(debug_assertions) {
        use_dev_identity(context.config_mut());
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::POSITION)
                .build(),
        )
        .setup(|app| {
            setup(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != MAIN_WINDOW {
                return;
            }
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                    note_shown(window.app_handle(), false);
                }
                // A hidden window cannot get the focus: whatever showed it,
                // the list must follow again.
                WindowEvent::Focused(true) => note_shown(window.app_handle(), true),
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            get_settings,
            set_settings,
            open_session,
            ack_session,
            set_compact,
            set_always_on_top,
            open_url,
            hide_window,
            quit_app
        ])
        .build(context)
        .expect("error while building tauri application")
        .run(|app, event| {
            // Logoff, shutdown or Quitter: the process may end before the
            // engine's worker writes its latest acks.
            if let RunEvent::Exit = event {
                flush_engine(app);
            }
        });
}

/// Debug builds get their own identity: single-instance lock, settings and
/// window state, WebView2 profile and toast AUMID all derive from the
/// identifier. So `pnpm tauri dev` runs beside the release build instead of
/// handing over to it, and never touches its settings.
fn use_dev_identity(config: &mut tauri::Config) {
    config.identifier.push_str(".dev");
    let name = config
        .product_name
        .take()
        .unwrap_or_else(|| "Claude Session Monitor".to_owned());
    config.product_name = Some(format!("{name} (dev)"));
    for window in &mut config.app.windows {
        window.title.push_str(" (dev)");
    }
}

/// Loads the settings, builds the tray, applies the window settings,
/// registers the toast identity and starts the engine.
fn setup(app: &AppHandle) -> tauri::Result<()> {
    let config_dir = app.path().app_config_dir().ok();
    let settings_path = config_dir
        .as_ref()
        .map(|dir| dir.join(settings_store::FILE_NAME));
    let settings = settings_path
        .as_deref()
        .map(settings_store::load)
        .unwrap_or_default();
    i18n::set(i18n::resolve(&settings.language));

    autostart::repair(app);
    tray::create(
        app,
        &settings,
        autostart::AVAILABLE.then(|| autostart::is_enabled(app)),
        on_menu_event,
        toggle_main_window,
    )?;

    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.set_title(window_title());
        let _ = window.set_always_on_top(settings.always_on_top);
        // The restored position may be off screen: a monitor that is gone, or
        // a compact bar saved near the bottom edge.
        if settings.compact {
            let _ = resize_height(&window, COMPACT_HEIGHT, None);
        } else {
            let _ = keep_on_screen(&window);
        }
    }

    app.manage(ToastTargets::default());
    #[cfg(windows)]
    app.manage(LiveToasts::default());
    app.manage(app_identity::ToastIdentity(app_identity::register(app)));

    // The window starts visible (`tauri.conf.json`). Assuming so is the safe
    // side: a wrong "hidden" would freeze the list.
    let feed = Arc::new(Feed::new(true));
    let shared = Arc::new(Mutex::new(settings.clone()));
    let notifier = Arc::new(Mutex::new(toasts::Notifier::default()));
    let sink = snapshot_sink(
        app.clone(),
        Arc::clone(&shared),
        Arc::clone(&feed),
        Arc::clone(&notifier),
    );
    let engine = Engine::start_with_state(settings, config_dir, sink);
    start_toast_ticker(app.clone(), engine.clone(), Arc::clone(&shared), notifier);
    app.manage(Shell {
        engine,
        settings: shared,
        settings_path,
        expanded_height: Mutex::new(None),
        compact_return: Mutex::new(None),
        apply_lock: Mutex::new(()),
        feed,
    });

    // Clicks on the toasts shown by the plugin (fallback path).
    let handle = app.clone();
    let _ = app.notification().on_action(move |performed| {
        if performed.action_id() != "tap" {
            return;
        }
        let Some(id) = performed.notification().map(|n| n.id()) else {
            return;
        };
        if let Some(key) = toast_target(&handle, id) {
            on_toast_click(&handle, &key);
        }
    });
    Ok(())
}

/// What the webview is sent when the window is shown or hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedEvent {
    Visibility(bool),
    /// The latest snapshot.
    Snapshot,
}

/// Snapshot events for the webview, sent only while the window is shown so a
/// window hidden in the tray costs nothing. The lock also keeps the events in
/// order between the engine thread and the show/hide paths.
struct Feed {
    shown: Mutex<bool>,
}

impl Feed {
    fn new(shown: bool) -> Self {
        Self {
            shown: Mutex::new(shown),
        }
    }

    /// Runs `send` (emit a snapshot) when the window is shown.
    fn publish(&self, send: impl FnOnce()) {
        let shown = lock(&self.shown);
        if *shown {
            send();
        }
    }

    /// Records that the window was shown or hidden. On a change, tells the
    /// webview and, when shown, sends it the latest snapshot at once.
    fn set_shown(&self, shown: bool, mut send: impl FnMut(FeedEvent)) {
        let mut current = lock(&self.shown);
        if *current == shown {
            return;
        }
        *current = shown;
        send(FeedEvent::Visibility(shown));
        if shown {
            send(FeedEvent::Snapshot);
        }
    }
}

/// Tells the feed the window was shown or hidden.
fn note_shown(app: &AppHandle, shown: bool) {
    let Some(shell) = app.try_state::<Shell>() else {
        return;
    };
    shell.feed.set_shown(shown, |event| match event {
        FeedEvent::Visibility(visible) => {
            let _ = app.emit(EVENT_VISIBILITY, visible);
        }
        FeedEvent::Snapshot => {
            let _ = app.emit(EVENT_SNAPSHOT, for_ui(shell.engine.snapshot()));
        }
    });
}

/// The snapshot as the window gets it. Before the engine's first pass
/// (`ready` false) the sources have not been read yet, so they are not
/// reported as broken; a real startup error is kept.
fn for_ui(mut snapshot: Snapshot) -> Snapshot {
    snapshot.lang = i18n::current();
    if !snapshot.ready && snapshot.health.errors.is_empty() {
        snapshot.health.registry_ok = true;
        snapshot.health.desktop_ok = true;
    }
    snapshot
}

/// Session keys of the toasts shown by the plugin, by notification id.
#[derive(Default)]
struct ToastTargets(Mutex<Vec<(i32, String)>>);

/// Most toasts remembered for a click.
const TOAST_TARGETS_MAX: usize = 64;

/// Puts `value` under `key` in a short list, newest last: an older entry for
/// the same key is replaced and the oldest ones beyond `max` are dropped.
fn remember<K: PartialEq, V>(list: &mut Vec<(K, V)>, key: K, value: V, max: usize) {
    list.retain(|(known, _)| *known != key);
    list.push((key, value));
    if list.len() > max {
        let extra = list.len() - max;
        list.drain(..extra);
    }
}

fn remember_toast(app: &AppHandle, id: i32, key: &str) {
    if let Some(targets) = app.try_state::<ToastTargets>() {
        remember(&mut lock(&targets.0), id, key.to_owned(), TOAST_TARGETS_MAX);
    }
}

fn toast_target(app: &AppHandle, id: i32) -> Option<String> {
    let targets = app.try_state::<ToastTargets>()?;
    let targets = lock(&targets.0);
    targets
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(_, key)| key.clone())
}

/// Native toasts kept alive, newest last, so Windows can report their clicks
/// for as long as the app runs.
#[cfg(windows)]
#[derive(Default)]
struct LiveToasts(Mutex<Vec<(String, windows::UI::Notifications::ToastNotification)>>);

#[cfg(windows)]
const LIVE_TOASTS_MAX: usize = 32;

/// Shows a toast with the app's own identity when it is registered, else
/// through the notification plugin.
fn show_toast(app: &AppHandle, toast: &toasts::Toast) {
    #[cfg(windows)]
    if show_native_toast(app, toast) {
        return;
    }
    remember_toast(app, toasts::toast_id(&toast.key), &toast.key);
    toasts::show_with_plugin(app, toast);
}

/// A Desktop session's toast opens it through Desktop's link, even from the
/// Notification Center; a click this process hears also opens it (or shows
/// the monitor) and acks it.
#[cfg(windows)]
fn show_native_toast(app: &AppHandle, toast: &toasts::Toast) -> bool {
    let Some(app_id) = app
        .try_state::<app_identity::ToastIdentity>()
        .and_then(|identity| identity.0.clone())
    else {
        return false;
    };
    // A toast about several sessions opens none of them.
    let link = (!toast.is_summary())
        .then(|| desktop_link::session_link(&toast.key))
        .flatten();
    let handle = app.clone();
    let key = toast.key.clone();
    let shown = toasts::native::show(&app_id, toast, link.as_deref(), move || {
        on_toast_click(&handle, &key)
    });
    match shown {
        Ok(notification) => {
            if let Some(live) = app.try_state::<LiveToasts>() {
                remember(
                    &mut lock(&live.0),
                    toast.key.clone(),
                    notification,
                    LIVE_TOASTS_MAX,
                );
            }
            true
        }
        Err(_) => false,
    }
}

/// A toast was clicked: a toast about the monitor (several sessions, the
/// quota…) shows our window, one about a session opens it.
fn on_toast_click(app: &AppHandle, key: &str) {
    if toasts::shows_monitor(key) {
        show_main_window(app);
    } else {
        open_or_show(app, key);
    }
}

/// Opens a session in Claude Desktop from a toast click; shows our window when
/// that is not possible (a terminal session, Desktop closed…).
fn open_or_show(app: &AppHandle, key: &str) {
    if open_in_desktop(app, key).is_err() {
        show_main_window(app);
    }
}

/// Shows `key` in Claude Desktop. The engine is told first, and the session
/// is acknowledged only when Desktop was asked to show it.
fn open_in_desktop(app: &AppHandle, key: &str) -> Result<(), String> {
    let shell = app.try_state::<Shell>().ok_or_else(starting)?;
    let asked = desktop_link::open_session(app, key, || shell.engine.note_open(key))?;
    if asked {
        shell.engine.ack(key);
    }
    Ok(())
}

/// Called on the engine thread with each new snapshot. Never blocks on the
/// main thread: events and tray updates are posted, toasts are shown from a
/// worker thread. While the window is hidden only the tray and the toasts
/// follow the snapshots.
fn snapshot_sink(
    app: AppHandle,
    settings: Arc<Mutex<Settings>>,
    feed: Arc<Feed>,
    notifier: Arc<Mutex<toasts::Notifier>>,
) -> SnapshotSink {
    Box::new(move |snapshot, transitions| {
        feed.publish(|| {
            let _ = app.emit(EVENT_SNAPSHOT, snapshot);
        });
        tray::update(&app, &snapshot.counts, snapshot.quota.as_ref());

        let current = lock(&settings).clone();
        let selected = lock(&notifier).select(transitions, snapshot, &current, now_ms());
        show_toasts(&app, selected);
    })
}

/// Shows toasts from a worker thread.
fn show_toasts(app: &AppHandle, selected: Vec<toasts::Toast>) {
    if selected.is_empty() {
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        for toast in &selected {
            show_toast(&handle, toast);
        }
    });
}

/// Every [`TOAST_TICK`], announces the held-back waits and errors that have
/// lasted long enough unanswered (see `toasts`), and a stop of Desktop's
/// quota samples. Runs for the life of the app.
fn start_toast_ticker(
    app: AppHandle,
    engine: Engine,
    settings: Arc<Mutex<Settings>>,
    notifier: Arc<Mutex<toasts::Notifier>>,
) {
    let spawned = std::thread::Builder::new()
        .name("toast-ticker".into())
        .spawn(move || {
            loop {
                std::thread::sleep(TOAST_TICK);
                let snapshot = engine.snapshot();
                let current = lock(&settings).clone();
                let now = now_ms();
                let mut due = lock(&notifier).tick(&snapshot, &current, now);
                due.extend(lock(&notifier).quota_stop(&snapshot, &current, now, user_idle_ms()));
                show_toasts(&app, due);
            }
        });
    // Without it, held-back waits are still checked with each new snapshot.
    drop(spawned);
}

/// Time since the last keyboard or mouse input in this Windows session.
#[cfg(windows)]
fn user_idle_ms() -> Option<i64> {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a valid LASTINPUTINFO with its size set.
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    // Both are tick counts that wrap after 49.7 days.
    // SAFETY: no arguments.
    Some(i64::from(unsafe { GetTickCount() }.wrapping_sub(info.dwTime)))
}

#[cfg(not(windows))]
fn user_idle_ms() -> Option<i64> {
    None
}

/// Saves the engine's acks right away.
fn flush_engine(app: &AppHandle) {
    if let Some(shell) = app.try_state::<Shell>() {
        shell.engine.flush();
    }
}

/// Changes the settings with `change`, under the lock: re-reads the file so
/// edits made by hand meanwhile are kept, writes it, hands the result to the
/// engine, applies it to the window and the tray, and emits the `settings`
/// event.
fn update_settings(
    app: &AppHandle,
    change: impl FnOnce(&mut Settings),
) -> Result<Settings, String> {
    let shell = app.try_state::<Shell>().ok_or_else(starting)?;
    let _guard = lock(&shell.apply_lock);
    let old = shell.current_settings();
    let mut wanted = old.clone();
    change(&mut wanted);

    // A failed write only loses persistence; the change still applies.
    let new = match &shell.settings_path {
        Some(path) => settings_store::save_change(path, &old, &wanted).0,
        None => wanted,
    };
    *lock(&shell.settings) = new.clone();
    // Before the engine: its next snapshot takes the language to the window.
    i18n::set(i18n::resolve(&new.language));
    shell.engine.set_settings(new.clone());

    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        if new.language != old.language {
            let _ = window.set_title(window_title());
        }
        let _ = window.set_always_on_top(new.always_on_top);
        // A window dragged down to the bar is expanded too, even though the
        // setting already says "not compact".
        let bar_only = logical_height(&window).is_ok_and(|h| h <= BAR_ONLY_HEIGHT);
        if new.compact != old.compact || (!new.compact && bar_only) {
            apply_compact(&shell, &window, new.compact);
        }
    }
    tray::sync_checks(app, &new);
    if new.language != old.language {
        tray::relabel(app);
    }
    let _ = app.emit(EVENT_SETTINGS, &new);
    Ok(new)
}

/// Where the compact bar was before the window was expanded, and where the
/// expanded window was put (physical pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactReturn {
    bar: (i32, i32),
    expanded: (i32, i32),
}

/// Where the compact bar goes: back where it was before the window was
/// expanded when the window has not moved since, else where the window is.
fn bar_origin(current: (i32, i32), memo: Option<CompactReturn>) -> (i32, i32) {
    memo.filter(|memo| memo.expanded == current)
        .map_or(current, |memo| memo.bar)
}

/// Shrinks the window to its title bar, or restores the remembered height.
/// The width is kept, and the window stays inside its monitor's work area.
fn apply_compact(shell: &Shell, window: &WebviewWindow, compact: bool) {
    let position = window.outer_position().ok().map(|p| (p.x, p.y));
    if compact {
        if let Ok(height) = logical_height(window)
            && height >= MIN_EXPANDED_HEIGHT
        {
            *lock(&shell.expanded_height) = Some(height);
        }
        let memo = lock(&shell.compact_return).take();
        let origin = position.map(|current| bar_origin(current, memo));
        let _ = resize_height(window, COMPACT_HEIGHT, origin);
    } else {
        let height = expanded_height(lock(&shell.expanded_height).take());
        if let Ok(expanded) = resize_height(window, height, None) {
            *lock(&shell.compact_return) = position.map(|bar| CompactReturn { bar, expanded });
        }
    }
}

/// Height to restore when expanding: the remembered one, never shorter than
/// [`MIN_EXPANDED_HEIGHT`].
fn expanded_height(remembered: Option<f64>) -> f64 {
    remembered
        .unwrap_or(DEFAULT_EXPANDED_HEIGHT)
        .max(MIN_EXPANDED_HEIGHT)
}

fn logical_height(window: &WebviewWindow) -> tauri::Result<f64> {
    let scale = window.scale_factor()?;
    Ok(window.inner_size()?.to_logical::<f64>(scale).height)
}

/// A window or work-area rectangle, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// Top-left that keeps `window` inside `area`, moving it as little as
/// possible. A window larger than the area is aligned on the area's
/// top-left, so its title bar stays reachable.
fn clamp_into(window: Rect, area: Rect) -> (i32, i32) {
    fn axis(pos: i32, len: i32, start: i32, area_len: i32) -> i32 {
        if len >= area_len {
            start
        } else {
            pos.clamp(start, start + area_len - len)
        }
    }
    (
        axis(window.x, window.w, area.x, area.w),
        axis(window.y, window.h, area.y, area.h),
    )
}

fn outer_rect(window: &WebviewWindow) -> tauri::Result<Rect> {
    let position = window.outer_position()?;
    let size = window.outer_size()?;
    Ok(Rect {
        x: position.x,
        y: position.y,
        w: i32::try_from(size.width).unwrap_or(i32::MAX),
        h: i32::try_from(size.height).unwrap_or(i32::MAX),
    })
}

/// Work area (screen minus taskbar) of the monitor the window is on, or of
/// the nearest one.
fn work_area(window: &WebviewWindow) -> Option<Rect> {
    let monitor = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten())?;
    let area = monitor.work_area();
    Some(Rect {
        x: area.position.x,
        y: area.position.y,
        w: i32::try_from(area.size.width).unwrap_or(i32::MAX),
        h: i32::try_from(area.size.height).unwrap_or(i32::MAX),
    })
}

/// Sets the window's height (logical pixels), keeping its width, and puts its
/// top-left at `origin` (physical pixels; the current one when None), moved
/// as little as needed to stay inside the work area of the monitor it was
/// on. Returns the final top-left.
fn resize_height(
    window: &WebviewWindow,
    height: f64,
    origin: Option<(i32, i32)>,
) -> tauri::Result<(i32, i32)> {
    let scale = window.scale_factor()?;
    let inner = window.inner_size()?;
    let width = inner.to_logical::<f64>(scale).width;
    let before = outer_rect(window)?;
    let area = work_area(window);
    window.set_size(LogicalSize::new(width, height))?;

    let frame = before
        .h
        .saturating_sub(i32::try_from(inner.height).unwrap_or(i32::MAX))
        .max(0);
    let (x, y) = origin.unwrap_or((before.x, before.y));
    let target = Rect {
        x,
        y,
        w: before.w,
        h: (height * scale).round() as i32 + frame,
    };
    let (x, y) = area.map_or((x, y), |area| clamp_into(target, area));
    if (x, y) != (before.x, before.y) {
        window.set_position(PhysicalPosition::new(x, y))?;
    }
    Ok((x, y))
}

/// Moves the window back inside its monitor's work area if needed.
fn keep_on_screen(window: &WebviewWindow) -> tauri::Result<()> {
    let rect = outer_rect(window)?;
    let Some(area) = work_area(window) else {
        return Ok(());
    };
    let (x, y) = clamp_into(rect, area);
    if (x, y) != (rect.x, rect.y) {
        window.set_position(PhysicalPosition::new(x, y))?;
    }
    Ok(())
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.show();
        if window.is_minimized().unwrap_or(false) {
            let _ = window.unminimize();
        }
        let _ = window.set_focus();
    }
    note_shown(app, true);
}

fn hide_main_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        window.hide()?;
    }
    note_shown(app, false);
    Ok(())
}

/// Tray left click: hides the window when it is on screen, else shows it.
fn toggle_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        return;
    };
    let on_screen =
        window.is_visible().unwrap_or(false) && !window.is_minimized().unwrap_or(false);
    if on_screen {
        let _ = hide_main_window(app);
    } else {
        show_main_window(app);
    }
}

/// Tray menu clicks. Runs on the main thread, so settings changes move to a
/// worker thread: they call the engine and wait on main-thread window calls.
fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    let id = event.id().as_ref().to_owned();
    match id.as_str() {
        tray::MENU_TOGGLE => toggle_main_window(app),
        tray::MENU_QUIT => quit(app),
        tray::MENU_AUTOSTART => {
            let wanted = tray::autostart_checked(app);
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let wanted = wanted.unwrap_or_else(|| !autostart::is_enabled(&app));
                set_autostart(&app, wanted);
            });
        }
        _ => {
            let Some(toggle) = menu_toggle(&id) else {
                return;
            };
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let _ = update_settings(&app, toggle);
            });
        }
    }
}

/// Turns the start with Windows on or off, then shows what the registry
/// says (the menu has already flipped its check mark on its own).
fn set_autostart(app: &AppHandle, wanted: bool) {
    if let Err(error) = autostart::set(app, wanted) {
        let lang = i18n::current();
        let (en, fr) = if wanted { ("turn on", "activer") } else { ("turn off", "désactiver") };
        show_toast(
            app,
            &toasts::Toast {
                key: format!("{}autostart", toasts::MONITOR_PREFIX),
                title: lang.pick("Start with Windows", "Démarrage avec Windows").to_owned(),
                body: lang.pick(
                    format!("Could not {en} starting with Windows: {error}"),
                    format!("Impossible de {fr} le lancement au démarrage : {error}"),
                ),
            },
        );
    }
    tray::sync_autostart(app, autostart::is_enabled(app));
}

/// The settings change of a tray check item.
fn menu_toggle(id: &str) -> Option<fn(&mut Settings)> {
    let toggle: fn(&mut Settings) = match id {
        tray::MENU_COMPACT => |s| s.compact = !s.compact,
        tray::MENU_ALWAYS_ON_TOP => |s| s.always_on_top = !s.always_on_top,
        tray::MENU_NOTIFY_WAITING => |s| s.notify_on_waiting = !s.notify_on_waiting,
        tray::MENU_NOTIFY_FINISHED => |s| s.notify_on_finished = !s.notify_on_finished,
        tray::MENU_NOTIFY_ERROR => |s| s.notify_on_error = !s.notify_on_error,
        tray::MENU_NOTIFY_QUOTA => |s| s.notify_on_quota = !s.notify_on_quota,
        tray::MENU_INCLUDE_CLI => |s| s.include_cli_sessions = !s.include_cli_sessions,
        tray::MENU_LANGUAGE_AUTO => |s| s.language = "auto".to_owned(),
        tray::MENU_LANGUAGE_EN => |s| s.language = "en".to_owned(),
        tray::MENU_LANGUAGE_FR => |s| s.language = "fr".to_owned(),
        _ => return None,
    };
    Some(toggle)
}

fn quit(app: &AppHandle) {
    let _ = app.save_window_state(StateFlags::POSITION);
    flush_engine(app);
    app.exit(0);
}

/// Only `https://` URLs with a host are opened.
fn checked_https_url(raw: &str) -> Result<Url, String> {
    let lang = i18n::current();
    let url = Url::parse(raw.trim())
        .map_err(|_| lang.pick(format!("Invalid URL: {raw}"), format!("URL invalide : {raw}")))?;
    if url.scheme() != "https" || url.host_str().is_none_or(str::is_empty) {
        return Err(lang.pick(
            format!("Only https:// addresses are opened: {raw}"),
            format!("Seules les adresses https:// sont ouvertes : {raw}"),
        ));
    }
    Ok(url)
}

// Commands. All are async so they run off the main thread: they may wait on
// the engine, which must never wait on the main thread in turn.

/// Before the engine's first pass the snapshot has `ready` false.
#[tauri::command]
async fn get_snapshot(shell: State<'_, Shell>) -> Result<Snapshot, String> {
    Ok(for_ui(shell.engine.snapshot()))
}

#[tauri::command]
async fn get_settings(shell: State<'_, Shell>) -> Result<Settings, String> {
    Ok(shell.current_settings())
}

#[tauri::command]
async fn set_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    update_settings(&app, move |current| *current = settings)
}

/// Fails for a session outside Claude Desktop, without touching Desktop.
#[tauri::command]
async fn open_session(app: AppHandle, key: String) -> Result<(), String> {
    open_in_desktop(&app, &key)
}

#[tauri::command]
async fn ack_session(shell: State<'_, Shell>, key: String) -> Result<(), String> {
    shell.engine.ack(&key);
    Ok(())
}

#[tauri::command]
async fn set_compact(app: AppHandle, compact: bool) -> Result<Settings, String> {
    update_settings(&app, |settings| settings.compact = compact)
}

#[tauri::command]
async fn set_always_on_top(app: AppHandle, value: bool) -> Result<Settings, String> {
    update_settings(&app, |settings| settings.always_on_top = value)
}

#[tauri::command]
async fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    let url = checked_https_url(&url)?;
    app.opener()
        .open_url(url.as_str(), None::<&str>)
        .map_err(|e| {
            i18n::current().pick(
                format!("Could not open the link: {e}"),
                format!("Impossible d'ouvrir le lien : {e}"),
            )
        })
}

#[tauri::command]
async fn hide_window(app: AppHandle) -> Result<(), String> {
    hide_main_window(&app).map_err(|e| e.to_string())
}

#[tauri::command]
async fn quit_app(app: AppHandle) -> Result<(), String> {
    quit(&app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use monitor_core::model::SourceHealth;

    use super::*;

    #[test]
    #[cfg(windows)]
    #[ignore = "needs an interactive Windows session"]
    fn live_reads_the_idle_time() {
        let idle = user_idle_ms().expect("idle time");
        assert!((0..i64::from(u32::MAX)).contains(&idle), "{idle}");
    }

    #[test]
    fn only_https_urls_pass() {
        assert!(checked_https_url("https://github.com/o/r/pull/1").is_ok());
        assert!(checked_https_url("  https://github.com/o/r/pull/1 ").is_ok());
        assert!(checked_https_url("http://github.com").is_err());
        assert!(checked_https_url("file:///C:/Windows/System32/cmd.exe").is_err());
        assert!(checked_https_url("claude://code/new").is_err());
        assert!(checked_https_url("javascript:alert(1)").is_err());
        assert!(checked_https_url("https://").is_err());
        assert!(checked_https_url("not a url").is_err());
    }

    #[test]
    fn hidden_window_gets_no_snapshot_and_catches_up_when_shown() {
        let feed = Feed::new(true);
        let mut sent = 0;
        feed.publish(|| sent += 1);
        assert_eq!(sent, 1);

        let mut events = Vec::new();
        feed.set_shown(false, |e| events.push(e));
        assert_eq!(events, [FeedEvent::Visibility(false)]);
        feed.publish(|| sent += 1);
        feed.publish(|| sent += 1);
        assert_eq!(sent, 1, "nothing while hidden");

        events.clear();
        feed.set_shown(false, |e| events.push(e));
        assert!(events.is_empty(), "hiding twice says nothing");
        feed.set_shown(true, |e| events.push(e));
        assert_eq!(events, [FeedEvent::Visibility(true), FeedEvent::Snapshot]);
        events.clear();
        feed.set_shown(true, |e| events.push(e));
        assert!(events.is_empty(), "showing a shown window says nothing");
        feed.publish(|| sent += 1);
        assert_eq!(sent, 2);
    }

    #[test]
    fn sources_are_not_broken_before_the_first_pass() {
        let pending = for_ui(Snapshot::default());
        assert!(!pending.ready);
        assert!(pending.health.registry_ok && pending.health.desktop_ok);

        let failed_start = Snapshot {
            health: SourceHealth {
                errors: vec!["Impossible de démarrer le moteur".into()],
                ..SourceHealth::default()
            },
            ..Snapshot::default()
        };
        assert_eq!(for_ui(failed_start.clone()), failed_start, "real errors stay");

        let real = Snapshot {
            ready: true,
            ..Snapshot::default()
        };
        assert_eq!(for_ui(real.clone()), real, "after the first pass, as is");
    }

    #[test]
    fn short_lists_replace_and_drop_the_oldest() {
        let mut list = Vec::new();
        remember(&mut list, 1, "a", 2);
        remember(&mut list, 2, "b", 2);
        remember(&mut list, 1, "c", 2);
        assert_eq!(list, [(2, "b"), (1, "c")]);
        remember(&mut list, 3, "d", 2);
        assert_eq!(list, [(1, "c"), (3, "d")]);
    }

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1040,
    };

    #[test]
    fn windows_are_kept_inside_the_work_area() {
        let inside = Rect {
            x: 100,
            y: 100,
            w: 380,
            h: 560,
        };
        assert_eq!(clamp_into(inside, AREA), (100, 100));
        // A compact bar just above the taskbar, expanded downward.
        let below = Rect { y: 996, ..inside };
        assert_eq!(clamp_into(below, AREA), (100, 1040 - 560));
        let left = Rect { x: -300, ..inside };
        assert_eq!(clamp_into(left, AREA), (0, 100));
        let right = Rect { x: 1800, ..inside };
        assert_eq!(clamp_into(right, AREA), (1920 - 380, 100));
        let too_tall = Rect { h: 2000, ..inside };
        assert_eq!(clamp_into(too_tall, AREA), (100, 0));
        // A second monitor left of the primary one.
        let second = Rect {
            x: -2560,
            y: 0,
            w: 2560,
            h: 1400,
        };
        assert_eq!(clamp_into(Rect { x: -100, ..inside }, second), (-380, 100));
    }

    #[test]
    fn expanding_never_restores_a_bar_height() {
        assert_eq!(expanded_height(None), DEFAULT_EXPANDED_HEIGHT);
        assert_eq!(expanded_height(Some(44.0)), MIN_EXPANDED_HEIGHT);
        assert_eq!(expanded_height(Some(640.0)), 640.0);
    }

    #[test]
    fn the_compact_bar_goes_back_where_it_was() {
        let memo = CompactReturn {
            bar: (100, 996),
            expanded: (100, 480),
        };
        assert_eq!(bar_origin((100, 480), Some(memo)), (100, 996));
        assert_eq!(bar_origin((300, 200), Some(memo)), (300, 200), "moved since");
        assert_eq!(bar_origin((100, 480), None), (100, 480));
    }

    #[test]
    fn every_tray_check_item_toggles_its_setting() {
        let start = Settings::default();
        for (id, read) in [
            (tray::MENU_COMPACT, (|s: &Settings| s.compact) as fn(&Settings) -> bool),
            (tray::MENU_ALWAYS_ON_TOP, |s| s.always_on_top),
            (tray::MENU_NOTIFY_WAITING, |s| s.notify_on_waiting),
            (tray::MENU_NOTIFY_FINISHED, |s| s.notify_on_finished),
            (tray::MENU_NOTIFY_ERROR, |s| s.notify_on_error),
            (tray::MENU_NOTIFY_QUOTA, |s| s.notify_on_quota),
            (tray::MENU_INCLUDE_CLI, |s| s.include_cli_sessions),
        ] {
            let mut settings = start.clone();
            menu_toggle(id).expect(id)(&mut settings);
            assert_eq!(read(&settings), !read(&start), "{id}");
        }
        assert!(menu_toggle(tray::MENU_QUIT).is_none());
        // Kept in the registry, not in the settings.
        assert!(menu_toggle(tray::MENU_AUTOSTART).is_none());
    }

    #[test]
    fn dev_builds_have_their_own_identity() {
        let mut config: tauri::Config = serde_json::from_value(serde_json::json!({
            "identifier": "local.claude-session-monitor",
            "productName": "Claude Session Monitor",
            "app": { "windows": [{ "label": "main", "title": "Sessions Claude" }] }
        }))
        .unwrap();
        use_dev_identity(&mut config);
        assert_eq!(config.identifier, "local.claude-session-monitor.dev");
        assert_eq!(
            config.product_name.as_deref(),
            Some("Claude Session Monitor (dev)")
        );
        assert_eq!(config.app.windows[0].title, "Sessions Claude (dev)");
        assert!(app_identity::is_valid_app_id(&config.identifier));
    }
}
