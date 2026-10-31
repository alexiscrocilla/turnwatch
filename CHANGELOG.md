# Changelog

Notable changes to this project. Versions follow [Semantic Versioning](https://semver.org/).

## Unreleased

- The window keeps its width and expanded height from one start to the next, updates included.
- Changing a setting no longer expands a window dragged down to its bar.
- With `gh` not installed, the window no longer warns that PR status is paused: `gh` is optional.
- A session caught while its registry file was being rewritten no longer looks stopped for a moment, which could announce a turn that had not ended.
- A session already seen no longer comes back as unseen because Desktop was rewriting its file as the monitor started.
- A reminder can no longer be lost when the check for due reminders runs just as a session starts waiting.
- Error messages and tool summaries that existed in one language now follow the language in use.
- The companion mod leaves no file behind for a session that never starts a sub-agent.

## 0.2.0 (2026-10-31)

- The app updates itself from GitHub's releases, with signed installers, no window and no prompt. The tray menu turns it off or checks at once. It starts with this version: from 0.1.0, run its installer by hand once.
- Switching to or from compact mode no longer changes the width of a window that straddles two monitors with different scaling, and no longer nudges a window that sits against a screen edge.

## 0.1.0 (2026-10-30)

First version.

- Always-on-top window showing the live state of Claude Desktop's Claude Code sessions: waiting, error, unseen, working and idle, grouped by project.
- Compact mode, nested sessions, proposed tasks, PR status with Desktop's Auto-fix, and a pixel crab per session that shows its state and current tool.
- Reminders for sessions left waiting, and alerts when the plan's quota crosses a threshold.
- Companion mod for the exact quota and the running sub-agents.
- Windows installer that adds the mod to Claude Desktop, with no administrator rights.
- English and French, following Windows' display language or chosen from the tray menu.
