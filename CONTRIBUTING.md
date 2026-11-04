# Contributing

Fixes and small improvements are welcome. For anything larger, open an issue first, so we can check it fits before you spend time on it.

## What the project is

The monitor answers one question: which of my sessions needs me now? Some choices follow from that, and changes that go against them are unlikely to be merged:

- **Current work only.** It shows what is in progress. Token counts, backlogs and lists of worktrees are out of scope.
- **Read-only.** The app reads Claude Code's and Claude Desktop's files and never writes in them. Only the companion mod writes, and only in `~/.claude/session-monitor`.
- **Local.** No telemetry, no network call of its own beyond the update check (and `gh`, for PR status), and no credential read, by the app or by the mod.
- **Quiet.** No continuous animation: it would keep the web view (WebView2, or WebKit on macOS) busy all day in a window that stays open. Anything that moves does so on an event.
- **Windows first, macOS too.** Code for one system goes behind `cfg(windows)` or `cfg(target_os = "macos")`; other systems only get stubs that compile. A change on one system must leave the other as it was.

## Getting started

You need Windows, stable Rust (MSVC), the Visual Studio Build Tools, Node.js and pnpm. On a Mac: stable Rust, the Xcode command line tools, Node.js and pnpm.

```bash
pnpm install
```

```bash
pnpm tauri dev
```

For interface work alone, `pnpm dev` serves it on http://localhost:1420 with fictional data, with no need for Claude Desktop. `?lang=en` or `?lang=fr` picks the language, `?wide` widens the window, and `?scenario=empty`, `?scenario=health`, `?scenario=paused` or `?scenario=ultra` (a plan with no 5-hour window) shows the other states.

To check what the engine reads on your machine, with no window:

```bash
cargo run --manifest-path crates/monitor-core/Cargo.toml --bin dump -- --summary
```

## Before you open a pull request

```bash
pnpm check
```

It builds the interface, runs clippy with warnings denied and runs both test suites. The project has no CI service, so nothing else runs these for you. To have them run before every push:

```bash
git config core.hooksPath .githooks
```

`git push --no-verify` skips the hook once.

The check leaves out the mod's tests, which need Claude Code: if you change `mod/`, run `claude plugin test mod/turnwatch-companion` with the `claude.exe` in `%APPDATA%\Claude\claude-code\<version>\<hash>\` (on macOS, `claude.app/Contents/MacOS/claude` in `~/Library/Application Support/Claude/claude-code/<version>/<hash>/`). The `claude` on your PATH may be too old.

A change to code shared by both systems needs `pnpm check` on both, as each compiles only its own `cfg` branches.

Changes to the interface need a look in both languages and in the narrow window: the window is 380 px wide.

## Conventions

- **Text the user sees exists in English and French.** In the window it lives in `src/i18n.ts`, whose French table has to match the English one or the build fails. In Rust it goes through `Lang::pick(english, french)`. Keep it short, and use "you" in English and "tu" in French.
- **Everything else is in English:** code, comments, commit messages and documentation.
- **The data contract is copied by hand.** A change to `crates/monitor-core/src/model.rs` has to be made in `src/types.ts` too.
- **Match the code around yours.** The code does not go through `rustfmt`, so please don't reformat files you are not changing.
- **Commits:** an imperative title that says what changes, and a body that says why, when the title is not enough.
- **Test data is fictional.** Never put real transcripts, session content, paths or project names in fixtures, and read `dump` output before you paste it anywhere.

## Testing the installer

The installer changes your real Claude Code configuration, and installing it replaces a running monitor. To test it safely, build a variant with its own name, and keep the build out of `src-tauri\target`, which a running monitor locks:

```bash
CARGO_TARGET_DIR=/some/scratch/folder pnpm tauri build --config '{"identifier":"local.turnwatch-test","productName":"Turnwatch Test","mainBinaryName":"turnwatch-test"}'
```

Then run the installer with `CLAUDE_CONFIG_DIR` pointing at a scratch folder. The variant has its own process name, registry values and application data, and its plugin configuration goes to that folder. To simulate a PC without Claude Code, set `APPDATA` to an empty folder for the installer: NSIS ignores it, but the app reads it.

Silent mode (`/S`) skips the pages that matter, namely the update page and the uninstaller's "Delete the application data" box, so go through the real windows too.

To test an update, build that variant twice, with a `version` of its own each time in `--config`, a throwaway key from `pnpm tauri signer generate`, and this, which replaces GitHub with a local server:

```json
{"plugins":{"updater":{"pubkey":"…","endpoints":["http://127.0.0.1:8765/latest.json"],"dangerousInsecureTransportProtocol":true}}}
```

Sign the newer installer with `pnpm tauri signer sign --app-version`, serve it with a `latest.json` like the one [`scripts/release.mjs`](scripts/release.mjs) writes, and install the older one: the app needs an `uninstall.exe` beside it to update itself. The app the installer restarts no longer has your `CLAUDE_CONFIG_DIR`, so stop it before offering it another version: that update's installer would add the mod to your real configuration.

## Testing on macOS

A release build adds the mod to your real Claude Code configuration as soon as it starts; a debug build (`pnpm tauri dev`) never does. Test it from the Finder, not from a terminal: only then does the app get launchd's short PATH, as your users' copy will. `CLAUDE_CONFIG_DIR` keeps the mod out of your configuration, as on Windows, but only when the app inherits it, so start it with `open --env CLAUDE_CONFIG_DIR=/some/scratch/folder src-tauri/target/release/bundle/macos/Turnwatch.app`.

To test an update, use the same throwaway key and local server as on Windows, with a `latest.json` whose `darwin-aarch64` entry points at a signed `Turnwatch.app.tar.gz`. Compress the bundle as [`scripts/release.mjs`](scripts/release.mjs) does, with `COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs -czf`: a plain `tar` can add AppleDouble `._` files, which would land in the updated bundle and break its signature. Run the older build from `~/Applications`: only a copy in an Applications folder updates itself.

## Releasing

For the maintainer. Raise the version in `src-tauri/tauri.conf.json`, `package.json` and both `Cargo.toml` files, update `CHANGELOG.md`, push `main`, then:

```bash
pnpm release
```

It needs `gh` logged in, the updater's private key in `~/.tauri/turnwatch-updater.key` with its `.pub` beside it, and the key's password in `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. It builds from a fresh clone of `main` in `%LOCALAPPDATA%\turnwatch-release`, runs the checks there, signs the installer, pushes the tag and opens a draft release with the installer, its SHA-256 and `latest.json`. Nothing is public until you publish the draft on GitHub, and publishing it updates every installed app within 6 hours: test the draft's installer first.

Each system builds its own files, so run `pnpm release` on Windows and on a Mac, in either order, before publishing. On the Mac it works in `~/Library/Caches/turnwatch-release`, and adds to the draft the disk image, the bundle the updater downloads (`Turnwatch_<version>_aarch64.app.tar.gz`), its signature in `latest.json` under `darwin-aarch64`, and their SHA-256. The second run finds the draft and merges its files with the first one's. The Mac needs the same key: a draft published with only one system's entry leaves the other system's apps without the update, and their "Check for updates" reports an error until the next release.

Keep a copy of the key and of its password somewhere safe, apart from each other. Installed apps only accept what the key signs: with a new one (`pnpm tauri signer generate`, then its public key in `plugins.updater.pubkey`), they stop updating, and each user has to run the next installer by hand once.

## Reporting bugs

Use the issue form, and leave out anything private. For security problems, follow [SECURITY.md](SECURITY.md) instead.

By contributing, you agree that your contribution is licensed under the [MIT License](LICENSE).
