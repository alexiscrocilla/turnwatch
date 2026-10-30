//! Prints the current snapshot as JSON. `--watch` keeps running and prints
//! each new snapshot and its transitions.

use std::io::{self, Write};
use std::process::ExitCode;
use std::thread;

use monitor_core::engine::Engine;
use monitor_core::model::{
    QuotaWindow, SessionState, SessionView, Settings, Snapshot, Transition,
};

const USAGE: &str = "\
Usage: dump [--summary] [--watch] [--all]
  (no option)    full snapshot as JSON
  --summary      one line per session
  --watch        stays on and prints each new snapshot and its transitions (Ctrl+C to quit)
  --all          includes the sessions started outside Claude Desktop";

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    summary: bool,
    watch: bool,
    all: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    for arg in args {
        match arg.as_str() {
            "--summary" => options.summary = true,
            "--watch" => options.watch = true,
            "--all" => options.all = true,
            other => return Err(format!("Unknown option: {other}")),
        }
    }
    Ok(options)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let settings = Settings {
        include_cli_sessions: options.all,
        ..Settings::default()
    };
    if options.watch {
        watch(settings)
    } else {
        once(&settings, options.summary)
    }
}

/// Prints one snapshot. Fails when the registry could not be read.
fn once(settings: &Settings, summary: bool) -> ExitCode {
    let snapshot = Engine::snapshot_once(settings);
    let text = if summary {
        summary_lines(&snapshot).join("\n")
    } else {
        match serde_json::to_string_pretty(&snapshot) {
            Ok(json) => json,
            Err(e) => {
                eprintln!("Could not serialize: {e}");
                return ExitCode::FAILURE;
            }
        }
    };
    let mut out = io::stdout().lock();
    if let Err(e) = writeln!(out, "{text}").and_then(|()| out.flush()) {
        eprintln!("Could not write: {e}");
        return ExitCode::FAILURE;
    }
    if !snapshot.health.registry_ok {
        for error in &snapshot.health.errors {
            eprintln!("{error}");
        }
        eprintln!("Unreadable session registry.");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Runs the engine and prints each snapshot it emits, until Ctrl+C.
fn watch(settings: Settings) -> ExitCode {
    let sink = Box::new(|snapshot: &Snapshot, transitions: &[Transition]| {
        let mut lines = vec![format!(
            "--- {} · n°{} · {} transition(s)",
            snapshot.generated_at,
            snapshot.seq,
            transitions.len()
        )];
        lines.extend(summary_lines(snapshot));
        lines.extend(transitions.iter().map(transition_line));
        let mut out = io::stdout().lock();
        let written = writeln!(out, "{}", lines.join("\n")).and_then(|()| out.flush());
        if let Err(e) = written {
            eprintln!("Could not write: {e}");
        }
    });
    let _engine = Engine::start(settings, sink);
    loop {
        thread::park();
    }
}

fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Waiting => "waiting",
        SessionState::Error => "error",
        SessionState::Unseen => "unseen",
        SessionState::Working => "working",
        SessionState::Idle => "idle",
    }
}

fn yes_no(ok: bool) -> &'static str {
    if ok { "ok" } else { "KO" }
}

/// A header with counts and source health, the errors, then one line per
/// session.
fn summary_lines(snapshot: &Snapshot) -> Vec<String> {
    let c = &snapshot.counts;
    let h = &snapshot.health;
    let gh = match h.gh_ok {
        Some(ok) => yes_no(ok),
        None => "?",
    };
    let mut lines = vec![format!(
        "{}{} session(s): {} waiting, {} error, {} unseen, {} working, {} idle \
         | PRs open {}, failing {} | messages awaiting {} \
         | registry {} · desktop {} · gh {}",
        if snapshot.ready { "" } else { "(not read yet) " },
        c.total,
        c.waiting,
        c.error,
        c.unseen,
        c.working,
        c.idle,
        c.pr_open,
        c.pr_failing,
        c.messages_pending,
        yes_no(h.registry_ok),
        yes_no(h.desktop_ok),
        gh,
    )];
    lines.extend(h.errors.iter().map(|e| format!("! {e}")));
    lines.extend(quota_line(snapshot));
    lines.extend(snapshot.sessions.iter().map(session_line));
    lines
}

/// `quota | sampled … ago | 5 h … | 7 d …`, times relative to the snapshot.
fn quota_line(snapshot: &Snapshot) -> Option<String> {
    let quota = snapshot.quota.as_ref()?;
    let now = snapshot.generated_at;
    let window = |label: &str, w: Option<&QuotaWindow>| match w {
        None => format!("{label} -"),
        Some(w) if w.expired => format!("{label} {}% (reset since)", w.percent),
        Some(w) => match w.resets_at {
            Some(at) => format!(
                "{label} {}% resets in {} ±{}",
                w.percent,
                duration(at - now),
                duration(w.reset_margin_ms)
            ),
            None => format!("{label} {}% reset unknown", w.percent),
        },
    };
    Some(format!(
        "quota | sampled {} ago | {} | {}",
        duration(now - quota.sampled_at),
        window("5 h", quota.five_hour.as_ref()),
        window("7 d", quota.seven_day.as_ref()),
    ))
}

/// "45 min", "4 h 12", "2 d 3 h".
fn duration(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{mins} min"),
        (0, _) => format!("{hours} h {mins:02}"),
        _ => format!("{days} d {hours} h"),
    }
}

/// `state | title | project/worktree @branch | waiting_for "question" | plan
/// | activity | pr | messages`, then `| on screen` when Desktop shows it.
fn session_line(s: &SessionView) -> String {
    let mut place = match &s.worktree {
        Some(worktree) => format!("{}/{worktree}", s.project),
        None => s.project.clone(),
    };
    if let Some(branch) = s.git.as_ref().and_then(|g| g.branch.as_deref()) {
        place.push_str(&format!(" @{branch}"));
    }
    let mut waiting = s.waiting_for.clone().unwrap_or_else(|| "-".to_owned());
    if let Some(question) = &s.question {
        waiting.push_str(&format!(" \"{}\"", shorten(&question.text, 80)));
    }
    let plan = match &s.plan {
        Some(p) => {
            let mut text = format!("plan {}/{}", p.done, p.total);
            if let Some(current) = &p.current {
                text.push_str(&format!(" \"{current}\""));
            }
            if p.wrapped_up {
                text.push_str(" (clos)");
            }
            text
        }
        None => "plan -".to_owned(),
    };
    let activity = match &s.activity {
        Some(a) if a.summary.is_empty() => a.name.clone(),
        Some(a) => format!("{}: {}", a.name, a.summary),
        None => "-".to_owned(),
    };
    let pr = match &s.pr {
        Some(pr) => {
            let checks = serde_json::to_value(pr.checks)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            let draft = if pr.is_draft { " draft" } else { "" };
            format!("PR #{} {}{draft} {checks}", pr.number, pr.state)
        }
        None => "PR -".to_owned(),
    };
    let m = &s.messages;
    let screen = if s.on_screen { " | on screen" } else { "" };
    let stopped = if s.stopped { " | stopped" } else { "" };
    let background = match &s.background {
        Some(wait) => {
            let kind = serde_json::to_value(wait.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            format!(" | awaits {kind} \"{}\" ({})", shorten(&wait.summary, 60), wait.count)
        }
        None => String::new(),
    };
    format!(
        "{:<7} | {} | {place} | {waiting} | {plan} | {activity} | {pr} | msg awaiting {} · undelivered {} · unread {}{background}{stopped}{screen}",
        state_name(s.state),
        s.title,
        m.awaiting_reply.len(),
        m.undelivered.len(),
        m.unread_inbound.len(),
    )
}

/// At most `max` characters, with an ellipsis when cut.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

fn transition_line(t: &Transition) -> String {
    let from = t.from.map_or("new", state_name);
    let reason = t
        .waiting_for
        .as_deref()
        .map(|w| format!(" ({w})"))
        .unwrap_or_default();
    let screen = if t.on_screen { " [on screen]" } else { "" };
    format!("  -> {}: {from} -> {}{reason}{screen}", t.title, state_name(t.to))
}

#[cfg(test)]
mod tests {
    use super::*;
    use monitor_core::model::{
        ChecksState, GitInfo, PendingQuestion, PlanProgress, PrInfo, ToolActivity,
    };

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_flags() {
        assert_eq!(parse_args(args(&[])), Ok(Options::default()));
        assert_eq!(
            parse_args(args(&["--watch", "--all"])),
            Ok(Options {
                summary: false,
                watch: true,
                all: true
            })
        );
        assert!(parse_args(args(&["--nope"])).is_err());
    }

    #[test]
    fn session_line_has_every_field() {
        let view = SessionView {
            title: "Dates relatives".into(),
            project: "atlas-web".into(),
            worktree: Some("wt-1".into()),
            state: SessionState::Waiting,
            waiting_for: Some("input needed".into()),
            plan: Some(PlanProgress {
                done: 2,
                total: 5,
                current: Some("Écrire les tests".into()),
                ..PlanProgress::default()
            }),
            activity: Some(ToolActivity {
                name: "Bash".into(),
                summary: "cargo test".into(),
                at: None,
            }),
            pr: Some(PrInfo {
                number: 12,
                state: "OPEN".into(),
                checks: ChecksState::Fail,
                ..PrInfo::default()
            }),
            ..SessionView::default()
        };
        let line = session_line(&view);
        assert_eq!(
            line,
            "waiting | Dates relatives | atlas-web/wt-1 | input needed | plan 2/5 \"Écrire les tests\" \
             | Bash: cargo test | PR #12 OPEN fail | msg awaiting 0 · undelivered 0 · unread 0"
        );
        let with_git = SessionView {
            git: Some(GitInfo {
                branch: Some("feat/x".into()),
                ..GitInfo::default()
            }),
            ..view
        };
        assert!(session_line(&with_git).contains(" | atlas-web/wt-1 @feat/x | "));
        assert!(!session_line(&with_git).ends_with("on screen"));

        let asking = SessionView {
            waiting_for: Some("decision".into()),
            question: Some(PendingQuestion {
                text: "Pousser feat/6-lot et ouvrir la PR ?".into(),
                options: vec!["Pousser et ouvrir".into(), "Pas encore".into()],
                source: "wrap_up".into(),
                at: None,
            }),
            on_screen: true,
            ..with_git
        };
        let line = session_line(&asking);
        assert!(line.contains(" | decision \"Pousser feat/6-lot et ouvrir la PR ?\" | "), "{line}");
        assert!(line.ends_with(" | on screen"), "{line}");
        assert!(!line.contains("stopped"), "{line}");

        let stopped = SessionView {
            stopped: true,
            ..asking
        };
        assert!(session_line(&stopped).ends_with(" | stopped | on screen"));
    }

    #[test]
    fn long_text_is_shortened() {
        assert_eq!(shorten("court", 10), "court");
        assert_eq!(shorten("éàüöçéàüöç-suite", 10), "éàüöçéàüö…");
    }

    #[test]
    fn summary_starts_with_counts_and_errors() {
        let mut snapshot = Snapshot::default();
        snapshot.health.errors.push("Folder not found: X".into());
        snapshot.sessions.push(SessionView::default());
        let lines = summary_lines(&snapshot);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("gh ?"));
        assert!(lines[0].starts_with("(not read yet) 0 session(s)"));
        snapshot.ready = true;
        assert!(summary_lines(&snapshot)[0].starts_with("0 session(s)"));
        assert_eq!(lines[1], "! Folder not found: X");
        assert!(lines[2].starts_with("idle "));
    }

    #[test]
    fn transition_line_mentions_new_sessions() {
        let t = Transition {
            key: "local_a".into(),
            title: "A".into(),
            from: None,
            to: SessionState::Working,
            waiting_for: None,
            ..Transition::default()
        };
        assert_eq!(transition_line(&t), "  -> A: new -> working");
        let t = Transition {
            from: Some(SessionState::Working),
            to: SessionState::Waiting,
            waiting_for: Some("permission prompt".into()),
            ..t
        };
        assert_eq!(
            transition_line(&t),
            "  -> A: working -> waiting (permission prompt)"
        );
        let seen = Transition {
            on_screen: true,
            ..t
        };
        assert!(transition_line(&seen).ends_with(" [on screen]"));
    }
}
