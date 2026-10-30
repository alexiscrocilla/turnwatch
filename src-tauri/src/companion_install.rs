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

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use monitor_core::paths;
use serde::Deserialize;

const INSTALL_ARG: &str = "--install-companion";
const UNINSTALL_ARG: &str = "--uninstall-companion";
/// `name` in `mod\.claude-plugin\marketplace.json`.
const MARKETPLACE: &str = "turnwatch";
/// `name` in the mod's `plugin.json`.
const MOD_NAME: &str = "turnwatch-companion";
/// Folder of the marketplace next to the exe, from `bundle.resources` in
/// `tauri.conf.json`.
const COMPANION_DIR: &str = "companion";
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
    let result = if arg == INSTALL_ARG {
        install()
    } else if arg == UNINSTALL_ARG {
        uninstall()
    } else {
        return None;
    };
    Some(match result {
        Ok(()) => 0,
        Err(reason) => {
            println!("{reason}");
            1
        }
    })
}

fn install() -> Result<(), String> {
    let claude = desktop_claude()?;
    let marketplace = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name(COMPANION_DIR);
    let plugin = plugin_id();

    // Each step does nothing when already done, so that installing again,
    // or over an older version, needs no other path.
    let add = [OsStr::new("plugin"), "marketplace".as_ref(), "add".as_ref(), marketplace.as_os_str()];
    run_claude(&claude, &add)?;
    run_claude(&claude, &["plugin", "marketplace", "update", MARKETPLACE])?;
    run_claude(&claude, &["plugin", "install", plugin.as_str()])?;
    run_claude(&claude, &["plugin", "update", plugin.as_str()])?;

    if let Ok(list) = run_claude(&claude, &["plugin", "list", "--json"]) {
        for other in other_installs(&list) {
            // Failing leaves both copies, which only repeat the same writes.
            let _ = run_claude(&claude, &["plugin", "uninstall", other.as_str()]);
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
    newest_claude(&root).ok_or_else(|| format!("claude.exe not found in {}", root.display()))
}

/// The `claude.exe` of the highest version under `root`, which Desktop lays
/// out as `<version>\<hash>\claude.exe`.
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
            .map(|entry| entry.path().join("claude.exe"))
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
        let dir = root.join(version).join("9078b9c1d466");
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("claude.exe");
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

    #[test]
    fn an_unreadable_list_removes_nothing() {
        assert!(other_installs("").is_empty());
        assert!(other_installs("Claude Code 9 prints something else").is_empty());
        assert!(other_installs(r#"{ "plugins": [] }"#).is_empty());
    }
}
