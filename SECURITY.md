# Security

## Reporting a vulnerability

Please do not open a public issue. Use **Report a vulnerability** in the repository's **Security** tab, which reaches the maintainer privately.

Include what is needed to reproduce the problem, and leave out real session content. This is a personal project maintained in spare time, so there is no guaranteed response time, but reports are taken seriously. Only the latest release gets fixes.

## What is in scope

The monitor reads sensitive material: session transcripts, which can hold anything pasted into a session, including secrets. What matters most is that this content stays on the machine and cannot run as code. The reports most useful to us are about:

- session content or a file on disk running code in the window, or reaching the commands it can call;
- anything sent off the machine, or written outside the places listed in the README;
- the installer or the companion mod doing more than the README says;
- an update installing anything other than a release signed by the maintainer;
- a link opened by the app that it should not open.

## How the app is built

- Everything is read locally and read-only. PR status goes through `gh`, with your own login. The app's only network call of its own is the update check, which sends nothing about you or your sessions.
- Updates are signed with a key that stays on the maintainer's computer. The app refuses an installer whose signature does not match the public key it was built with, or that was signed for another version than the one announced. The check runs in Rust: the window has no access to it.
- It reads no credential. Neither does the mod, which only writes what the engine already knows to `~/.claude/session-monitor`.
- The window renders session content as text, never as HTML, under a Content Security Policy that allows no remote content.
- Apart from Claude Desktop's own `claude://` session links, the app only opens `https://` links.

## Verifying a download

The installer has no Windows code signature, so Windows does not vouch for it. The update signature above only serves the app itself. Each release lists its SHA-256 in `SHA256SUMS.txt`, and [`scripts/release.mjs`](scripts/release.mjs) builds the installers on the maintainer's computer, from a fresh clone of the tagged commit. To check a download:

```bash
sha256sum Turnwatch_*_x64-setup.exe
```
