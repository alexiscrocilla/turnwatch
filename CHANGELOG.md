# Changelog

Notable changes to this project. Versions follow [Semantic Versioning](https://semver.org/).

## Unreleased

- Switching to or from compact mode no longer changes the width of a window that straddles two monitors with different scaling, and no longer nudges a window that sits against a screen edge.

## 0.1.0 (2026-10-30)

First version.

- Always-on-top window showing the live state of Claude Desktop's Claude Code sessions: waiting, error, unseen, working and idle, grouped by project.
- Compact mode, nested sessions, proposed tasks, PR status with Desktop's Auto-fix, and a pixel crab per session that shows its state and current tool.
- Reminders for sessions left waiting, and alerts when the plan's quota crosses a threshold.
- Companion mod for the exact quota and the running sub-agents.
- Windows installer that adds the mod to Claude Desktop, with no administrator rights.
- English and French, following Windows' display language or chosen from the tray menu.
