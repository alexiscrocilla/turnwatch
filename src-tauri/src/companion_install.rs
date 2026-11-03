//! Adds the companion mod to Claude Code and takes it away again. Only the
//! installer asks for it, with an argument of the exe: `--install-companion`
//! once its files are in place, `--uninstall-companion` before it removes
//! them (`installer-hooks.nsh`). Both run without a window and before the
//! rest of the app, so they stay out of its single instance.
//!
//! The mod travels next to the exe in `companion\`, a local marketplace that
//! Claude Code keeps reading the mod from. All the writing in `~\.claude` is
//! left to the `claude.exe` Claude Desktop runs its sessions with: those
//! plugin files are its own, and change with its versions.
//!
//! One copy of the mod is enough, as two would write the same files: an
//! install from another marketplace gives way once this one is in.
//!
//! macOS has no installer: the app adds the mod itself when it starts
//! ([`install_at_launch`]), and only `--uninstall-companion` is an argument
//! there. The marketplace is copied out of the bundle first, into the app's
//! data folder: the bundle may run from a quarantine copy (App
//! Translocation) or be moved, and Claude Code would keep its old path.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use monitor_core::paths;
use serde::Deserialize;

#[cfg(not(target_os = "macos"))]
const INSTALL_ARG: &str = "--install-companion";
const UNINSTALL_ARG: &str = "--uninstall-companion";
/// `name` in `mod\.claude-plugin\marketplace.json`.
const MARKETPLACE: &str = "turnwatch";
/// `name` in the mod's `plugin.json`.
const MOD_NAME: &str = "turnwatch-companion";
/// Folder of the marketplace next to the exe (in `Contents/Resources` on
/// macOS), from `bundle.resources` in `tauri.conf.json`.
const COMPANION_DIR: &str = "companion";
/// The CLI in each `<version>\<hash>` folder of Desktop's `claude-code`.
#[cfg(not(target_os = "macos"))]
const CLAUDE_EXE: &str = "claude.exe";
/// The CLI in each `<version>/<hash>` folder: an app bundle on macOS.
#[cfg(target_os = "macos")]
const CLAUDE_EXE: &str = "claude.app/Contents/MacOS/claude";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// One entry of `claude plugin list --json`.
#[derive(Debug, Deserialize)]
struct InstalledPlugin {
    id: String,
    scope: String,
}

/// Does what the installer started the exe for, if it did, and returns the
/// exit code. A failure prints its reason, which the installer shows.
pub fn run_from_args() -> Option<i32> {
    let arg = std::env::args_os().nth(1)?;
    let result = match arg.to_str()? {
        #[cfg(not(target_os = "macos"))]
        INSTALL_ARG => install(),
        UNINSTALL_ARG => uninstall(),
        _ => return None,
    };
    Some(match result {
        Ok(()) => 0,
        Err(reason) => {
            println!("{reason}");
            1
        }
    })
}

#[cfg(not(target_os = "macos"))]
fn install() -> Result<(), String> {
    let claude = desktop_claude()?;
    let marketplace = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name(COMPANION_DIR);
    add(&claude, &marketplace)
}

/// Adds the mod from the marketplace folder `marketplace`, or brings it up
/// to date there.
fn add(claude: &Path, marketplace: &Path) -> Result<(), String> {
    let plugin = plugin_id();

    // Each step does nothing when already done, so that installing again,
    // or over an older version, needs no other path.
    let add = [OsStr::new("plugin"), "marketplace".as_ref(), "add".as_ref(), marketplace.as_os_str()];
    run_claude(claude, &add)?;
    run_claude(claude, &["plugin", "marketplace", "update", MARKETPLACE])?;
    run_claude(claude, &["plugin", "install", plugin.as_str()])?;
    run_claude(claude, &["plugin", "update", plugin.as_str()])?;

    if let Ok(list) = run_claude(claude, &["plugin", "list", "--json"]) {
        for other in other_installs(&list) {
            // Failing leaves both copies, which only repeat the same writes.
            let _ = run_claude(claude, &["plugin", "uninstall", other.as_str()]);
        }
    }
    Ok(())
}

/// Adds the mod, or brings it up to date, unless Claude Code already lists
/// it and the copy of the marketplace in `data_dir` holds the mod bundled in
/// `resource_dir`. Returns whether it ran the install. Each step of the
/// install does nothing when already done, as with the Windows installer.
#[cfg(target_os = "macos")]
pub fn install_at_launch(resource_dir: &Path, data_dir: &Path) -> Result<bool, String> {
    let bundled = resource_dir.join(COMPANION_DIR);
    if !bundled.is_dir() {
        return Err(format!("{} is missing", bundled.display()));
    }
    let copy = data_dir.join(COMPANION_DIR);
    let claude = desktop_claude()?;
    let listed = run_claude(&claude, &["plugin", "list", "--json"])
        .is_ok_and(|list| is_installed(&list));
    if listed && holds_tree(&copy, &bundled) {
        return Ok(false);
    }
    if copy.exists() {
        fs::remove_dir_all(&copy).map_err(|e| e.to_string())?;
    }
    copy_tree(&bundled, &copy).map_err(|e| e.to_string())?;
    add(&claude, &copy)?;
    Ok(true)
}

/// Whether `list`, the output of `claude plugin list --json`, has the mod
/// from this marketplace for the user.
#[cfg(target_os = "macos")]
fn is_installed(list: &str) -> bool {
    let ours = plugin_id();
    serde_json::from_str::<Vec<InstalledPlugin>>(list)
        .unwrap_or_default()
        .iter()
        .any(|plugin| plugin.scope == "user" && plugin.id == ours)
}

/// Whether every file under `original` is under `copy` with the same bytes.
/// Files only in `copy`, which Claude Code may add, do not count.
#[cfg(target_os = "macos")]
fn holds_tree(copy: &Path, original: &Path) -> bool {
    let Ok(mut entries) = fs::read_dir(original) else {
        return false;
    };
    entries.all(|entry| {
        let Ok(entry) = entry else {
            return false;
        };
        let (from, to) = (entry.path(), copy.join(entry.file_name()));
        if from.is_dir() {
            holds_tree(&to, &from)
        } else {
            fs::read(&from).is_ok_and(|bytes| fs::read(&to).is_ok_and(|copied| copied == bytes))
        }
    })
}

#[cfg(target_os = "macos")]
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Takes the mod and its marketplace away. Failing here is no failure: the
/// uninstaller goes on either way, and a mod that was never installed fails.
fn uninstall() -> Result<(), String> {
    let claude = desktop_claude()?;
    let plugin = run_claude(&claude, &["plugin", "uninstall", plugin_id().as_str()]);
    let marketplace = run_claude(&claude, &["plugin", "marketplace", "remove", MARKETPLACE]);
    plugin.and(marketplace).map(|_| ())
}

fn plugin_id() -> String {
    format!("{MOD_NAME}@{MARKETPLACE}")
}

fn desktop_claude() -> Result<PathBuf, String> {
    let root = paths::desktop_data_dir().join("claude-code");
    newest_claude(&root).ok_or_else(|| format!("{CLAUDE_EXE} not found in {}", root.display()))
}

/// The `claude.exe` of the highest version under `root`, which Desktop lays
/// out as `<version>\<hash>\claude.exe` ([`CLAUDE_EXE`] on macOS).
fn newest_claude(root: &Path) -> Option<PathBuf> {
    let mut versions: Vec<(Vec<u32>, PathBuf)> = fs::read_dir(root)
        .ok()?
        .flatten()
        .filter_map(|entry| Some((version_numbers(entry.file_name().to_str()?)?, entry.path())))
        .collect();
    versions.sort();
    versions.into_iter().rev().find_map(|(_, dir)| {
        fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path().join(CLAUDE_EXE))
            .find(|exe| exe.is_file())
    })
}

/// The numbers of a folder named like `2.1.309`, which compare as versions.
fn version_numbers(name: &str) -> Option<Vec<u32>> {
    name.split('.').map(|part| part.parse().ok()).collect()
}

/// The user-scope installs of the mod from other marketplaces, in the output
/// of `claude plugin list --json`.
fn other_installs(list: &str) -> Vec<String> {
    let ours = plugin_id();
    serde_json::from_str::<Vec<InstalledPlugin>>(list)
        .unwrap_or_default()
        .into_iter()
        .filter(|plugin| {
            plugin.scope == "user"
                && plugin.id != ours
                && plugin.id.split('@').next() == Some(MOD_NAME)
        })
        .map(|plugin| plugin.id)
        .collect()
}

/// Runs `claude` and returns its output, or its error message.
fn run_claude<S: AsRef<OsStr>>(exe: &Path, args: &[S]) -> Result<String, String> {
    let mut command = Command::new(exe);
    command.args(args).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() {
        return Ok(stdout.into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = if stderr.trim().is_empty() { stdout } else { stderr };
    Err(message.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("turnwatch-tests")
            .join(format!("{name} é-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn add_exe(root: &Path, version: &str) -> PathBuf {
        let exe = root.join(version).join("9078b9c1d466").join(CLAUDE_EXE);
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"").unwrap();
        exe
    }

    #[test]
    fn the_highest_version_wins_as_a_number_not_as_text() {
        let root = temp_dir("newest");
        add_exe(&root, "2.1.99");
        let newest = add_exe(&root, "2.1.309");
        add_exe(&root, "2.1.308");
        assert_eq!(newest_claude(&root), Some(newest));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_version_without_its_exe_is_skipped() {
        let root = temp_dir("partial");
        let usable = add_exe(&root, "2.1.308");
        fs::create_dir_all(root.join("2.1.309").join("downloading")).unwrap();
        fs::create_dir_all(root.join("3.0.0")).unwrap();
        assert_eq!(newest_claude(&root), Some(usable));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn other_folders_are_no_versions() {
        let root = temp_dir("others");
        add_exe(&root, "latest");
        fs::write(root.join("2.1.309"), b"a file").unwrap();
        assert_eq!(newest_claude(&root), None);
        assert_eq!(newest_claude(&root.join("absent")), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn only_the_same_mod_from_another_marketplace_gives_way() {
        let list = r#"[
            { "id": "turnwatch-companion@mods-locaux", "version": "0.1.0", "scope": "user" },
            { "id": "turnwatch-companion@turnwatch", "version": "0.1.0", "scope": "user" },
            { "id": "turnwatch-companion@projet", "version": "0.1.0", "scope": "project" },
            { "id": "vue-scindee@mods-locaux", "version": "0.2.0", "scope": "user" }
        ]"#;
        assert_eq!(other_installs(list), vec!["turnwatch-companion@mods-locaux"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_this_marketplace_for_the_user_counts_as_installed() {
        let ours = r#"[{ "id": "turnwatch-companion@turnwatch", "version": "0.1.2", "scope": "user" }]"#;
        assert!(is_installed(ours));
        let others = r#"[
            { "id": "turnwatch-companion@mods-locaux", "version": "0.1.0", "scope": "user" },
            { "id": "turnwatch-companion@turnwatch", "version": "0.1.0", "scope": "project" }
        ]"#;
        assert!(!is_installed(others));
        assert!(!is_installed("Claude Code 9 prints something else"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_copy_is_replaced_only_when_a_bundled_file_differs() {
        let root = temp_dir("copy");
        let bundled = root.join("Resources");
        let data = root.join("data");
        fs::create_dir_all(bundled.join(COMPANION_DIR).join("mod").join("hooks")).unwrap();
        fs::write(bundled.join(COMPANION_DIR).join("marketplace.json"), b"{}").unwrap();
        fs::write(bundled.join(COMPANION_DIR).join("mod").join("hooks").join("a.ts"), b"1").unwrap();

        let copy = data.join(COMPANION_DIR);
        assert!(!holds_tree(&copy, &bundled.join(COMPANION_DIR)), "no copy yet");
        copy_tree(&bundled.join(COMPANION_DIR), &copy).unwrap();
        assert!(holds_tree(&copy, &bundled.join(COMPANION_DIR)));

        fs::write(copy.join("mod").join("types.d.ts"), b"added by Claude Code").unwrap();
        assert!(holds_tree(&copy, &bundled.join(COMPANION_DIR)), "extra files are fine");

        fs::write(bundled.join(COMPANION_DIR).join("mod").join("hooks").join("a.ts"), b"2").unwrap();
        assert!(!holds_tree(&copy, &bundled.join(COMPANION_DIR)), "a newer mod");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_list_removes_nothing() {
        assert!(other_installs("").is_empty());
        assert!(other_installs("Claude Code 9 prints something else").is_empty());
        assert!(other_installs(r#"{ "plugins": [] }"#).is_empty());
    }
}
