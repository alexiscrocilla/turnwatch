# Security

## Reporting a vulnerability

Please do not open a public issue. Use **Report a vulnerability** in the repository's **Security** tab, which reaches the maintainer privately.

Include what is needed to reproduce the problem, and leave out real session content. This is a personal project maintained in spare time, so there is no guaranteed response time, but reports are taken seriously. Only the latest release gets fixes.

## What is in scope

The monitor reads sensitive material: session transcripts, which can hold anything pasted into a session, including secrets. What matters most is that this content stays on the machine and cannot run as code. The reports most useful to us are about:

- session content or a file on disk running code in the window, or reaching the commands it can call;
- anything sent off the machine, or written outside the places listed in the README;
- the installer or the companion mod doing more than the README says;
- a link opened by the app that it should not open.

## How the app is built

- Everything is read locally and read-only. The app makes no network call of its own: PR status goes through `gh`, with your own login.
- It reads no credential. Neither does the mod, which only writes what the engine already knows to `~/.claude/session-monitor`.
- The window renders session content as text, never as HTML, under a Content Security Policy that allows no remote content.
- Apart from Claude Desktop's own `claude://` session links, the app only opens `https://` links.

## Verifying a download

The installer is not signed, so Windows does not vouch for it. Each release lists its SHA-256 in `SHA256SUMS.txt`, and the installers are built by the [release workflow](.github/workflows/release.yml) from the tagged commit. To check a download:

```bash
sha256sum Claude-Session-Monitor_*_x64-setup.exe
```
