//! Updates from GitHub. The latest published release's `latest.json` (see
//! `scripts/release.mjs`) names the new version and its installer, and the
//! plugin refuses an installer whose signature does not match the public key
//! in `tauri.conf.json`. The installer then runs with `/S /UPDATE`: no window,
//! no prompt, the old uninstaller stays out of it, and it starts the app again.
//!
//! On macOS the update is the app bundle itself, in a `.tar.gz`: the plugin
//! replaces the bundle in place and the app restarts on its own.
//!
//! This is the app's only network call of its own. It all runs from Rust: the
//! window's Content Security Policy stays closed, and its capabilities give it
//! no access to the plugin.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use monitor_core::i18n;
use monitor_core::model::Settings;
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Error, UpdaterExt};

use crate::toasts::{self, Toast};
use crate::tray;

/// Lets the app settle before the first check.
const FIRST_CHECK: Duration = Duration::from_secs(30);
/// For an app that stays open for days.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// Limit of each request, the download included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Left in the config folder by an update that found the window hidden.
const STAY_HIDDEN: &str = "update-stay-hidden";

/// One check at a time: the timer and the tray menu must not both install.
static CHECKING: Mutex<()> = Mutex::new(());

/// Only an installed app updates itself: an exe run from `target` would hand
/// over to the installed one.
pub fn available() -> bool {
    std::env::current_exe().is_ok_and(|exe| installed(&exe))
}

/// The installer leaves its uninstaller beside the exe.
#[cfg(not(target_os = "macos"))]
fn installed(exe: &Path) -> bool {
    exe.with_file_name("uninstall.exe").is_file()
}

/// macOS: the bundle lies in an Applications folder, the system's or the
/// user's. Run from the build folder, a disk image or a quarantine copy
/// (App Translocation), it would replace itself where it stands.
#[cfg(target_os = "macos")]
fn installed(exe: &Path) -> bool {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    in_applications(exe, home.as_deref().map(Path::new))
}

/// Whether `exe` is the executable of a bundle under `/Applications` or
/// `<home>/Applications`.
#[cfg(target_os = "macos")]
fn in_applications(exe: &Path, home: Option<&Path>) -> bool {
    let Some(bundle) = bundle_of(exe) else {
        return false;
    };
    bundle.starts_with("/Applications")
        || home.is_some_and(|home| bundle.starts_with(home.join("Applications")))
}

/// `X.app` for an executable at `X.app/Contents/MacOS/<name>`.
#[cfg(target_os = "macos")]
fn bundle_of(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let laid_out = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app";
    laid_out.then_some(bundle)
}

/// Checks soon after the start, then every [`CHECK_EVERY`], while the setting
/// is on. A failed check says nothing: the next one tries again.
pub fn start(app: AppHandle, settings: Arc<Mutex<Settings>>) {
    if !available() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("updater".into())
        .spawn(move || {
            std::thread::sleep(FIRST_CHECK);
            loop {
                if crate::lock(&settings).auto_update {
                    let _ = update(&app);
                }
                std::thread::sleep(CHECK_EVERY);
            }
        });
    // Without it, updates still come from the tray menu.
    drop(spawned);
}

/// The tray menu's check. Says how it went, unless an update ends the app.
pub fn check_now(app: &AppHandle) {
    let lang = i18n::current();
    let version = &app.package_info().version;
    let body = match update(app) {
        Ok(()) => lang.pick(
            format!("Version {version} is the latest one."),
            format!("La version {version} est la dernière."),
        ),
        Err(error) => lang.pick(
            format!("Could not check for updates: {error}"),
            format!("Impossible de vérifier les mises à jour : {error}"),
        ),
    };
    show(app, body);
}

/// Installs the latest release when it is newer. Starting the installer ends
/// the process, so what comes back is that there was nothing to install, or
/// an error.
fn update(app: &AppHandle) -> Result<(), Error> {
    let _checking = crate::lock(&CHECKING);
    tauri::async_runtime::block_on(async {
        let handle = app.clone();
        let updater = app
            .updater_builder()
            .timeout(REQUEST_TIMEOUT)
            .on_before_exit(move || before_exit(&handle))
            .build()?;
        let Some(update) = updater.check().await? else {
            return Ok(());
        };
        let installer = update.download(|_, _| {}, || {}).await?;
        show(app, announcement(app, &update.version));
        let refused = update.install(installer);
        // macOS: the new bundle is in place, and the app must start it.
        #[cfg(target_os = "macos")]
        if refused.is_ok() {
            before_exit(app);
            app.restart();
        }
        // Still here: Windows did not start the installer, the app goes on.
        take_stay_hidden(app);
        tray::set_visible(app, true);
        refused
    })
}

/// The plugin ends the process without a `RunEvent::Exit`. The tray icon is
/// only hidden: gone for good, it could not come back if the installer fails
/// to start, and left as it is, it would stay on screen after the process.
fn before_exit(app: &AppHandle) {
    let hidden = app
        .get_webview_window(crate::MAIN_WINDOW)
        .is_some_and(|window| !window.is_visible().unwrap_or(true));
    if hidden && let Some(marker) = stay_hidden_file(app) {
        let _ = fs::write(marker, "");
    }
    crate::save_state(app);
    tray::set_visible(app, false);
}

fn stay_hidden_file(app: &AppHandle) -> Option<PathBuf> {
    Some(app.path().app_config_dir().ok()?.join(STAY_HIDDEN))
}

/// Whether the update that restarted the app found the window hidden. Says so
/// once.
pub fn take_stay_hidden(app: &AppHandle) -> bool {
    stay_hidden_file(app).is_some_and(|marker| fs::remove_file(marker).is_ok())
}

fn announcement(app: &AppHandle, version: &str) -> String {
    let name = &app.package_info().name;
    i18n::current().pick(
        format!("{name} {version} is installing and restarts in a moment."),
        format!("{name} {version} s'installe et redémarre dans un instant."),
    )
}

fn show(app: &AppHandle, body: String) {
    crate::show_toast(
        app,
        &Toast {
            key: format!("{}update", toasts::MONITOR_PREFIX),
            title: i18n::current().pick("Update", "Mise à jour").to_owned(),
            body,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn only_a_bundle_in_an_applications_folder_is_installed() {
        let home = Path::new("/Users/someone");
        let at = |path: &str| in_applications(Path::new(path), Some(home));
        assert!(at("/Applications/Turnwatch.app/Contents/MacOS/turnwatch"));
        assert!(at("/Applications/Tools/Turnwatch.app/Contents/MacOS/turnwatch"));
        assert!(at("/Users/someone/Applications/Turnwatch.app/Contents/MacOS/turnwatch"));
        assert!(!at("/Users/someone/src/turnwatch/target/release/bundle/macos/Turnwatch.app/Contents/MacOS/turnwatch"));
        assert!(!at("/Volumes/Turnwatch/Turnwatch.app/Contents/MacOS/turnwatch"));
        assert!(!at("/private/var/folders/xy/T/AppTranslocation/0A1B/d/Turnwatch.app/Contents/MacOS/turnwatch"));
        assert!(!at("/Applications/turnwatch"), "not a bundle");
        assert!(!at("/Applications/Turnwatch.app/Contents/Resources/turnwatch"));
        assert!(!in_applications(Path::new("/Users/someone/Applications/T.app/Contents/MacOS/t"), None));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn only_an_exe_beside_its_uninstaller_is_installed() {
        let dir = std::env::temp_dir()
            .join("turnwatch-tests")
            .join(format!("installed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("turnwatch.exe");
        assert!(!installed(&exe), "as built, in target");
        fs::write(dir.join("uninstall.exe"), "").unwrap();
        assert!(installed(&exe), "as installed");
        let _ = fs::remove_dir_all(&dir);
    }
}
