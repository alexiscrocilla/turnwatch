# Contributing

Fixes and small improvements are welcome. For anything larger, open an issue first, so we can check it fits before you spend time on it.

## What the project is

The monitor answers one question: which of my sessions needs me now? Some choices follow from that, and changes that go against them are unlikely to be merged:

- **Current work only.** It shows what is in progress. Token counts, backlogs and lists of worktrees are out of scope.
- **Read-only.** The app reads Claude Code's and Claude Desktop's files and never writes in them. Only the companion mod writes, and only in `~/.claude/session-monitor`.
- **Local.** No telemetry, no network call of its own (`gh` is the one exception, for PR status), and no credential read, by the app or by the mod.
- **Quiet.** No continuous animation: it would keep WebView2 busy all day in a window that stays open. Anything that moves does so on an event.

## Getting started

You need Windows, stable Rust (MSVC), the Visual Studio Build Tools, Node.js and pnpm.

```bash
pnpm install
```

```bash
pnpm tauri dev
```

For interface work alone, `pnpm dev` serves it on http://localhost:1420 with fictional data, with no need for Claude Desktop. `?lang=en` or `?lang=fr` picks the language, and `?scenario=empty`, `?scenario=health` or `?scenario=paused` shows the other states.

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

The check leaves out the mod's tests, which need Claude Code: if you change `mod/`, run `claude plugin test mod/turnwatch-companion` with the `claude.exe` in `%APPDATA%\Claude\claude-code\<version>\<hash>\`. The `claude` on your PATH may be too old.

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

## Releasing

For the maintainer. Raise the version in `src-tauri/tauri.conf.json`, `package.json` and both `Cargo.toml` files, update `CHANGELOG.md`, push `main`, then:

```bash
pnpm release
```

It needs `gh` logged in. It builds from a fresh clone of `main` in `%LOCALAPPDATA%\turnwatch-release`, runs the checks there, pushes the tag and opens a draft release with the installer and its SHA-256. Nothing is public until you publish the draft on GitHub.

## Reporting bugs

Use the issue form, and leave out anything private. For security problems, follow [SECURITY.md](SECURITY.md) instead.

By contributing, you agree that your contribution is licensed under the [MIT License](LICENSE).
