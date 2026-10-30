//! Joins the sources into the snapshot shown by the window. Pure functions.

use std::cmp::{Ordering, Reverse};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use crate::model::{
    BackgroundKind, BackgroundTask, BackgroundWait, ChecksState, Counts, DesktopSession,
    ErrorMark, GitInfo, MessageSummary, PeerLink, PeerReceipt, PendingQuestion, PrInfo,
    ProposedTask, RegistryEntry, RunningAgent, SessionRef, SessionState, SessionView, Settings, Snapshot,
    SourceHealth, TaskSuggestion, ToolActivity, TranscriptInfo, Transition, TurnOrigin,
};
use crate::transcript::plain_text;

/// Everything the engine read from the sources, ready to be joined.
pub struct AggregateInput<'a> {
    pub now_ms: i64,
    /// Live entries only.
    pub registry: &'a [RegistryEntry],
    /// By `host_session_id`, archived ones included.
    pub desktop: &'a HashMap<String, DesktopSession>,
    /// By CLI session id.
    pub transcripts: &'a HashMap<String, TranscriptInfo>,
    /// Running sub-agents, by CLI session id.
    pub agents: &'a HashMap<String, Vec<RunningAgent>>,
    /// By session key.
    pub git_pr: &'a HashMap<String, (Option<GitInfo>, Option<PrInfo>)>,
    /// Session key -> time the user acknowledged it (clicked it in the window,
    /// or was looking at it in Desktop when its turn ended).
    pub acks: &'a HashMap<String, i64>,
    /// Keys of the sessions shown in a pane of the Claude Desktop window while
    /// that window is in front.
    pub on_screen: &'a HashSet<String>,
    /// Session key -> time the engine saw the registry leave "busy". Stands in
    /// for Desktop's `last_turn_edge_at`, which reaches disk 1-3 s later and
    /// meanwhile still holds the turn's start.
    pub turn_ended_at: &'a HashMap<String, i64>,
    pub settings: &'a Settings,
    pub health: SourceHealth,
}

/// `entrypoint` of the sessions run by Claude Desktop's Code tab.
const DESKTOP_ENTRYPOINT: &str = "claude-desktop";
/// Title used when neither the registry, Desktop nor the folder gives one.
const FALLBACK_TITLE: &str = "Session";
const MS_PER_HOUR: i64 = 3_600_000;
/// Milliseconds between the FILETIME epoch (1601-01-01) and the Unix epoch.
const FILETIME_UNIX_OFFSET_MS: i64 = 11_644_473_600_000;
/// Desktop shows an error for 7 days.
const ERROR_WINDOW_MS: i64 = 7 * 24 * MS_PER_HOUR;
/// A Desktop session without a live process is considered when its latest
/// turn or activity is this recent.
pub const STOPPED_WINDOW_MS: i64 = ERROR_WINDOW_MS;
/// Slack between two sources stamping the same moment (Desktop writes its
/// turn edge a few milliseconds before or after the transcript's marker).
const CLOCK_SLACK_MS: i64 = 5_000;
/// The registry going "busy" this long after the transcript's end of turn
/// is a new turn starting, not background work of the turn that ended.
const NEW_TURN_SLACK_MS: i64 = 1_000;
/// How long a question found only in the text waits for Desktop's own
/// verdict on the turn, when Desktop writes one.
const CLASSIFIER_GRACE_MS: i64 = 20_000;
/// A background command stops being waited for after this long: it is most
/// likely a server left running. Of 520 commands measured, 3 ran over two
/// hours.
const COMMAND_WAIT_MS: i64 = 2 * MS_PER_HOUR;
/// Longest wait for other background work, should its end go unreported.
/// The longest workflow measured ran for 22 hours.
const BACKGROUND_WAIT_MS: i64 = 24 * MS_PER_HOUR;
/// Work with an end of its own still counts this long after it, the margin
/// Desktop gives its wake-ups.
const BACKGROUND_END_GRACE_MS: i64 = 120_000;
/// `waiting_for` of a turn that ended on an explicit question.
pub const WAITING_DECISION: &str = "decision";
/// `waiting_for` of a turn whose last message ends with a question mark.
pub const WAITING_PROBABLE_QUESTION: &str = "probable question";
/// `waiting_for` of a session blocked on a task offer: the person asked for
/// a task and must choose "New session" or "Do it here" in Desktop.
pub const WAITING_OFFER: &str = "offer";
/// `waiting_for` of a session waiting for the person to approve its plan.
pub const WAITING_PLAN_APPROVAL: &str = "plan approval";
/// A "New task" card is listed this long after it was proposed: Desktop's
/// own proposals expire then, and an older card is no longer current work.
pub const PROPOSAL_WINDOW_MS: i64 = 7 * 24 * MS_PER_HOUR;
/// Longest proposed task title, in characters.
const TASK_TITLE_MAX: usize = 120;
/// Longest recap, in characters.
const RECAP_MAX: usize = 400;
/// Longest question, in characters.
const QUESTION_MAX: usize = 300;

/// `host_session_id` when present, else `"cli:" + session_id`.
pub fn session_key(entry: &RegistryEntry) -> String {
    match non_blank(entry.host_session_id.as_deref()) {
        Some(host) => host.to_owned(),
        None => format!("cli:{}", entry.session_id),
    }
}

/// Joins registry, Desktop, transcript and git/PR data into the sorted list
/// of visible sessions, with their counts. Desktop sessions whose process
/// Desktop stopped ([`stopped_sessions`]) are listed only while they need
/// the person: waiting, error or unseen.
pub fn build_snapshot(input: &AggregateInput) -> Snapshot {
    let candidates = select_candidates(input);
    let titles = PeerTitles::new(input.desktop, &candidates);
    let cutoff = message_cutoff(input.now_ms, input.settings.message_window_hours);
    let inbound = inbound_by_target(input.desktop, cutoff, &titles);
    let registry_ok = input.health.registry_ok;
    let stopped: Vec<(String, RegistryEntry)> =
        stopped_sessions(input.now_ms, input.registry, registry_ok, input.desktop)
            .into_iter()
            .map(|d| {
                let entry = stopped_entry(d);
                (session_key(&entry), entry)
            })
            .collect();
    let live_views = candidates.iter().filter_map(|(key, entry)| {
        build_view(input, key, entry, false, cutoff, &titles, &inbound)
    });
    let stopped_views = stopped
        .iter()
        .filter_map(|(key, entry)| build_view(input, key, entry, true, cutoff, &titles, &inbound))
        .filter(|view| needs_person(view.state));
    let mut sessions: Vec<SessionView> = live_views.chain(stopped_views).collect();
    nest_families(&mut sessions, input.desktop);
    wait_for_side_sessions(&mut sessions, input.desktop);
    sort_sessions(&mut sessions);
    let counts = count_sessions(&sessions);
    let proposed_tasks = proposed_tasks(input, &sessions);
    Snapshot {
        generated_at: input.now_ms,
        ready: true,
        seq: 0,
        sessions,
        counts,
        health: input.health.clone(),
        // Not a session matter: the engine adds it.
        quota: None,
        proposed_tasks,
    }
}

/// The "New task" cards of the sessions not archived, proposed within
/// [`PROPOSAL_WINDOW_MS`] and not expired, newest first. Offers are left out
/// (a live session waiting on one shows it as its wait), and so are cloud
/// requests that start on their own. A card takes the project its session
/// shows when the session is listed, else the same project worked out from
/// the session's folder and git root, so it stays in its group when the
/// session leaves the list.
fn proposed_tasks(input: &AggregateInput, sessions: &[SessionView]) -> Vec<ProposedTask> {
    let mut tasks = Vec::new();
    for desktop in input.desktop.values().filter(|d| !d.is_archived) {
        let key = desktop.host_session_id.trim();
        if key.is_empty() {
            continue;
        }
        let view = sessions.iter().find(|s| s.key == key);
        let second_last_prompt = desktop
            .cli_session_id
            .as_deref()
            .and_then(|id| input.transcripts.get(id.trim()))
            .and_then(|t| t.previous_human_prompt_at);
        let cwd = desktop.cwd.as_deref().unwrap_or_default();
        let pending = desktop.suggestions.iter().filter(|s| is_listed_card(s, input.now_ms));
        for suggestion in pending {
            let title = plain_text(&suggestion.title, TASK_TITLE_MAX);
            if title.is_empty() {
                continue;
            }
            let (project, session_title) = match view {
                Some(view) => (view.project.clone(), view.title.clone()),
                None => {
                    let git = input.git_pr.get(key).and_then(|(git, _)| git.as_ref());
                    let worktree = non_blank(desktop.worktree_name.as_deref());
                    let title = non_blank(desktop.title.as_deref())
                        .or_else(|| last_component(cwd))
                        .unwrap_or(FALLBACK_TITLE);
                    (project_and_worktree(cwd, git, worktree).0, title.to_owned())
                }
            };
            let window_end = suggestion.created_at.saturating_add(PROPOSAL_WINDOW_MS);
            tasks.push(ProposedTask {
                id: suggestion.id.clone(),
                title,
                tldr: suggestion
                    .tldr
                    .as_deref()
                    .map(|t| plain_text(t, RECAP_MAX))
                    .filter(|t| !t.is_empty()),
                created_at: suggestion.created_at,
                expires_at: suggestion.expires_at,
                listed_until: suggestion.expires_at.map_or(window_end, |at| at.min(window_end)),
                session_key: key.to_owned(),
                session_title,
                project,
                passed_over: second_last_prompt.is_some_and(|at| at > suggestion.created_at),
            });
        }
    }
    tasks.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.session_key.cmp(&b.session_key))
            .then_with(|| a.id.cmp(&b.id))
    });
    tasks
}

/// A card the person may still start, recent enough to be listed.
fn is_listed_card(suggestion: &TaskSuggestion, now_ms: i64) -> bool {
    !suggestion.offer
        && !suggestion.auto_start
        && suggestion.created_at >= now_ms.saturating_sub(PROPOSAL_WINDOW_MS)
        && suggestion.expires_at.is_none_or(|at| at > now_ms)
}

/// CLI ids of the sessions with a listed card, whether the sessions are listed
/// or not: [`proposed_tasks`] reads in their transcripts if the card was passed over.
pub fn proposing_sessions<'a>(
    now_ms: i64,
    desktop: &'a HashMap<String, DesktopSession>,
) -> impl Iterator<Item = &'a str> {
    desktop
        .values()
        .filter(move |d| !d.is_archived && d.suggestions.iter().any(|s| is_listed_card(s, now_ms)))
        .filter_map(|d| d.cli_session_id.as_deref())
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

/// State changes between two snapshots, sessions that appeared included.
pub fn transitions(prev: Option<&Snapshot>, next: &Snapshot) -> Vec<Transition> {
    let before: HashMap<&str, SessionState> = prev
        .map(|p| p.sessions.iter().map(|s| (s.key.as_str(), s.state)).collect())
        .unwrap_or_default();
    next.sessions
        .iter()
        .filter_map(|s| {
            let from = before.get(s.key.as_str()).copied();
            (from != Some(s.state)).then(|| Transition {
                key: s.key.clone(),
                title: s.title.clone(),
                from,
                to: s.state,
                waiting_for: s.waiting_for.clone(),
                state_since: s.state_since,
                on_screen: s.on_screen,
            })
        })
        .collect()
}

/// What decides the state of one session.
#[derive(Debug, Clone, Copy, Default)]
pub struct StateInput<'a> {
    pub now_ms: i64,
    /// Raw registry status and waiting reason.
    pub status: Option<&'a str>,
    pub registry_waiting_for: Option<&'a str>,
    /// When the registry status last changed.
    pub status_since: Option<i64>,
    pub desktop: Option<&'a DesktopSession>,
    pub transcript: Option<&'a TranscriptInfo>,
    /// When the user acknowledged the session (click, or watching it in
    /// Desktop when its turn ended).
    pub acked_at: Option<i64>,
    /// When the engine saw the registry leave "busy": the end of the turn,
    /// seconds before Desktop writes it.
    pub turn_ended_at: Option<i64>,
    /// No process runs the session: Desktop stopped it.
    pub stopped: bool,
    /// When the process running the session started.
    pub process_started_at: Option<i64>,
}

/// The state of one session and what explains it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DerivedState {
    pub state: SessionState,
    /// Registry reason, or [`WAITING_DECISION`] / [`WAITING_PROBABLE_QUESTION`].
    pub waiting_for: Option<String>,
    pub question: Option<PendingQuestion>,
    /// When the state started.
    pub since: Option<i64>,
    /// Label of the error, in the Error state.
    pub error: Option<String>,
    /// What a Working session waits for, when no turn runs.
    pub background: Option<BackgroundWait>,
}

/// State of one session, in this order:
///
/// 0. A task offer the live session waits on ([`pending_offer`]): Waiting,
///    "offer". The registry shows such a session busy (its tool call blocks).
/// 1. Registry "waiting" (permission, question tool, dialog): Waiting, with
///    the registry's reason ([`registry_reason`]: "plan approval" for a plan).
/// 2. Registry "busy" while a turn runs: Working. The registry can stay busy
///    after the turn; the turn counts as over when the transcript ended it
///    and the registry did not go busy again after that.
/// 3. Turn over, registry not busy: Error when the turn ended on an error
///    the person has not answered ([`turn_error`]).
/// 4. Turn over: Waiting when it left a question the person has not
///    answered ([`pending_question`]): "decision" for an explicit question,
///    "probable question" for a last message ending with "?". Focus, acks
///    and the window do not clear it; only the person's next message does.
/// 5. Registry still busy, or background work the session waits for
///    ([`background_wait`]; the registry goes "idle" at the end of the turn
///    whatever still runs): Working, from the start of that turn.
/// 6. Unseen when the turn ended after both the last focus in Desktop and
///    the acknowledgement ([`finished_edge`]); else Idle.
pub fn derive_state(input: &StateInput) -> DerivedState {
    if let Some(offer) = pending_offer(input) {
        return DerivedState {
            state: SessionState::Waiting,
            waiting_for: Some(WAITING_OFFER.to_owned()),
            since: offer.at,
            question: Some(offer),
            ..DerivedState::default()
        };
    }
    let status = normalized_status(input.status);
    let status_since = input.status_since;
    if status == "waiting" {
        return DerivedState {
            state: SessionState::Waiting,
            waiting_for: registry_reason(input),
            since: status_since,
            ..DerivedState::default()
        };
    }
    let busy = status == "busy";
    let working = DerivedState {
        state: SessionState::Working,
        since: status_since,
        ..DerivedState::default()
    };
    if busy && !turn_over_while_busy(input) {
        return working;
    }
    if !busy && let Some(error) = turn_error(input) {
        return DerivedState {
            state: SessionState::Error,
            since: error.at.or(status_since),
            error: clean(error.label.as_deref()),
            ..DerivedState::default()
        };
    }
    if let Some(question) = pending_question(input) {
        let reason = if question.source == "text" {
            WAITING_PROBABLE_QUESTION
        } else {
            WAITING_DECISION
        };
        return DerivedState {
            state: SessionState::Waiting,
            waiting_for: Some(reason.to_owned()),
            since: question.at.or(status_since),
            question: Some(question),
            ..DerivedState::default()
        };
    }
    let background = background_wait(input);
    if busy || background.is_some() {
        let turn_start = input.transcript.and_then(|t| t.turn.as_ref()?.started_at);
        return DerivedState {
            since: if busy { status_since } else { turn_start.or(status_since) },
            background,
            ..working
        };
    }
    let edge = finished_edge(input);
    let Some(desktop) = input.desktop else {
        return DerivedState {
            since: edge.or(status_since),
            ..DerivedState::default()
        };
    };
    let seen_at = desktop.last_focused_at.max(input.acked_at);
    let unseen = edge.is_some_and(|edge| seen_at.is_none_or(|seen| edge > seen));
    DerivedState {
        state: if unseen {
            SessionState::Unseen
        } else {
            SessionState::Idle
        },
        since: if unseen {
            edge
        } else {
            latest_edge(input).or(status_since)
        },
        ..DerivedState::default()
    }
}

/// What the session waits for with no turn running: background work whose
/// end will wake it up. When several run, one that does the work is named
/// rather than a monitor or a wake-up. None for a stopped session.
fn background_wait(input: &StateInput) -> Option<BackgroundWait> {
    if input.stopped {
        return None;
    }
    let tasks: Vec<&BackgroundTask> = input
        .transcript?
        .background
        .iter()
        .filter(|task| is_awaited(task, input))
        .collect();
    let main = tasks
        .iter()
        .rev()
        .find(|task| !matches!(task.kind, BackgroundKind::Monitor | BackgroundKind::Wakeup))
        .or(tasks.last())?;
    Some(BackgroundWait {
        kind: main.kind,
        summary: main.summary.clone(),
        count: u32::try_from(tasks.len()).unwrap_or(u32::MAX),
        ends_at: main.ends_at,
    })
}

/// A task still runs for all we know: the running process started it (work
/// dies with its process, without a word), and it is neither past its own
/// end nor older than its kind's limit.
fn is_awaited(task: &BackgroundTask, input: &StateInput) -> bool {
    let limit = match task.kind {
        BackgroundKind::Command => COMMAND_WAIT_MS,
        _ => BACKGROUND_WAIT_MS,
    };
    let now = input.now_ms;
    let current = task.started_at.is_none_or(|at| {
        now.saturating_sub(at) < limit && input.process_started_at.is_none_or(|start| at >= start)
    });
    current && task.ends_at.is_none_or(|end| now < end.saturating_add(BACKGROUND_END_GRACE_MS))
}

/// With the registry "busy": the transcript ended the latest turn, and
/// neither the registry nor Desktop saw a new one start since.
fn turn_over_while_busy(input: &StateInput) -> bool {
    let Some(turn) = input.transcript.and_then(|t| t.turn.as_ref()) else {
        return false;
    };
    if !turn.ended {
        return false;
    }
    let Some(ended) = turn.ended_at else {
        return input.status_since.is_none();
    };
    let later = |at: Option<i64>| at.is_some_and(|at| at > ended.saturating_add(NEW_TURN_SLACK_MS));
    !later(input.status_since) && !later(input.desktop.and_then(|d| d.latest_user_frame_at))
}

/// Latest end of turn known: Desktop's edge or the engine's, whichever is
/// newer (Desktop writes the end 1-3 s late and holds the start meanwhile).
fn latest_edge(input: &StateInput) -> Option<i64> {
    input
        .desktop
        .and_then(|d| d.last_turn_edge_at)
        .max(input.turn_ended_at)
}

/// End of turn that makes a session "finished, not seen": the latest one,
/// except when another session started that turn (a child's ping, a peer
/// message) and it asked nothing: such a turn does not need the person,
/// so only the latest turn the person or the session's own task started
/// counts.
pub fn finished_edge(input: &StateInput) -> Option<i64> {
    match input.transcript {
        Some(t) if t.turn.as_ref().is_some_and(|turn| turn.origin == TurnOrigin::Peer) => {
            t.notable_turn_ended_at
        }
        _ => latest_edge(input),
    }
}

/// Error the latest turn ended on, unless the person wrote since. Sources,
/// the latest winning: Desktop's own `error` (what its sidebar shows),
/// Desktop's "failed" verdict on the turn, a `priorErrorMark` of a turn that
/// was running, a query crash, an API error in the transcript. All expire
/// after 7 days, as in Desktop. A mark or crash is also over once a later
/// turn (whoever started it) ended normally: the session runs again. A
/// turn that only started does not clear it; acks and focus do not either.
pub fn turn_error(input: &StateInput) -> Option<ErrorMark> {
    let now = input.now_ms;
    let desktop = input.desktop;
    let transcript = input.transcript;
    let edge = desktop.and_then(|d| d.last_turn_edge_at);
    let human_frame = match transcript {
        Some(t) => t.last_human_prompt_at,
        None => desktop.and_then(|d| d.latest_user_frame_at),
    };
    // Start of the latest turn when it ended normally (not on an API error).
    let clean_turn_start = transcript.and_then(|t| {
        let turn = t.turn.as_ref().filter(|turn| turn.ended)?;
        let start = turn.started_at?;
        t.api_error
            .as_ref()
            .is_none_or(|e| e.at.is_some_and(|at| at < start))
            .then_some(start)
    });
    let current = |at: Option<i64>, needs_edge: bool| -> bool {
        let Some(at) = at else {
            return false;
        };
        let recent = now.saturating_sub(at) < ERROR_WINDOW_MS;
        let unanswered = human_frame.is_none_or(|frame| at > frame);
        // A later turn ended normally: the session ran again. Without a
        // transcript, Desktop's later turn edge tells the same.
        let last = !needs_edge
            || match transcript {
                Some(_) => clean_turn_start.is_none_or(|start| start <= at),
                None => edge.is_none_or(|edge| at.saturating_add(CLOCK_SLACK_MS) >= edge),
            };
        recent && unanswered && last
    };
    let mut candidates: Vec<ErrorMark> = Vec::new();
    if let Some(d) = desktop {
        candidates.extend(d.error.clone().filter(|e| current(e.at, false)));
        let end = desktop_turn_end(d);
        if let Some(summary) = d.turn_summary.as_ref().filter(|s| s.fresh)
            && summary.category.as_deref() == Some("failed")
            && desktop_verdict_is_current(input)
            && current(end, false)
        {
            candidates.push(ErrorMark {
                at: end,
                label: summary
                    .needs_action
                    .clone()
                    .or_else(|| summary.detail.clone())
                    .or_else(|| Some("failed".to_owned())),
            });
        }
        candidates.extend(d.prior_error.clone().filter(|e| current(e.at, true)));
        candidates.extend(d.last_crash.clone().filter(|e| current(e.at, true)));
    }
    if let Some(t) = transcript {
        candidates.extend(t.api_error.clone().filter(|e| current(e.at, false)));
    }
    candidates.into_iter().max_by_key(|e| e.at)
}

/// End of Desktop's latest turn: its edge, else (older files) its latest
/// activity.
fn desktop_turn_end(desktop: &DesktopSession) -> Option<i64> {
    desktop.last_turn_edge_at.or(desktop.last_activity_at)
}

/// Desktop's verdict on its latest turn still applies: no message reached
/// Desktop after that turn ended, and the transcript has no newer turn.
/// Desktop clears its verdict at the next message, so without any time to
/// compare, it applies.
fn desktop_verdict_is_current(input: &StateInput) -> bool {
    let Some(desktop) = input.desktop else {
        return false;
    };
    let Some(end) = desktop_turn_end(desktop) else {
        return true;
    };
    let newer = |at: Option<i64>| at.is_some_and(|at| at > end.saturating_add(CLOCK_SLACK_MS));
    desktop.latest_user_frame_at.is_none_or(|frame| frame <= end)
        && !newer(input.transcript.and_then(|t| t.turn.as_ref()?.started_at))
}

/// The question the session's latest turns left to the person, if the
/// person has not answered it. Called once the turn is over. In order:
///
/// 1. `wrap_up` with `ask` (options: its buttons) or `blocked`, from the
///    transcript ("wrap_up"). It survives the turns other sessions or
///    background tasks start; the person's next prompt clears it. A message
///    Desktop received after it that the transcript does not show yet also
///    hides it.
/// 2. Desktop's fresh verdict on its latest turn: "blocked" or "need_input",
///    or something the person must do ("desktop").
/// 3. The closing message of the latest turn ends with "?" and the turn
///    made no `wrap_up` ("text"). When Desktop writes verdicts, its verdict
///    on that turn decides instead, and the text waits for it for a while.
pub fn pending_question(input: &StateInput) -> Option<PendingQuestion> {
    let desktop = input.desktop;
    let transcript = input.transcript;
    let latest_turn = transcript.and_then(|t| t.turn.as_ref());
    // A message Desktop got after `at` that the transcript does not show
    // yet: the person may have answered. Once the transcript moved past the
    // frame, the frame is explained (a queued task notification taken mid-
    // turn also moves it).
    let unexplained_after = |at: Option<i64>| {
        let turn_start = latest_turn.and_then(|turn| turn.started_at);
        let transcript_last = transcript.and_then(|t| t.last_event_at);
        desktop
            .and_then(|d| d.latest_user_frame_at)
            .is_some_and(|frame| {
                at.is_none_or(|at| frame > at)
                    && turn_start.is_none_or(|start| frame > start.saturating_add(CLOCK_SLACK_MS))
                    && transcript_last.is_none_or(|last| frame > last)
            })
    };

    if let Some(ask) = transcript.and_then(|t| t.pending_ask.as_ref())
        && !unexplained_after(ask.at)
    {
        return Some(question(ask.clone()));
    }

    let verdict = desktop
        .and_then(|d| d.turn_summary.as_ref())
        .filter(|s| s.fresh && desktop_verdict_is_current(input));
    if let Some(summary) = verdict {
        let category = summary.category.as_deref().unwrap_or_default();
        let needs = summary.needs_action.as_deref().and_then(|n| non_blank(Some(n)));
        if category != "failed" && (matches!(category, "blocked" | "need_input") || needs.is_some())
        {
            let text = needs.or(summary.detail.as_deref()).unwrap_or(category);
            return Some(question(PendingQuestion {
                text: text.to_owned(),
                options: Vec::new(),
                source: "desktop".to_owned(),
                at: desktop.and_then(desktop_turn_end),
            }));
        }
    }

    let turn = latest_turn.filter(|turn| !turn.wrapped_up)?;
    let text = turn.closing_question.as_deref()?;
    let ended_at = turn.ended_at.or(input.turn_ended_at);
    if unexplained_after(ended_at.or(turn.started_at)) {
        return None;
    }
    if let Some(d) = desktop {
        // Desktop already judged this very turn, and found nothing pending.
        let judged = verdict.is_some()
            && match (desktop_turn_end(d), ended_at) {
                (Some(edge), Some(end)) => edge.saturating_add(CLOCK_SLACK_MS) >= end,
                _ => true,
            };
        if judged {
            return None;
        }
        // An unknown end (no stop marker, no edge) does not wait.
        let fresh_end = ended_at
            .or_else(|| desktop_turn_end(d))
            .is_some_and(|end| input.now_ms.saturating_sub(end) < CLASSIFIER_GRACE_MS);
        if d.classifier_summary_enabled && fresh_end {
            return None;
        }
    }
    Some(question(PendingQuestion {
        text: text.to_owned(),
        options: Vec::new(),
        source: "text".to_owned(),
        at: ended_at.or(turn.started_at),
    }))
}

/// The registry's waiting reason; [`WAITING_PLAN_APPROVAL`] when the call
/// waiting for permission is `ExitPlanMode`, which the CLI reports as a
/// plain "permission prompt".
fn registry_reason(input: &StateInput) -> Option<String> {
    let reason = clean(input.registry_waiting_for);
    let plan = input
        .transcript
        .and_then(|t| t.current_tool.as_ref())
        .is_some_and(|tool| tool.name == "ExitPlanMode");
    if plan && reason.as_deref().is_none_or(|r| r.eq_ignore_ascii_case("permission prompt")) {
        return Some(WAITING_PLAN_APPROVAL.to_owned());
    }
    reason
}

/// The task offer a live session waits on: the person asked for a task, and
/// Desktop blocks the session until they choose "New session" or "Do it
/// here" (the latest one when several are left). Not once "New session" was
/// chosen, nor after the offer expired. A stopped session waits on nothing:
/// its offer is a leftover that Desktop drops at its next start.
fn pending_offer(input: &StateInput) -> Option<PendingQuestion> {
    if input.stopped {
        return None;
    }
    let offer = input
        .desktop?
        .suggestions
        .iter()
        .filter(|s| s.offer && !s.accepted && s.expires_at.is_none_or(|at| at > input.now_ms))
        .max_by_key(|s| s.created_at)?;
    let title = plain_text(&offer.title, TASK_TITLE_MAX);
    Some(question(PendingQuestion {
        text: format!("Nouvelle session pour « {title} », ou ici ?"),
        options: vec!["Nouvelle session".to_owned(), "Ici".to_owned()],
        source: "offer".to_owned(),
        at: Some(offer.created_at),
    }))
}

/// Question text and options as plain, single-line text.
fn question(mut q: PendingQuestion) -> PendingQuestion {
    q.text = plain_text(&q.text, QUESTION_MAX);
    q.options = q
        .options
        .iter()
        .map(|o| plain_text(o, QUESTION_MAX))
        .filter(|o| !o.is_empty())
        .collect();
    q
}

fn normalized_status(status: Option<&str>) -> String {
    status.unwrap_or_default().trim().to_ascii_lowercase()
}

/// Trimmed text, or None when missing or blank.
fn non_blank(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|t| !t.is_empty())
}

/// Owned trimmed text, or None when missing or blank.
fn clean(text: Option<&str>) -> Option<String> {
    non_blank(text).map(str::to_owned)
}

fn is_desktop_entry(entry: &RegistryEntry) -> bool {
    entry
        .entrypoint
        .as_deref()
        .is_some_and(|e| e.trim().eq_ignore_ascii_case(DESKTOP_ENTRYPOINT))
}

// ---------------------------------------------------------------------------
// Candidates
// ---------------------------------------------------------------------------

/// Registry entries to consider, one per session key: Desktop sessions (plus
/// CLI sessions when enabled), the best-ranked process winning (see
/// [`entry_rank`]).
fn select_candidates<'a>(input: &AggregateInput<'a>) -> Vec<(String, &'a RegistryEntry)> {
    let mut by_key: HashMap<String, &'a RegistryEntry> = HashMap::new();
    for entry in input.registry {
        if !is_desktop_entry(entry) && !input.settings.include_cli_sessions {
            continue;
        }
        match by_key.entry(session_key(entry)) {
            Entry::Occupied(mut slot) => {
                if entry_rank(entry, input.desktop) > entry_rank(slot.get(), input.desktop) {
                    slot.insert(entry);
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(entry);
            }
        }
    }
    by_key.into_iter().collect()
}

/// Ordering key from [`entry_rank`]: (linked by Desktop, start time, pid).
pub type EntryRank = (bool, Option<i64>, u32);

/// Rank of a live registry entry among the entries sharing its session key
/// (the highest wins): the process its Desktop file links to through
/// `cliSessionId` first, then the most recently started one, then the
/// highest pid.
pub fn entry_rank(entry: &RegistryEntry, desktop: &HashMap<String, DesktopSession>) -> EntryRank {
    let linked = non_blank(entry.host_session_id.as_deref())
        .and_then(|host| desktop.get(host))
        .and_then(|d| non_blank(d.cli_session_id.as_deref()))
        .is_some_and(|cli| cli == entry.session_id.trim());
    let (started, pid) = launch_order(entry);
    (linked, started, pid)
}

/// Sort key of a process launch: start time (`startedAt`, else `procStart`),
/// then pid. Unknown start times sort first.
fn launch_order(entry: &RegistryEntry) -> (Option<i64>, u32) {
    let started = entry
        .started_at
        .or_else(|| entry.proc_start.as_deref().and_then(filetime_to_unix_ms));
    (started, entry.pid)
}

/// Converts a Windows FILETIME (100 ns ticks since 1601) written as a decimal
/// string to Unix milliseconds.
fn filetime_to_unix_ms(raw: &str) -> Option<i64> {
    let ticks = raw.trim().parse::<u64>().ok()?;
    i64::try_from(ticks / 10_000)
        .ok()
        .map(|ms| ms - FILETIME_UNIX_OFFSET_MS)
}

/// A process Desktop started ahead of time that nobody used yet: no name,
/// no prompt, no completed turn, nothing running.
fn is_ghost(entry: &RegistryEntry, is_desktop: bool, desktop: Option<&DesktopSession>) -> bool {
    let idle = matches!(normalized_status(entry.status.as_deref()).as_str(), "" | "idle");
    let unused = desktop.is_none_or(|d| {
        d.latest_user_frame_at.is_none() && d.completed_turns.unwrap_or(0) == 0
    });
    is_desktop && idle && unused && non_blank(entry.name.as_deref()).is_none()
}

/// Desktop sessions without a live process whose state is worth working
/// out: not archived, active within [`STOPPED_WINDOW_MS`], and no live entry
/// (whatever its kind) runs them, by Desktop id or by CLI id. Desktop keeps
/// only a few processes alive and stops idle ones beyond that or under
/// memory pressure, so a session can lose its process while it still waits
/// for the person. Sorted by Desktop id. None while the registry was never
/// read (`registry_ok` false and nothing kept from an earlier read): every
/// session would look stopped.
pub fn stopped_sessions<'a>(
    now_ms: i64,
    registry: &[RegistryEntry],
    registry_ok: bool,
    desktop: &'a HashMap<String, DesktopSession>,
) -> Vec<&'a DesktopSession> {
    if !registry_ok && registry.is_empty() {
        return Vec::new();
    }
    let live_hosts: HashSet<&str> = registry
        .iter()
        .filter_map(|e| non_blank(e.host_session_id.as_deref()))
        .collect();
    let live_clis: HashSet<&str> = registry
        .iter()
        .filter_map(|e| non_blank(Some(e.session_id.as_str())))
        .collect();
    let since = now_ms.saturating_sub(STOPPED_WINDOW_MS);
    let mut stopped: Vec<&DesktopSession> = desktop
        .values()
        .filter(|d| {
            let host = d.host_session_id.trim();
            let recent = d.last_turn_edge_at.max(d.last_activity_at).is_some_and(|at| at >= since);
            !host.is_empty()
                && !d.is_archived
                && recent
                && !live_hosts.contains(host)
                && non_blank(d.cli_session_id.as_deref()).is_none_or(|cli| !live_clis.contains(cli))
        })
        .collect();
    stopped.sort_by(|a, b| a.host_session_id.cmp(&b.host_session_id));
    stopped
}

/// Stand-in registry entry for a stopped Desktop session: no process, no
/// status, the transcript found through Desktop's CLI id.
fn stopped_entry(desktop: &DesktopSession) -> RegistryEntry {
    RegistryEntry {
        session_id: clean(desktop.cli_session_id.as_deref()).unwrap_or_default(),
        cwd: desktop.cwd.clone().unwrap_or_default(),
        entrypoint: Some(DESKTOP_ENTRYPOINT.to_owned()),
        host_session_id: Some(desktop.host_session_id.trim().to_owned()),
        ..RegistryEntry::default()
    }
}

/// States a stopped session is listed in.
fn needs_person(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Waiting | SessionState::Error | SessionState::Unseen
    )
}

// ---------------------------------------------------------------------------
// One session
// ---------------------------------------------------------------------------

/// Display names of other sessions (message peers, parents).
struct PeerTitles<'a> {
    desktop: &'a HashMap<String, DesktopSession>,
    /// Registry name of each live Desktop session, by host id.
    live_names: HashMap<&'a str, &'a str>,
}

impl<'a> PeerTitles<'a> {
    fn new(
        desktop: &'a HashMap<String, DesktopSession>,
        candidates: &[(String, &'a RegistryEntry)],
    ) -> Self {
        let live_names = candidates
            .iter()
            .filter_map(|&(_, entry)| {
                let host = non_blank(entry.host_session_id.as_deref())?;
                let name = non_blank(entry.name.as_deref())?;
                Some((host, name))
            })
            .collect();
        Self { desktop, live_names }
    }

    /// Title of the peer's Desktop file, else the name of its live process.
    fn get(&self, host_id: &str) -> Option<String> {
        let id = host_id.trim();
        self.desktop
            .get(id)
            .and_then(|d| non_blank(d.title.as_deref()))
            .or_else(|| self.live_names.get(id).copied())
            .map(str::to_owned)
    }
}

/// Builds the view of one candidate, or None when it must stay hidden
/// (archived in Desktop, or a pre-warmed ghost). `stopped`: `entry` stands
/// in for a process Desktop stopped ([`stopped_entry`]).
fn build_view(
    input: &AggregateInput,
    key: &str,
    entry: &RegistryEntry,
    stopped: bool,
    cutoff: i64,
    titles: &PeerTitles,
    inbound: &HashMap<&str, Vec<PeerLink>>,
) -> Option<SessionView> {
    let is_desktop = is_desktop_entry(entry);
    let host_id = non_blank(entry.host_session_id.as_deref());
    let desktop = host_id.and_then(|id| input.desktop.get(id));
    if desktop.is_some_and(|d| d.is_archived) || is_ghost(entry, is_desktop, desktop) {
        return None;
    }
    let transcript = input.transcripts.get(&entry.session_id);
    let (git, mut pr) = input.git_pr.get(key).cloned().unwrap_or_default();
    if let Some(pr) = pr.as_mut() {
        pr.auto_fix = auto_fix_watches(input, desktop, pr);
    }
    let derived = derive_state(&StateInput {
        now_ms: input.now_ms,
        status: entry.status.as_deref(),
        registry_waiting_for: entry.waiting_for.as_deref(),
        status_since: entry.status_updated_at.or(entry.updated_at),
        desktop,
        transcript,
        acked_at: input.acks.get(key).copied(),
        turn_ended_at: input.turn_ended_at.get(key).copied(),
        stopped,
        process_started_at: entry.started_at,
    });
    let state = derived.state;

    let cwd = effective_cwd(entry, desktop);
    let (project, worktree) = project_and_worktree(
        &cwd,
        git.as_ref(),
        desktop.and_then(|d| non_blank(d.worktree_name.as_deref())),
    );
    // Desktop renames sessions after the process started; the registry
    // keeps the name it had at spawn. The sidebar shows Desktop's title.
    let title = desktop
        .and_then(|d| non_blank(d.title.as_deref()))
        .or_else(|| non_blank(entry.name.as_deref()))
        .or_else(|| last_component(&cwd))
        .unwrap_or(FALLBACK_TITLE)
        .to_owned();
    let (awaiting_reply, undelivered) = outbound_messages(input, desktop, cutoff, titles);
    let unread_inbound = host_id
        .and_then(|id| inbound.get(id))
        .cloned()
        .unwrap_or_default();

    Some(SessionView {
        key: key.to_owned(),
        question: derived.question,
        on_screen: input.on_screen.contains(key),
        host_session_id: host_id.map(str::to_owned),
        cli_session_id: entry.session_id.clone(),
        pid: entry.pid,
        is_desktop,
        stopped,
        title,
        project,
        cwd,
        worktree,
        state,
        waiting_for: derived.waiting_for,
        state_since: derived.since,
        model: desktop.and_then(|d| clean(d.model.as_deref())),
        effort: desktop.and_then(|d| clean(d.effort.as_deref())),
        permission_mode: desktop.and_then(|d| clean(d.permission_mode.as_deref())),
        completed_turns: desktop.and_then(|d| d.completed_turns),
        last_activity_at: desktop
            .and_then(|d| d.last_activity_at)
            .max(transcript.and_then(|t| t.last_event_at)),
        recap: recap(desktop, transcript),
        activity: transcript.and_then(|t| activity(state, t)),
        plan: transcript.and_then(|t| t.plan.clone()),
        git,
        pr,
        messages: MessageSummary {
            awaiting_reply,
            undelivered,
            unread_inbound,
        },
        parent: desktop.and_then(|d| parent_ref(d, titles)),
        // Known once every session is listed: nest_families.
        nest_under: None,
        context_exceeded_count: desktop.and_then(|d| d.context_exceeded_count),
        last_error: derived.error,
        version: clean(entry.version.as_deref()),
        findings: transcript
            .and_then(|t| t.findings.clone())
            .filter(|f| f.open > 0),
        background: derived.background,
        // A dead process leaves its list behind: only a Working session has any.
        agents: if state == SessionState::Working && !stopped {
            input.agents.get(&entry.session_id).cloned().unwrap_or_default()
        } else {
            Vec::new()
        },
    })
}

/// Desktop's Auto-fix watches `pr` for this session: Desktop runs (it
/// started a process still alive), the session is not archived, the PR is
/// open, and it is one of the session's Auto-fix cards (same number and
/// repository, or same link).
fn auto_fix_watches(input: &AggregateInput, desktop: Option<&DesktopSession>, pr: &PrInfo) -> bool {
    let Some(desktop) = desktop.filter(|d| !d.is_archived) else {
        return false;
    };
    let desktop_runs = input.registry.iter().any(is_desktop_entry);
    let open = pr.state.trim().eq_ignore_ascii_case("OPEN");
    let same_repo = |card: Option<&str>| match (non_blank(card), non_blank(pr.repository.as_deref())) {
        (Some(card), Some(shown)) => card.eq_ignore_ascii_case(shown),
        _ => true,
    };
    let link = |url: &str| url.trim().trim_end_matches('/').to_ascii_lowercase();
    desktop_runs
        && open
        && desktop.auto_fix_prs.iter().any(|card| {
            let by_number = card.number == Some(pr.number) && same_repo(card.repository.as_deref());
            let by_link = card
                .url
                .as_deref()
                .is_some_and(|url| !pr.url.trim().is_empty() && link(url) == link(&pr.url));
            by_number || by_link
        })
}

/// Registry cwd, else the Desktop file's cwd.
fn effective_cwd(entry: &RegistryEntry, desktop: Option<&DesktopSession>) -> String {
    if !entry.cwd.trim().is_empty() {
        return entry.cwd.clone();
    }
    desktop
        .and_then(|d| d.cwd.clone())
        .filter(|cwd| !cwd.trim().is_empty())
        .unwrap_or_default()
}

/// Recap of the latest turn, as plain text: Desktop's wrap-up, else the
/// transcript's wrap-up of the latest turn (before Desktop saves it), else
/// Desktop's fresh verdict, else its turn report, else the latest assistant
/// text (Desktop rarely keeps the first ones).
fn recap(desktop: Option<&DesktopSession>, transcript: Option<&TranscriptInfo>) -> Option<String> {
    let wrap_up = desktop.and_then(|d| d.turn_wrap_up.as_deref());
    let transcript_wrap_up = transcript
        .and_then(|t| t.turn.as_ref())
        .filter(|turn| turn.wrapped_up)
        .and_then(|turn| turn.wrap_up_recap.as_deref());
    let verdict = desktop
        .and_then(|d| d.turn_summary.as_ref())
        .filter(|s| s.fresh)
        .and_then(|s| s.detail.as_deref());
    let report = desktop.and_then(|d| d.last_turn_report.as_deref());
    let said = transcript.and_then(|t| t.last_assistant_text.as_deref());
    [wrap_up, transcript_wrap_up, verdict, report, said]
        .into_iter()
        .flatten()
        .map(|text| plain_text(text, RECAP_MAX))
        .find(|text| !text.is_empty())
}

/// Tool running now while working, else the latest tool used.
fn activity(state: SessionState, transcript: &TranscriptInfo) -> Option<ToolActivity> {
    let tool = if state == SessionState::Working {
        transcript.current_tool.as_ref().or(transcript.last_tool.as_ref())
    } else {
        transcript.last_tool.as_ref()
    };
    tool.cloned()
}

/// The session this one comes from ([`lineage_parent`]), titled from the
/// parent's own Desktop file or live process. `spawnedFrom.title` is not
/// used: Desktop stores there the title given to the new session, not the
/// parent's.
fn parent_ref(desktop: &DesktopSession, titles: &PeerTitles) -> Option<SessionRef> {
    let id = lineage_parent(desktop)?;
    Some(SessionRef {
        host_session_id: id.to_owned(),
        title: titles.get(id),
    })
}

/// The session this one hangs from in Desktop's sidebar, as Desktop works
/// it out: none once the person detached it; else the session that started
/// it as a side session; else the session it was forked from; else the
/// session whose "New task" card started it. A link to itself is ignored.
fn lineage_parent(desktop: &DesktopSession) -> Option<&str> {
    if desktop.lineage_detached {
        return None;
    }
    let own = desktop.host_session_id.trim();
    let spawner = desktop
        .spawned_from
        .as_ref()
        .and_then(|s| non_blank(Some(s.host_session_id.as_str())))
        .filter(|id| *id != own);
    let fork = non_blank(desktop.forked_from.as_deref()).filter(|id| *id != own);
    if desktop.side_session && spawner.is_some() {
        return spawner;
    }
    fork.or(spawner)
}

/// Sets each session's `nest_under`: its nearest ancestor listed with it,
/// up Desktop's lineage. Desktop puts a whole lineage in one family and,
/// as it is set up, links every session past its archived ancestors: the
/// walk goes past any ancestor that is not listed (archived, or stopped
/// while idle).
fn nest_families(sessions: &mut [SessionView], desktop: &HashMap<String, DesktopSession>) {
    let listed: HashSet<String> = sessions.iter().map(|s| s.key.clone()).collect();
    for view in sessions.iter_mut() {
        view.nest_under = view
            .host_session_id
            .as_deref()
            .and_then(|id| listed_ancestor(id, desktop, &listed));
    }
}

fn listed_ancestor(
    id: &str,
    desktop: &HashMap<String, DesktopSession>,
    listed: &HashSet<String>,
) -> Option<String> {
    let mut seen = HashSet::from([id.trim()]);
    let mut current = desktop.get(id)?;
    loop {
        let parent = lineage_parent(current)?;
        if !seen.insert(parent) {
            return None;
        }
        if listed.contains(parent) {
            return Some(parent.to_owned());
        }
        current = desktop.get(parent)?;
    }
}

/// A live session whose turn is over stays Working while side sessions it
/// started work: Desktop continues it once they are done. A side session
/// waiting that way counts as working for its own parent.
fn wait_for_side_sessions(sessions: &mut [SessionView], desktop: &HashMap<String, DesktopSession>) {
    loop {
        let mut working: HashMap<String, Vec<String>> = HashMap::new();
        for view in sessions.iter().filter(|s| s.state == SessionState::Working) {
            let parent = view
                .host_session_id
                .as_deref()
                .and_then(|id| desktop.get(id))
                .filter(|d| d.side_session && !d.lineage_detached)
                .and_then(|d| d.spawned_from.as_ref());
            if let Some(parent) = parent {
                working
                    .entry(parent.host_session_id.trim().to_owned())
                    .or_default()
                    .push(view.title.clone());
            }
        }
        let mut changed = false;
        for view in sessions.iter_mut() {
            let over = matches!(view.state, SessionState::Idle | SessionState::Unseen);
            if let Some(titles) = working.get(&view.key).filter(|_| over && !view.stopped) {
                view.state = SessionState::Working;
                view.background = Some(BackgroundWait {
                    kind: BackgroundKind::Sessions,
                    summary: titles.first().cloned().unwrap_or_default(),
                    count: u32::try_from(titles.len()).unwrap_or(u32::MAX),
                    ends_at: None,
                });
                changed = true;
            }
        }
        if !changed {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Project and worktree
// ---------------------------------------------------------------------------

/// Repository folder name and worktree name of a session.
///
/// A cwd under `<repo>\.claude\worktrees\<name>` gives `<repo>` and `<name>`.
/// Otherwise the project is the basename of the git root, else of the cwd.
/// Desktop's own worktree name wins when it has one.
fn project_and_worktree(
    cwd: &str,
    git: Option<&GitInfo>,
    desktop_worktree: Option<&str>,
) -> (String, Option<String>) {
    let parsed = parse_worktree_path(cwd);
    let project = parsed
        .and_then(|wt| wt.project)
        .or_else(|| git.and_then(|g| last_component(g.repo_root.as_deref()?)))
        .or_else(|| last_component(cwd))
        .unwrap_or_default()
        .to_owned();
    let worktree = desktop_worktree
        .or(parsed.map(|wt| wt.name))
        .map(str::to_owned);
    (project, worktree)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct WorktreePath<'a> {
    /// Folder holding `.claude`, None when `.claude` is the first component.
    project: Option<&'a str>,
    name: &'a str,
}

/// Finds a `.claude\worktrees\<name>` run in a path (`\` or `/`, any case).
/// The project comes from the outermost run, the name from the innermost.
fn parse_worktree_path(path: &str) -> Option<WorktreePath<'_>> {
    let parts: Vec<&str> = path_components(path).collect();
    let is_marker = |i: usize| match (parts.get(i), parts.get(i + 1), parts.get(i + 2)) {
        (Some(dot_claude), Some(worktrees), Some(_)) => {
            dot_claude.eq_ignore_ascii_case(".claude") && worktrees.eq_ignore_ascii_case("worktrees")
        }
        _ => false,
    };
    let first = (0..parts.len()).find(|&i| is_marker(i))?;
    let last = (0..parts.len()).rev().find(|&i| is_marker(i)).unwrap_or(first);
    Some(WorktreePath {
        project: first.checked_sub(1).and_then(|i| parts.get(i).copied()),
        name: parts.get(last + 2).copied()?,
    })
}

/// Non-empty components of a Windows or POSIX path.
fn path_components(path: &str) -> impl DoubleEndedIterator<Item = &str> {
    path.split(['\\', '/'])
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != ".")
}

fn last_component(path: &str) -> Option<&str> {
    path_components(path).next_back()
}

// ---------------------------------------------------------------------------
// Messages between sessions
// ---------------------------------------------------------------------------

/// Oldest `sent_at` still reported.
fn message_cutoff(now_ms: i64, window_hours: u64) -> i64 {
    let window = i64::try_from(window_hours)
        .unwrap_or(i64::MAX)
        .saturating_mul(MS_PER_HOUR);
    now_ms.saturating_sub(window)
}

/// Delivered according to Desktop, or obviously delivered since it was read
/// or answered.
fn is_delivered(receipt: &PeerReceipt) -> bool {
    receipt
        .delivery
        .as_deref()
        .is_some_and(|d| d.trim().eq_ignore_ascii_case("delivered"))
        || receipt.delivered_at.is_some()
        || receipt.read_at.is_some()
        || receipt.replied_at.is_some()
}

fn peer_link(peer_id: &str, receipt: &PeerReceipt, titles: &PeerTitles) -> PeerLink {
    PeerLink {
        peer_host_session_id: peer_id.to_owned(),
        peer_title: titles.get(peer_id),
        sent_at: receipt.sent_at,
        delivered_at: receipt.delivered_at,
        read_at: receipt.read_at,
    }
}

/// Messages this session sent within the window: (delivered and awaiting a
/// reply, not delivered). A delivered message stops awaiting a reply once
/// the recipient answered in any way ([`reply_settled`]).
fn outbound_messages(
    input: &AggregateInput,
    desktop: Option<&DesktopSession>,
    cutoff: i64,
    titles: &PeerTitles,
) -> (Vec<PeerLink>, Vec<PeerLink>) {
    let mut awaiting = Vec::new();
    let mut undelivered = Vec::new();
    let Some(sender) = desktop else {
        return (awaiting, undelivered);
    };
    let sender_id = sender.host_session_id.trim();
    for receipt in sender.peer_receipts.iter().filter(|r| r.sent_at >= cutoff) {
        let link = peer_link(receipt.target_session_id.trim(), receipt, titles);
        if !is_delivered(receipt) {
            undelivered.push(link);
        } else if receipt.replied_at.is_none() && !reply_settled(input, sender_id, receipt) {
            awaiting.push(link);
        }
    }
    sort_links(&mut awaiting);
    sort_links(&mut undelivered);
    (awaiting, undelivered)
}

/// A message no reply is expected for any more: its recipient is archived
/// (hidden while it is), wrote back to the sender after it, or read it and
/// has since finished a turn without answering. Desktop sets `repliedAt`
/// only for a reply through the same channel.
fn reply_settled(input: &AggregateInput, sender_id: &str, receipt: &PeerReceipt) -> bool {
    let target_id = receipt.target_session_id.trim();
    let Some(target) = input.desktop.get(target_id) else {
        return false;
    };
    if target.is_archived {
        return true;
    }
    let wrote_back = target.peer_receipts.iter().any(|r| {
        r.target_session_id.trim() == sender_id && r.sent_at > receipt.sent_at
    });
    if wrote_back {
        return true;
    }
    let running = input.registry.iter().any(|e| {
        non_blank(e.host_session_id.as_deref()) == Some(target_id)
            && matches!(normalized_status(e.status.as_deref()).as_str(), "busy" | "waiting")
    });
    receipt.read_at.is_some_and(|read| {
        !running && target.last_turn_edge_at.is_some_and(|edge| edge > read)
    })
}

/// Unread messages within the window, by recipient host id, gathered from
/// every Desktop file (live or not, archived included): sent messages not
/// read yet, and turn reports of other sessions queued for the recipient.
fn inbound_by_target<'a>(
    desktop: &'a HashMap<String, DesktopSession>,
    cutoff: i64,
    titles: &PeerTitles,
) -> HashMap<&'a str, Vec<PeerLink>> {
    let mut inbound: HashMap<&'a str, Vec<PeerLink>> = HashMap::new();
    for (file_key, owner) in desktop {
        let owner_id = non_blank(Some(owner.host_session_id.as_str())).unwrap_or(file_key.trim());
        for note in owner.queued_notes.iter().filter(|n| n.at >= cutoff) {
            let peer = note.session_id.trim();
            if peer.is_empty() || peer == owner_id {
                continue;
            }
            inbound.entry(owner_id).or_default().push(PeerLink {
                peer_host_session_id: peer.to_owned(),
                peer_title: titles.get(peer).or_else(|| clean(note.title.as_deref())),
                sent_at: note.at,
                delivered_at: None,
                read_at: None,
            });
        }
    }
    for (file_key, sender) in desktop {
        let sender_id =
            non_blank(Some(sender.host_session_id.as_str())).unwrap_or(file_key.trim());
        for receipt in &sender.peer_receipts {
            let target = receipt.target_session_id.trim();
            if target.is_empty()
                || target == sender_id
                || receipt.read_at.is_some()
                || receipt.sent_at < cutoff
            {
                continue;
            }
            inbound
                .entry(target)
                .or_default()
                .push(peer_link(sender_id, receipt, titles));
        }
    }
    for links in inbound.values_mut() {
        sort_links(links);
    }
    inbound
}

/// Newest first; the other fields only make the order deterministic.
fn sort_links(links: &mut [PeerLink]) {
    links.sort_by(|a, b| {
        b.sent_at
            .cmp(&a.sent_at)
            .then_with(|| a.peer_host_session_id.cmp(&b.peer_host_session_id))
            .then_with(|| a.delivered_at.cmp(&b.delivered_at))
            .then_with(|| a.read_at.cmp(&b.read_at))
    });
}

// ---------------------------------------------------------------------------
// Order and counts
// ---------------------------------------------------------------------------

/// Waiting (longest wait first), Error, Unseen (newest first), Working
/// (newest first), Idle (most recent activity first); then title, then key.
fn sort_sessions(sessions: &mut [SessionView]) {
    sessions.sort_by(|a, b| {
        state_rank(a.state)
            .cmp(&state_rank(b.state))
            .then_with(|| within_state(a, b))
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.key.cmp(&b.key))
    });
}

fn state_rank(state: SessionState) -> u8 {
    match state {
        SessionState::Waiting => 0,
        SessionState::Error => 1,
        SessionState::Unseen => 2,
        SessionState::Working => 3,
        SessionState::Idle => 4,
    }
}

/// Order inside one state group (`a` and `b` share their state).
fn within_state(a: &SessionView, b: &SessionView) -> Ordering {
    match a.state {
        SessionState::Waiting => oldest_first(a.state_since, b.state_since),
        SessionState::Error | SessionState::Unseen | SessionState::Working => {
            newest_first(a.state_since, b.state_since)
        }
        SessionState::Idle => newest_first(a.last_activity_at, b.last_activity_at),
    }
}

/// Ascending, unknown times last.
fn oldest_first(a: Option<i64>, b: Option<i64>) -> Ordering {
    (a.is_none(), a).cmp(&(b.is_none(), b))
}

/// Descending, unknown times last.
fn newest_first(a: Option<i64>, b: Option<i64>) -> Ordering {
    Reverse(a).cmp(&Reverse(b))
}

fn bump(counter: &mut u32, by: usize) {
    *counter = counter.saturating_add(u32::try_from(by).unwrap_or(u32::MAX));
}

fn count_sessions(sessions: &[SessionView]) -> Counts {
    let mut counts = Counts::default();
    for session in sessions {
        bump(&mut counts.total, 1);
        let per_state = match session.state {
            SessionState::Waiting => &mut counts.waiting,
            SessionState::Error => &mut counts.error,
            SessionState::Unseen => &mut counts.unseen,
            SessionState::Working => &mut counts.working,
            SessionState::Idle => &mut counts.idle,
        };
        bump(per_state, 1);
        if let Some(pr) = &session.pr
            && pr.state.trim().eq_ignore_ascii_case("OPEN")
        {
            bump(&mut counts.pr_open, 1);
            if pr.checks == ChecksState::Fail {
                bump(&mut counts.pr_failing, 1);
            }
        }
        bump(
            &mut counts.messages_pending,
            session.messages.awaiting_reply.len(),
        );
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        DesktopPrBinding, FindingsReport, PlanItem, PlanProgress, SideNote, TaskSuggestion,
        TurnInfo, TurnSummary,
    };
    use SessionState::{Error, Idle, Unseen, Waiting, Working};

    const NOW: i64 = 1_800_000_000_000;
    const MIN: i64 = 60_000;
    const HOUR: i64 = 3_600_000;
    const REPO_CWD: &str = r"D:\Données Utilisateur\Documents\GitHub\atlas-web";
    const WORKTREE_CWD: &str = r"D:\Données Utilisateur\Documents\GitHub\Carnet\.claude\worktrees\affectionate-lalande-433900";

    /// A live, named, idle Desktop process.
    fn entry(pid: u32, host: &str) -> RegistryEntry {
        RegistryEntry {
            pid,
            session_id: format!("cli-{pid}"),
            cwd: REPO_CWD.to_owned(),
            started_at: Some(NOW - HOUR),
            version: Some("2.1.308".to_owned()),
            kind: Some("interactive".to_owned()),
            entrypoint: Some("claude-desktop".to_owned()),
            host_session_id: Some(host.to_owned()),
            name: Some(format!("Session {pid}")),
            status: Some("idle".to_owned()),
            updated_at: Some(NOW - 30 * MIN),
            status_updated_at: Some(NOW - 30 * MIN),
            ..Default::default()
        }
    }

    fn with_status(mut e: RegistryEntry, status: &str) -> RegistryEntry {
        e.status = Some(status.to_owned());
        e
    }

    /// A live Desktop process without a name (ghost candidate).
    fn unnamed(pid: u32, host: &str) -> RegistryEntry {
        RegistryEntry {
            name: None,
            ..entry(pid, host)
        }
    }

    /// A CLI process outside Desktop.
    fn cli(pid: u32) -> RegistryEntry {
        RegistryEntry {
            entrypoint: Some("cli".to_owned()),
            host_session_id: None,
            ..entry(pid, "")
        }
    }

    /// The Desktop file of a used session, seen after its latest turn.
    fn desk(host: &str) -> DesktopSession {
        DesktopSession {
            host_session_id: host.to_owned(),
            title: Some(format!("Desktop {host}")),
            latest_user_frame_at: Some(NOW - 2 * HOUR),
            completed_turns: Some(1),
            last_turn_edge_at: Some(NOW - 90 * MIN),
            last_focused_at: Some(NOW - 80 * MIN),
            ..Default::default()
        }
    }

    fn receipt(id: &str, target: &str, sent_at: i64) -> PeerReceipt {
        PeerReceipt {
            message_id: id.to_owned(),
            target_session_id: target.to_owned(),
            sent_at,
            delivery: Some("delivered".to_owned()),
            delivered_at: Some(sent_at + 1_000),
            read_at: None,
            replied_at: None,
        }
    }

    fn tool(name: &str, at: i64) -> ToolActivity {
        ToolActivity {
            name: name.to_owned(),
            summary: format!("{name} summary"),
            at: Some(at),
        }
    }

    fn pr(state: &str, checks: ChecksState) -> PrInfo {
        PrInfo {
            number: 7,
            url: "https://github.com/o/r/pull/7".to_owned(),
            state: state.to_owned(),
            checks,
            source: "desktop".to_owned(),
            ..Default::default()
        }
    }

    struct Fixture {
        now: i64,
        registry: Vec<RegistryEntry>,
        desktop: HashMap<String, DesktopSession>,
        transcripts: HashMap<String, TranscriptInfo>,
        agents: HashMap<String, Vec<RunningAgent>>,
        git_pr: HashMap<String, (Option<GitInfo>, Option<PrInfo>)>,
        acks: HashMap<String, i64>,
        turn_ended: HashMap<String, i64>,
        settings: Settings,
        health: SourceHealth,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                now: NOW,
                registry: Vec::new(),
                desktop: HashMap::new(),
                transcripts: HashMap::new(),
                agents: HashMap::new(),
                git_pr: HashMap::new(),
                acks: HashMap::new(),
                turn_ended: HashMap::new(),
                settings: Settings::default(),
                health: SourceHealth {
                    registry_ok: true,
                    desktop_ok: true,
                    ..SourceHealth::default()
                },
            }
        }

        fn live(&mut self, e: RegistryEntry) -> &mut Self {
            self.registry.push(e);
            self
        }

        fn file(&mut self, d: DesktopSession) -> &mut Self {
            self.desktop.insert(d.host_session_id.clone(), d);
            self
        }

        fn snapshot(&self) -> Snapshot {
            build_snapshot(&AggregateInput {
                now_ms: self.now,
                registry: &self.registry,
                desktop: &self.desktop,
                transcripts: &self.transcripts,
                agents: &self.agents,
                git_pr: &self.git_pr,
                acks: &self.acks,
                on_screen: &HashSet::new(),
                turn_ended_at: &self.turn_ended,
                settings: &self.settings,
                health: self.health.clone(),
            })
        }

        fn view(&self, key: &str) -> SessionView {
            self.snapshot()
                .sessions
                .into_iter()
                .find(|s| s.key == key)
                .unwrap_or_else(|| panic!("session {key} missing"))
        }

        fn keys(&self) -> Vec<String> {
            self.snapshot().sessions.into_iter().map(|s| s.key).collect()
        }

        fn sorted_keys(&self) -> Vec<String> {
            let mut keys = self.keys();
            keys.sort();
            keys
        }
    }

    // --- session_key -------------------------------------------------------

    #[test]
    fn session_key_uses_host_id() {
        assert_eq!(session_key(&entry(1, "local_a")), "local_a");
        assert_eq!(session_key(&entry(1, "  local_a ")), "local_a");
    }

    #[test]
    fn session_key_falls_back_to_cli_id() {
        let mut e = entry(1, "x");
        for host in [None, Some(String::new()), Some("   ".to_owned())] {
            e.host_session_id = host;
            assert_eq!(session_key(&e), "cli:cli-1");
        }
    }

    // --- derive_state ------------------------------------------------------

    /// Registry status of the session, everything else empty.
    fn input(status: &str) -> StateInput<'_> {
        StateInput {
            now_ms: NOW,
            status: Some(status),
            status_since: Some(NOW - 30 * MIN),
            ..StateInput::default()
        }
    }

    /// A transcript whose latest turn, started by `origin` at `started`,
    /// ended at `ended` (still running when None).
    fn turn(origin: TurnOrigin, started: i64, ended: Option<i64>) -> TranscriptInfo {
        TranscriptInfo {
            turn: Some(TurnInfo {
                started_at: Some(started),
                origin,
                ended: ended.is_some(),
                ended_at: ended,
                ..TurnInfo::default()
            }),
            last_human_prompt_at: (origin == TurnOrigin::Human).then_some(started),
            notable_turn_ended_at: if origin == TurnOrigin::Peer { None } else { ended },
            ..TranscriptInfo::default()
        }
    }

    /// The question of a `wrap_up` at `at`, with two buttons.
    fn ask(at: i64) -> PendingQuestion {
        PendingQuestion {
            text: "Pousser feat/6-lot et ouvrir la PR ?".to_owned(),
            options: vec!["Pousser et ouvrir".to_owned(), "Pas encore".to_owned()],
            source: "wrap_up".to_owned(),
            at: Some(at),
        }
    }

    /// A human turn from NOW-20min to NOW-10min that ended on `ask`.
    fn asked() -> TranscriptInfo {
        TranscriptInfo {
            pending_ask: Some(ask(NOW - 10 * MIN - 5_000)),
            ..turn(TurnOrigin::Human, NOW - 20 * MIN, Some(NOW - 10 * MIN))
        }
    }

    fn error_at(at: i64, label: &str) -> Option<ErrorMark> {
        Some(ErrorMark {
            at: Some(at),
            label: Some(label.to_owned()),
        })
    }

    fn verdict(category: &str, needs: Option<&str>) -> Option<TurnSummary> {
        Some(TurnSummary {
            category: Some(category.to_owned()),
            detail: Some("PR #311 ouverte".to_owned()),
            needs_action: needs.map(str::to_owned),
            fresh: true,
        })
    }

    #[test]
    fn state_from_raw_status() {
        let state = |status: Option<&str>| {
            derive_state(&StateInput {
                status,
                ..input("")
            })
            .state
        };
        assert_eq!(state(Some("waiting")), Waiting);
        assert_eq!(state(Some(" Busy ")), Working);
        assert_eq!(state(Some("WAITING")), Waiting);
        assert_eq!(state(Some("idle")), Idle);
        assert_eq!(state(Some("hibernating")), Idle);
        assert_eq!(state(Some("")), Idle);
        assert_eq!(state(None), Idle);
        let empty = DesktopSession::default();
        let with_file = StateInput {
            desktop: Some(&empty),
            ..input("idle")
        };
        assert_eq!(derive_state(&with_file).state, Idle);
        let acked = StateInput {
            acked_at: Some(NOW),
            ..input("idle")
        };
        assert_eq!(derive_state(&acked).state, Idle);
    }

    #[test]
    fn registry_waiting_keeps_priority_and_its_reason() {
        let transcript = asked();
        let d = derive_state(&StateInput {
            registry_waiting_for: Some(" permission prompt "),
            transcript: Some(&transcript),
            ..input("waiting")
        });
        assert_eq!(d.state, Waiting);
        assert_eq!(d.waiting_for.as_deref(), Some("permission prompt"));
        assert_eq!(d.question, None);
        assert_eq!(d.since, Some(NOW - 30 * MIN));
    }

    #[test]
    fn busy_wins_over_error_while_a_turn_runs() {
        let d = DesktopSession {
            error: error_at(NOW - MIN, "auth_error: expired"),
            last_turn_edge_at: Some(NOW - MIN),
            ..Default::default()
        };
        let running = turn(TurnOrigin::Human, NOW - 2 * HOUR, None);
        let busy = StateInput {
            desktop: Some(&d),
            transcript: Some(&running),
            ..input("busy")
        };
        assert_eq!(derive_state(&busy).state, Working);
        let idle = StateInput {
            status: Some("idle"),
            ..busy
        };
        assert_eq!(derive_state(&idle).state, Error);
        let unknown = StateInput {
            status: Some("mystery"),
            ..busy
        };
        assert_eq!(derive_state(&unknown).state, Error);
    }

    // --- decisions ---------------------------------------------------------

    #[test]
    fn wrap_up_ask_waits_for_a_decision() {
        let transcript = asked();
        let d = derive_state(&StateInput {
            transcript: Some(&transcript),
            ..input("idle")
        });
        assert_eq!(d.state, Waiting);
        assert_eq!(d.waiting_for.as_deref(), Some(WAITING_DECISION));
        assert_eq!(d.question, Some(ask(NOW - 10 * MIN - 5_000)));
        assert_eq!(d.since, Some(NOW - 10 * MIN - 5_000), "since the question");
    }

    #[test]
    fn focus_ack_and_screen_do_not_clear_a_decision() {
        let transcript = asked();
        let desktop = DesktopSession {
            last_focused_at: Some(NOW),
            last_turn_edge_at: Some(NOW - 10 * MIN),
            latest_user_frame_at: Some(NOW - 20 * MIN),
            ..Default::default()
        };
        let d = derive_state(&StateInput {
            desktop: Some(&desktop),
            transcript: Some(&transcript),
            acked_at: Some(NOW),
            ..input("idle")
        });
        assert_eq!(d.state, Waiting);
        assert_eq!(d.waiting_for.as_deref(), Some(WAITING_DECISION));
    }

    #[test]
    fn user_answered_the_decision() {
        // The transcript shows the answer: the transcript clears the ask.
        let answered = turn(TurnOrigin::Human, NOW - 5 * MIN, Some(NOW - 4 * MIN));
        let d = derive_state(&StateInput {
            transcript: Some(&answered),
            ..input("idle")
        });
        assert_eq!(d.question, None);
        // Desktop has the answer, the transcript not yet.
        let transcript = asked();
        let desktop = DesktopSession {
            last_turn_edge_at: Some(NOW - 10 * MIN),
            latest_user_frame_at: Some(NOW - MIN),
            ..Default::default()
        };
        let d = derive_state(&StateInput {
            desktop: Some(&desktop),
            transcript: Some(&transcript),
            ..input("idle")
        });
        assert_eq!(d.question, None);
        assert_eq!(d.state, Unseen);
        // A frame the transcript explains (a turn another session started
        // after the question) does not hide it.
        let woken = TranscriptInfo {
            pending_ask: Some(ask(NOW - 10 * MIN - 5_000)),
            last_human_prompt_at: Some(NOW - 20 * MIN),
            notable_turn_ended_at: Some(NOW - 10 * MIN),
            ..turn(TurnOrigin::Peer, NOW - MIN, Some(NOW - MIN + 5_000))
        };
        let d = derive_state(&StateInput {
            desktop: Some(&desktop),
            transcript: Some(&woken),
            ..input("idle")
        });
        assert_eq!(d.state, Waiting);
        assert_eq!(
            d.question.map(|q| q.text).as_deref(),
            Some("Pousser feat/6-lot et ouvrir la PR ?")
        );
    }

    #[test]
    fn new_turn_hides_the_decision_while_it_runs() {
        // The person answered: the registry went busy after the turn ended,
        // before the transcript shows the new prompt.
        let transcript = asked();
        let answered = StateInput {
            transcript: Some(&transcript),
            status_since: Some(NOW - MIN),
            ..input("busy")
        };
        assert_eq!(derive_state(&answered).state, Working);
        // A turn another session started is running.
        let woken = TranscriptInfo {
            pending_ask: Some(ask(NOW - 10 * MIN - 5_000)),
            ..turn(TurnOrigin::Peer, NOW - MIN, None)
        };
        let running = StateInput {
            transcript: Some(&woken),
            status_since: Some(NOW - MIN),
            ..input("busy")
        };
        assert_eq!(derive_state(&running).state, Working);
        // Once it ends without a question, the decision is back.
        let ended = TranscriptInfo {
            pending_ask: Some(ask(NOW - 10 * MIN - 5_000)),
            ..turn(TurnOrigin::Peer, NOW - MIN, Some(NOW - MIN + 7_000))
        };
        let d = derive_state(&StateInput {
            transcript: Some(&ended),
            ..input("idle")
        });
        assert_eq!(d.state, Waiting);
        assert_eq!(d.since, Some(NOW - 10 * MIN - 5_000), "same episode");
    }

    // --- background work ---------------------------------------------------

    fn task(kind: BackgroundKind, summary: &str, started_at: i64) -> BackgroundTask {
        BackgroundTask {
            id: format!("task-{summary}"),
            kind,
            summary: summary.to_owned(),
            started_at: Some(started_at),
            ends_at: None,
        }
    }

    /// A turn that ended ten minutes ago, leaving `tasks` behind.
    fn left_running(tasks: Vec<BackgroundTask>) -> TranscriptInfo {
        TranscriptInfo {
            background: tasks,
            ..turn(TurnOrigin::Human, NOW - 20 * MIN, Some(NOW - 10 * MIN))
        }
    }

    fn after_turn(transcript: &TranscriptInfo) -> StateInput<'_> {
        StateInput {
            transcript: Some(transcript),
            status_since: Some(NOW - 10 * MIN),
            ..input("idle")
        }
    }

    #[test]
    fn work_left_running_keeps_an_idle_session_working() {
        let transcript = left_running(vec![task(BackgroundKind::Command, "cargo test", NOW - 11 * MIN)]);
        let desktop = DesktopSession {
            last_turn_edge_at: Some(NOW - 10 * MIN),
            last_focused_at: Some(NOW - 5 * MIN),
            ..desk("local_a")
        };
        let d = derive_state(&StateInput {
            desktop: Some(&desktop),
            ..after_turn(&transcript)
        });
        assert_eq!(d.state, Working);
        assert_eq!(d.since, Some(NOW - 20 * MIN), "the turn's work goes on");
        assert_eq!(
            d.background,
            Some(BackgroundWait {
                kind: BackgroundKind::Command,
                summary: "cargo test".to_owned(),
                count: 1,
                ends_at: None,
            })
        );

        // Once it ended, the session is what it was.
        let over = left_running(Vec::new());
        let d = derive_state(&StateInput {
            desktop: Some(&desktop),
            ..after_turn(&over)
        });
        assert_eq!(d.state, Idle);
        assert_eq!(d.background, None);
    }

    #[test]
    fn work_left_running_does_not_hide_what_needs_the_person() {
        let tasks = vec![task(BackgroundKind::Workflow, "audit", NOW - 11 * MIN)];
        let mut transcript = asked();
        transcript.background = tasks.clone();
        let d = derive_state(&after_turn(&transcript));
        assert_eq!(d.state, Waiting);
        assert_eq!(d.background, None);

        let failed = TranscriptInfo {
            api_error: Some(ErrorMark {
                at: Some(NOW - 10 * MIN),
                label: Some("rate_limit".to_owned()),
            }),
            ..left_running(tasks)
        };
        let d = derive_state(&after_turn(&failed));
        assert_eq!(d.state, Error);
        assert_eq!(d.background, None);
    }

    #[test]
    fn work_dies_with_its_process() {
        let transcript = left_running(vec![task(BackgroundKind::Agent, "relecture", NOW - 11 * MIN)]);
        let same_process = StateInput {
            process_started_at: Some(NOW - HOUR),
            ..after_turn(&transcript)
        };
        assert_eq!(derive_state(&same_process).state, Working);
        let restarted = StateInput {
            process_started_at: Some(NOW - 5 * MIN),
            ..same_process
        };
        assert_ne!(derive_state(&restarted).state, Working);
        let stopped = StateInput {
            stopped: true,
            ..same_process
        };
        assert_ne!(derive_state(&stopped).state, Working);
    }

    #[test]
    fn work_is_waited_for_within_limits() {
        let state = |task: BackgroundTask| {
            let transcript = left_running(vec![task]);
            derive_state(&after_turn(&transcript)).state
        };
        // A command that long is a server left running.
        assert_eq!(state(task(BackgroundKind::Command, "dev", NOW - 119 * MIN)), Working);
        assert_ne!(state(task(BackgroundKind::Command, "dev", NOW - 121 * MIN)), Working);
        // A workflow can take most of a day.
        assert_eq!(state(task(BackgroundKind::Workflow, "audit", NOW - 22 * HOUR)), Working);
        assert_ne!(state(task(BackgroundKind::Workflow, "audit", NOW - 25 * HOUR)), Working);
        // Work with an end of its own stops a little after it.
        let timed = |ends_at: i64| BackgroundTask {
            ends_at: Some(ends_at),
            ..task(BackgroundKind::Wakeup, "CI", NOW - 11 * MIN)
        };
        assert_eq!(state(timed(NOW + 15 * MIN)), Working);
        assert_eq!(state(timed(NOW - MIN)), Working);
        assert_ne!(state(timed(NOW - 3 * MIN)), Working);
    }

    #[test]
    fn the_work_named_is_the_one_doing_it() {
        let transcript = left_running(vec![
            task(BackgroundKind::Command, "cargo test", NOW - 12 * MIN),
            task(BackgroundKind::Agent, "relecture", NOW - 11 * MIN),
            task(BackgroundKind::Monitor, "logs", NOW - 10 * MIN),
        ]);
        let wait = derive_state(&after_turn(&transcript)).background.expect("wait");
        assert_eq!((wait.kind, wait.summary.as_str(), wait.count), (BackgroundKind::Agent, "relecture", 3));
        let watching = left_running(vec![task(BackgroundKind::Monitor, "logs", NOW - 10 * MIN)]);
        let wait = derive_state(&after_turn(&watching)).background.expect("wait");
        assert_eq!((wait.kind, wait.count), (BackgroundKind::Monitor, 1));
    }

    #[test]
    fn a_session_waits_for_the_side_sessions_it_started() {
        let side = |host: &str, parent: &str| DesktopSession {
            spawned_from: Some(SessionRef {
                host_session_id: parent.to_owned(),
                title: None,
            }),
            side_session: true,
            ..desk(host)
        };
        let state_of = |f: &Fixture, key: &str| {
            let snapshot = f.snapshot();
            let view = snapshot.sessions.iter().find(|s| s.key == key).expect("listed");
            (view.state, view.background.clone())
        };
        let mut f = Fixture::new();
        f.live(entry(1, "local_root"))
            .file(desk("local_root"))
            .live(entry(2, "local_mid"))
            .file(side("local_mid", "local_root"))
            .live(with_status(entry(3, "local_leaf"), "busy"))
            .file(side("local_leaf", "local_mid"));

        // Up the whole chain: the leaf works, so the middle waits, so the root.
        let (state, wait) = state_of(&f, "local_mid");
        assert_eq!(state, Working);
        let wait = wait.expect("wait");
        assert_eq!((wait.kind, wait.summary.as_str(), wait.count), (BackgroundKind::Sessions, "Desktop local_leaf", 1));
        assert_eq!(state_of(&f, "local_root").0, Working);
        assert_eq!(f.snapshot().counts.working, 3);

        // A side session waiting for the person holds nobody up.
        f.registry[2] = with_status(entry(3, "local_leaf"), "waiting");
        assert_eq!(state_of(&f, "local_mid"), (Idle, None));
        assert_eq!(state_of(&f, "local_root"), (Idle, None));

        // A session detached from its parent, or started from a "New task"
        // card, reports to nobody.
        f.registry[2] = with_status(entry(3, "local_leaf"), "busy");
        f.file(DesktopSession {
            lineage_detached: true,
            ..side("local_leaf", "local_mid")
        });
        assert_eq!(state_of(&f, "local_mid").0, Idle);
        f.file(DesktopSession {
            side_session: false,
            ..side("local_leaf", "local_mid")
        });
        assert_eq!(state_of(&f, "local_mid").0, Idle);
    }

    #[test]
    fn a_snapshot_counts_a_session_waiting_for_its_work_as_working() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_a")).file(desk("local_a"));
        f.transcripts.insert(
            "cli-1".to_owned(),
            left_running(vec![task(BackgroundKind::Command, "cargo test", NOW - 11 * MIN)]),
        );
        let snapshot = f.snapshot();
        assert_eq!(snapshot.counts.working, 1);
        assert_eq!(snapshot.sessions[0].state, Working);
        assert_eq!(
            snapshot.sessions[0].background.as_ref().map(|w| w.summary.as_str()),
            Some("cargo test")
        );

        // The process that ran it is gone, a new one took the same transcript.
        f.registry[0].started_at = Some(NOW - MIN);
        assert_eq!(f.snapshot().sessions[0].state, Idle);
    }

    #[test]
    fn background_task_keeping_the_registry_busy_does_not_hide_a_decision() {
        // Busy since the turn started; the transcript ended the turn after.
        let transcript = asked();
        let background = StateInput {
            transcript: Some(&transcript),
            status_since: Some(NOW - 20 * MIN),
            ..input("busy")
        };
        let d = derive_state(&background);
        assert_eq!(d.state, Waiting);
        assert_eq!(d.waiting_for.as_deref(), Some(WAITING_DECISION));
        // Without a question it stays Working: the background work is real.
        let quiet = turn(TurnOrigin::Human, NOW - 20 * MIN, Some(NOW - 10 * MIN));
        let d = derive_state(&StateInput {
            transcript: Some(&quiet),
            ..background
        });
        assert_eq!(d.state, Working);
        assert_eq!(d.since, Some(NOW - 20 * MIN));
        // A frame Desktop received after the end: a new turn is starting.
        let desktop = DesktopSession {
            latest_user_frame_at: Some(NOW - MIN),
            ..Default::default()
        };
        let d = derive_state(&StateInput {
            desktop: Some(&desktop),
            ..background
        });
        assert_eq!(d.state, Working);
    }

    #[test]
    fn decision_does_not_fire_while_the_turn_runs() {
        // wrap_up was called but the turn has not ended yet.
        let running = TranscriptInfo {
            pending_ask: Some(ask(NOW - MIN)),
            ..turn(TurnOrigin::Human, NOW - 20 * MIN, None)
        };
        let d = derive_state(&StateInput {
            transcript: Some(&running),
            status_since: Some(NOW - 20 * MIN),
            ..input("busy")
        });
        assert_eq!(d.state, Working);
        // No transcript at all: busy means working.
        assert_eq!(derive_state(&input("busy")).state, Working);
    }

    #[test]
    fn desktop_blocked_verdict_waits_for_a_decision() {
        let desktop = DesktopSession {
            turn_summary: verdict("blocked", Some("ta **décision** sur le ménage")),
            last_turn_edge_at: Some(NOW - 5 * MIN),
            latest_user_frame_at: Some(NOW - 9 * MIN),
            last_focused_at: Some(NOW),
            ..Default::default()
        };
        let d = derive_state(&StateInput {
            desktop: Some(&desktop),
            acked_at: Some(NOW),
            ..input("idle")
        });
        assert_eq!(d.state, Waiting);
        assert_eq!(d.waiting_for.as_deref(), Some(WAITING_DECISION));
        let q = d.question.expect("question");
        assert_eq!(q.text, "ta décision sur le ménage");
        assert_eq!((q.source.as_str(), q.at), ("desktop", Some(NOW - 5 * MIN)));
        // need_input, or something to do, count too; review_ready does not.
        let state = |summary: Option<TurnSummary>| {
            let d = DesktopSession {
                turn_summary: summary,
                ..desktop.clone()
            };
            derive_state(&StateInput {
                desktop: Some(&d),
                ..input("idle")
            })
            .state
        };
        assert_eq!(state(verdict("need_input", None)), Waiting);
        assert_eq!(state(verdict("completed", Some("lancer la migration"))), Waiting);
        assert_eq!(state(verdict("review_ready", None)), Idle);
        assert_eq!(state(verdict("completed", Some("  "))), Idle);
        let stale = verdict("blocked", Some("x")).map(|s| TurnSummary {
            fresh: false,
            ..s
        });
        assert_eq!(state(stale), Idle);
        // A message after the turn: Desktop's verdict is about an older turn.
        let answered = DesktopSession {
            latest_user_frame_at: Some(NOW - MIN),
            ..desktop.clone()
        };
        let d = derive_state(&StateInput {
            desktop: Some(&answered),
            ..input("idle")
        });
        assert_eq!(d.question, None);
        // The transcript already shows a newer turn.
        let newer = turn(TurnOrigin::Peer, NOW - 2 * MIN, Some(NOW - MIN));
        let d = derive_state(&StateInput {
            desktop: Some(&desktop),
            transcript: Some(&newer),
            ..input("idle")
        });
        assert_eq!(d.question, None);
        // Older files have no turn edge: the latest activity stands in, and
        // without any time Desktop's own lifetime applies.
        let old_file = DesktopSession {
            last_turn_edge_at: None,
            last_activity_at: Some(NOW - 5 * MIN),
            ..desktop.clone()
        };
        let d = derive_state(&StateInput {
            desktop: Some(&old_file),
            ..input("idle")
        });
        assert_eq!((d.state, d.since), (Waiting, Some(NOW - 5 * MIN)));
        let no_time = DesktopSession {
            last_activity_at: None,
            latest_user_frame_at: None,
            ..old_file.clone()
        };
        let d = derive_state(&StateInput {
            desktop: Some(&no_time),
            ..input("idle")
        });
        assert_eq!(d.waiting_for.as_deref(), Some(WAITING_DECISION));
        let answered_old = DesktopSession {
            latest_user_frame_at: Some(NOW - MIN),
            ..old_file
        };
        let d = derive_state(&StateInput {
            desktop: Some(&answered_old),
            ..input("idle")
        });
        assert_eq!(d.question, None);
    }

    #[test]
    fn closing_question_mark_is_a_probable_question() {
        let mut transcript = turn(TurnOrigin::Human, NOW - 20 * MIN, Some(NOW - 10 * MIN));
        if let Some(t) = transcript.turn.as_mut() {
            t.closing_question = Some("Je pousse la branche ?".to_owned());
        }
        let d = derive_state(&StateInput {
            transcript: Some(&transcript),
            ..input("idle")
        });
        assert_eq!(d.state, Waiting);
        assert_eq!(d.waiting_for.as_deref(), Some(WAITING_PROBABLE_QUESTION));
        let q = d.question.expect("question");
        assert_eq!(
            (q.text.as_str(), q.source.as_str()),
            ("Je pousse la branche ?", "text")
        );
        assert_eq!(q.at, Some(NOW - 10 * MIN));
        assert!(q.options.is_empty());

        let reason = |t: &TranscriptInfo, desktop: Option<&DesktopSession>, now: i64| {
            derive_state(&StateInput {
                now_ms: now,
                transcript: Some(t),
                desktop,
                ..input("idle")
            })
            .waiting_for
        };
        // A wrap-up without question said nothing waits.
        let mut wrapped = transcript.clone();
        if let Some(t) = wrapped.turn.as_mut() {
            t.wrapped_up = true;
        }
        assert_eq!(reason(&wrapped, None, NOW), None);

        // Desktop writes verdicts: the text waits for its verdict on the turn…
        let classified = DesktopSession {
            classifier_summary_enabled: true,
            last_turn_edge_at: Some(NOW - 10 * MIN),
            latest_user_frame_at: Some(NOW - 20 * MIN),
            ..Default::default()
        };
        let just_ended = NOW - 10 * MIN + 5_000;
        assert_eq!(reason(&transcript, Some(&classified), just_ended), None);
        // …which decides when it comes: nothing pending…
        let judged = DesktopSession {
            turn_summary: verdict("completed", None),
            ..classified.clone()
        };
        assert_eq!(reason(&transcript, Some(&judged), NOW), None);
        // …or a decision, in Desktop's words.
        let blocked = DesktopSession {
            turn_summary: verdict("blocked", Some("Je pousse ?")),
            ..classified.clone()
        };
        assert_eq!(
            reason(&transcript, Some(&blocked), NOW).as_deref(),
            Some(WAITING_DECISION)
        );
        // No verdict after the grace delay: the text decides.
        assert_eq!(
            reason(&transcript, Some(&classified), NOW).as_deref(),
            Some(WAITING_PROBABLE_QUESTION)
        );
        // Desktop without verdicts: the text decides at once.
        let unclassified = DesktopSession {
            classifier_summary_enabled: false,
            ..classified
        };
        assert_eq!(
            reason(&transcript, Some(&unclassified), just_ended).as_deref(),
            Some(WAITING_PROBABLE_QUESTION)
        );
        // Desktop got a message the transcript does not show yet.
        let answered = DesktopSession {
            latest_user_frame_at: Some(NOW - MIN),
            ..unclassified
        };
        assert_eq!(reason(&transcript, Some(&answered), NOW), None);
    }

    #[test]
    fn wrap_up_question_wins_over_desktop_and_text() {
        let mut transcript = asked();
        if let Some(t) = transcript.turn.as_mut() {
            t.wrapped_up = true;
            t.closing_question = Some("Autre chose ?".to_owned());
        }
        let desktop = DesktopSession {
            turn_summary: verdict("review_ready", None),
            last_turn_edge_at: Some(NOW - 10 * MIN),
            ..Default::default()
        };
        let d = derive_state(&StateInput {
            desktop: Some(&desktop),
            transcript: Some(&transcript),
            ..input("idle")
        });
        let q = d.question.expect("question");
        assert_eq!(q.source, "wrap_up");
        assert_eq!(q.options, ["Pousser et ouvrir", "Pas encore"]);
    }

    // --- errors ------------------------------------------------------------

    #[test]
    fn error_needs_an_error_after_the_person_s_last_message() {
        let state = |desktop: DesktopSession, transcript: Option<&TranscriptInfo>| {
            derive_state(&StateInput {
                desktop: Some(&desktop),
                transcript,
                acked_at: Some(NOW),
                ..input("idle")
            })
        };
        let base = DesktopSession {
            latest_user_frame_at: Some(NOW - 20 * MIN),
            last_turn_edge_at: Some(NOW - 10 * MIN),
            last_focused_at: Some(NOW),
            ..Default::default()
        };
        // Desktop's own error, acked or not.
        let d = state(
            DesktopSession {
                error: error_at(NOW - 10 * MIN, "auth_error: Your session credentials expired."),
                ..base.clone()
            },
            None,
        );
        assert_eq!(d.state, Error);
        assert_eq!(
            d.error.as_deref(),
            Some("auth_error: Your session credentials expired.")
        );
        assert_eq!(d.since, Some(NOW - 10 * MIN));
        // Older than the person's message: answered.
        let old = DesktopSession {
            error: error_at(NOW - 30 * MIN, "x"),
            ..base.clone()
        };
        assert_eq!(state(old, None).state, Idle);
        // More than 7 days old: Desktop no longer shows it.
        let stale = DesktopSession {
            error: error_at(NOW - 8 * 24 * HOUR, "x"),
            latest_user_frame_at: Some(NOW - 9 * 24 * HOUR),
            ..base.clone()
        };
        assert_eq!(state(stale, None).state, Idle);
        // Desktop's "failed" verdict.
        let failed = DesktopSession {
            turn_summary: verdict("failed", Some("relancer")),
            ..base.clone()
        };
        let d = state(failed, None);
        assert_eq!((d.state, d.error.as_deref()), (Error, Some("relancer")));
        // A mark of a pending turn; a later turn edge supersedes it.
        let mark = DesktopSession {
            prior_error: error_at(NOW - 10 * MIN, "result"),
            ..base.clone()
        };
        assert_eq!(state(mark.clone(), None).state, Error);
        let later_turn = DesktopSession {
            last_turn_edge_at: Some(NOW - MIN),
            ..mark.clone()
        };
        assert_eq!(state(later_turn, None).state, Idle);
        // A crash, the latest error winning.
        let crash = DesktopSession {
            last_crash: error_at(NOW - 9 * MIN, "process_interrupted: exit"),
            ..mark
        };
        let d = state(crash, None);
        assert_eq!(d.error.as_deref(), Some("process_interrupted: exit"));
        // API error found in the transcript.
        let api = TranscriptInfo {
            api_error: error_at(NOW - 10 * MIN, "rate_limit: You have hit your limit"),
            ..turn(TurnOrigin::Human, NOW - 20 * MIN, Some(NOW - 10 * MIN))
        };
        let d = state(base.clone(), Some(&api));
        assert_eq!(
            (d.state, d.error.as_deref()),
            (Error, Some("rate_limit: You have hit your limit"))
        );
        // No error at all.
        assert_eq!(state(base, None).state, Idle);
    }

    #[test]
    fn turns_other_sessions_start_do_not_clear_an_error() {
        let desktop = DesktopSession {
            error: error_at(NOW - 30 * MIN, "auth_error: expired"),
            // Desktop's frame moved for the child's ping…
            latest_user_frame_at: Some(NOW - 5 * MIN),
            last_turn_edge_at: Some(NOW - 4 * MIN),
            ..Default::default()
        };
        // …but the person's last prompt is older than the error.
        let woken = TranscriptInfo {
            last_human_prompt_at: Some(NOW - 40 * MIN),
            notable_turn_ended_at: Some(NOW - 30 * MIN),
            ..turn(TurnOrigin::Peer, NOW - 5 * MIN, Some(NOW - 4 * MIN))
        };
        let state = |t: &TranscriptInfo| {
            derive_state(&StateInput {
                desktop: Some(&desktop),
                transcript: Some(t),
                ..input("idle")
            })
            .state
        };
        assert_eq!(state(&woken), Error);
        // The person wrote since: cleared.
        let answered = turn(TurnOrigin::Human, NOW - 5 * MIN, Some(NOW - 4 * MIN));
        assert_eq!(state(&answered), Unseen);
    }

    #[test]
    fn a_later_clean_turn_supersedes_a_crash() {
        let desktop = DesktopSession {
            last_crash: error_at(NOW - 60 * MIN, "process_interrupted: exit"),
            prior_error: error_at(NOW - 61 * MIN, "result"),
            latest_user_frame_at: Some(NOW - 90 * MIN),
            last_turn_edge_at: Some(NOW - 10 * MIN),
            last_focused_at: Some(NOW),
            ..Default::default()
        };
        let state = |t: &TranscriptInfo| {
            derive_state(&StateInput {
                desktop: Some(&desktop),
                transcript: Some(t),
                ..input("idle")
            })
        };
        // A peer turn after the crash ended normally: the session runs again.
        let resumed = TranscriptInfo {
            last_human_prompt_at: Some(NOW - 90 * MIN),
            ..turn(TurnOrigin::Peer, NOW - 20 * MIN, Some(NOW - 10 * MIN))
        };
        assert_eq!(state(&resumed).state, Idle);
        // That turn ended on an API error: still an error (the latest wins).
        let failed_again = TranscriptInfo {
            api_error: error_at(NOW - 10 * MIN, "rate_limit: x"),
            ..resumed.clone()
        };
        let d = state(&failed_again);
        assert_eq!((d.state, d.error.as_deref()), (Error, Some("rate_limit: x")));
        // The turn before the crash: the crash stands.
        let before = TranscriptInfo {
            last_human_prompt_at: Some(NOW - 90 * MIN),
            ..turn(TurnOrigin::Human, NOW - 90 * MIN, Some(NOW - 80 * MIN))
        };
        let d = state(&before);
        assert_eq!((d.state, d.error.as_deref()), (Error, Some("process_interrupted: exit")));
    }

    #[test]
    fn a_frame_the_transcript_moved_past_does_not_hide_a_decision() {
        // Desktop's frame moved for a task notification queued mid-turn,
        // before the ask; the transcript went on after it.
        let transcript = TranscriptInfo {
            last_event_at: Some(NOW - 10 * MIN),
            ..asked()
        };
        let desktop = DesktopSession {
            last_turn_edge_at: Some(NOW - 10 * MIN),
            latest_user_frame_at: Some(NOW - 10 * MIN - 2_000),
            ..Default::default()
        };
        let state = |t: &TranscriptInfo| {
            derive_state(&StateInput {
                desktop: Some(&desktop),
                transcript: Some(t),
                ..input("idle")
            })
            .state
        };
        assert_eq!(state(&transcript), Waiting);
        // A frame newer than everything the transcript holds: maybe an answer.
        let later = DesktopSession {
            latest_user_frame_at: Some(NOW - MIN),
            ..desktop.clone()
        };
        let d = derive_state(&StateInput {
            desktop: Some(&later),
            transcript: Some(&transcript),
            ..input("idle")
        });
        assert_eq!(d.question, None);
    }

    #[test]
    fn text_question_does_not_wait_forever_for_an_unknown_end() {
        // No stop marker, no engine edge, Desktop writes verdicts but has none.
        let mut transcript = turn(TurnOrigin::Human, NOW - 20 * MIN, None);
        if let Some(t) = transcript.turn.as_mut() {
            t.closing_question = Some("Je pousse ?".to_owned());
        }
        let desktop = |edge: Option<i64>| DesktopSession {
            classifier_summary_enabled: true,
            last_turn_edge_at: edge,
            latest_user_frame_at: Some(NOW - 20 * MIN),
            ..Default::default()
        };
        let reason = |d: &DesktopSession| {
            derive_state(&StateInput {
                desktop: Some(d),
                transcript: Some(&transcript),
                ..input("idle")
            })
            .waiting_for
        };
        // Desktop's edge is old: past the grace delay.
        assert_eq!(
            reason(&desktop(Some(NOW - 10 * MIN))).as_deref(),
            Some(WAITING_PROBABLE_QUESTION)
        );
        // Desktop's edge just moved: wait for its verdict.
        assert_eq!(reason(&desktop(Some(NOW - 2_000))), None);
        // Nothing known at all: no wait.
        assert_eq!(
            reason(&desktop(None)).as_deref(),
            Some(WAITING_PROBABLE_QUESTION)
        );
    }

    // --- finished, not seen ------------------------------------------------

    #[test]
    fn unseen_compares_turn_end_with_focus_and_ack() {
        let state = |edge: Option<i64>, focus: Option<i64>, ack: Option<i64>| {
            let desktop = DesktopSession {
                last_turn_edge_at: edge,
                last_focused_at: focus,
                ..Default::default()
            };
            derive_state(&StateInput {
                desktop: Some(&desktop),
                acked_at: ack,
                ..input("idle")
            })
            .state
        };
        assert_eq!(state(Some(200), None, None), Unseen);
        assert_eq!(state(Some(200), Some(100), None), Unseen);
        // Equal timestamps count as seen.
        assert_eq!(state(Some(200), Some(200), None), Idle);
        assert_eq!(state(Some(200), None, Some(200)), Idle);
        // Focus newer than the turn end.
        assert_eq!(state(Some(200), Some(300), None), Idle);
        // Ack newer than the turn end, focus older.
        assert_eq!(state(Some(200), Some(100), Some(250)), Idle);
        // Focus newer, ack older.
        assert_eq!(state(Some(200), Some(250), Some(100)), Idle);
        // Both older.
        assert_eq!(state(Some(200), Some(100), Some(150)), Unseen);
        // No turn ended yet.
        assert_eq!(state(None, None, None), Idle);
        assert_eq!(state(None, Some(100), Some(1)), Idle);
    }

    #[test]
    fn engine_turn_end_beats_desktop_s_late_edge() {
        // Desktop still holds the turn start (seen thanks to the ack); the
        // engine saw the registry leave busy after the ack.
        let desktop = DesktopSession {
            last_turn_edge_at: Some(NOW - 10 * MIN),
            last_focused_at: Some(NOW - HOUR),
            ..Default::default()
        };
        let at = |turn_ended_at: Option<i64>| {
            derive_state(&StateInput {
                desktop: Some(&desktop),
                acked_at: Some(NOW - 5 * MIN),
                turn_ended_at,
                ..input("idle")
            })
        };
        assert_eq!(at(None).state, Idle);
        let d = at(Some(NOW - 2_000));
        assert_eq!((d.state, d.since), (Unseen, Some(NOW - 2_000)));
        // An older engine edge changes nothing.
        assert_eq!(at(Some(NOW - 20 * MIN)).state, Idle);
    }

    #[test]
    fn turns_other_sessions_start_do_not_make_a_session_unseen() {
        let desktop = DesktopSession {
            last_turn_edge_at: Some(NOW - MIN),
            last_focused_at: Some(NOW - 30 * MIN),
            ..Default::default()
        };
        let state = |t: &TranscriptInfo| {
            derive_state(&StateInput {
                desktop: Some(&desktop),
                transcript: Some(t),
                ..input("idle")
            })
        };
        // A child's ping woke it and it answered nothing for the person; the
        // person's own turn ended before the focus.
        let ping = TranscriptInfo {
            notable_turn_ended_at: Some(NOW - 40 * MIN),
            last_human_prompt_at: Some(NOW - 50 * MIN),
            ..turn(TurnOrigin::Peer, NOW - 2 * MIN, Some(NOW - MIN))
        };
        let d = state(&ping);
        assert_eq!((d.state, d.since), (Idle, Some(NOW - MIN)));
        // The person's turn ended after the focus: still unseen.
        let unseen = TranscriptInfo {
            notable_turn_ended_at: Some(NOW - 20 * MIN),
            ..ping.clone()
        };
        let d = state(&unseen);
        assert_eq!((d.state, d.since), (Unseen, Some(NOW - 20 * MIN)));
        // The session's own background task finishing counts.
        let task = turn(TurnOrigin::Task, NOW - 2 * MIN, Some(NOW - MIN));
        assert_eq!(state(&task).state, Unseen);
        // A ping that ended on a question waits for the person.
        let asking = TranscriptInfo {
            pending_ask: Some(ask(NOW - MIN)),
            ..ping
        };
        assert_eq!(state(&asking).state, Waiting);
    }

    // --- candidates --------------------------------------------------------

    #[test]
    fn empty_input_gives_empty_snapshot() {
        let mut f = Fixture::new();
        f.health = SourceHealth {
            registry_ok: true,
            desktop_ok: false,
            gh_ok: Some(true),
            errors: vec!["Desktop illisible".to_owned()],
        };
        let s = f.snapshot();
        assert!(s.sessions.is_empty());
        assert_eq!(s.counts, Counts::default());
        assert_eq!(s.generated_at, NOW);
        assert_eq!(s.health, f.health);
    }

    #[test]
    fn duplicate_keys_keep_the_latest_process() {
        let mut f = Fixture::new();
        f.live(RegistryEntry {
            started_at: Some(NOW - 2 * HOUR),
            name: Some("ancien".to_owned()),
            ..entry(10, "local_a")
        })
        .live(RegistryEntry {
            started_at: Some(NOW - HOUR),
            name: Some("nouveau".to_owned()),
            ..entry(5, "local_a")
        })
        .live(RegistryEntry {
            started_at: None,
            name: Some("inconnu".to_owned()),
            ..entry(99, "local_a")
        });
        let s = f.snapshot();
        assert_eq!(s.sessions.len(), 1);
        assert_eq!(s.sessions[0].pid, 5);
        assert_eq!(s.sessions[0].title, "nouveau");
        assert_eq!(s.sessions[0].cli_session_id, "cli-5");
    }

    #[test]
    fn duplicate_keys_prefer_the_process_desktop_links_to() {
        let mut f = Fixture::new();
        f.live(RegistryEntry {
            started_at: Some(NOW - 2 * HOUR),
            name: Some("liée".to_owned()),
            ..entry(10, "local_a")
        })
        .live(RegistryEntry {
            started_at: Some(NOW - HOUR),
            name: Some("plus récente".to_owned()),
            ..entry(20, "local_a")
        })
        .file(DesktopSession {
            cli_session_id: Some("cli-10".to_owned()),
            ..desk("local_a")
        });
        let s = f.snapshot();
        assert_eq!(s.sessions.len(), 1);
        assert_eq!(s.sessions[0].pid, 10);
        assert_eq!(s.sessions[0].cli_session_id, "cli-10");
        // Desktop's title wins over the registry's name.
        assert_eq!(s.sessions[0].title, "Desktop local_a");

        let linked = entry(10, "local_a");
        assert!(entry_rank(&linked, &f.desktop) > entry_rank(&entry(20, "local_a"), &f.desktop));
        assert!(!entry_rank(&linked, &HashMap::new()).0);
    }

    #[test]
    fn duplicate_keys_with_same_start_keep_the_highest_pid() {
        for order in [[10, 20], [20, 10]] {
            let mut f = Fixture::new();
            for pid in order {
                f.live(entry(pid, "local_a"));
            }
            let s = f.snapshot();
            assert_eq!(s.sessions.len(), 1);
            assert_eq!(s.sessions[0].pid, 20);
        }
    }

    #[test]
    fn missing_started_at_falls_back_to_proc_start() {
        let filetime = |ms: i64| ((ms + FILETIME_UNIX_OFFSET_MS) * 10_000).to_string();
        let mut f = Fixture::new();
        f.live(RegistryEntry {
            started_at: Some(NOW - HOUR),
            ..entry(1, "local_a")
        })
        .live(RegistryEntry {
            started_at: None,
            proc_start: Some(filetime(NOW - 10 * MIN)),
            ..entry(2, "local_a")
        })
        .live(RegistryEntry {
            started_at: None,
            proc_start: Some("pas un nombre".to_owned()),
            ..entry(3, "local_a")
        });
        assert_eq!(f.view("local_a").pid, 2);
    }

    #[test]
    fn filetime_conversion_matches_the_registry() {
        // procStart and startedAt of a real-looking registry entry.
        assert_eq!(
            filetime_to_unix_ms("134371474871009047"),
            Some(1_792_673_887_100)
        );
        assert_eq!(filetime_to_unix_ms(" 116444736000000000 "), Some(0));
        assert_eq!(filetime_to_unix_ms("-5"), None);
        assert_eq!(filetime_to_unix_ms(""), None);
        assert_eq!(filetime_to_unix_ms("99999999999999999999999"), None);
        assert!(filetime_to_unix_ms(&u64::MAX.to_string()).is_some());
    }

    #[test]
    fn cli_sessions_need_the_setting() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_a")).live(cli(7)).live(RegistryEntry {
            entrypoint: None,
            ..cli(8)
        });
        assert_eq!(f.keys(), ["local_a"]);

        f.settings.include_cli_sessions = true;
        assert_eq!(f.sorted_keys(), ["cli:cli-7", "cli:cli-8", "local_a"]);
        let view = f.view("cli:cli-7");
        assert!(!view.is_desktop);
        assert_eq!(view.host_session_id, None);
        assert_eq!(view.cli_session_id, "cli-7");
        assert!(f.view("local_a").is_desktop);
    }

    #[test]
    fn desktop_entrypoint_is_matched_loosely() {
        let mut f = Fixture::new();
        f.live(RegistryEntry {
            entrypoint: Some(" Claude-Desktop ".to_owned()),
            ..entry(1, "local_a")
        });
        assert!(f.view("local_a").is_desktop);
    }

    #[test]
    fn archived_sessions_are_hidden() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_a"))
            .file(DesktopSession {
                is_archived: true,
                ..desk("local_a")
            })
            .live(entry(2, "local_b"))
            .file(desk("local_b"));
        assert_eq!(f.keys(), ["local_b"]);
    }

    #[test]
    fn prewarmed_ghosts_are_hidden() {
        let mut f = Fixture::new();
        // No Desktop file, idle, no name.
        f.live(unnamed(1, "local_g1"));
        // Desktop file without prompt nor turn, status missing.
        f.live(RegistryEntry {
            status: None,
            ..unnamed(2, "local_g2")
        })
        .file(DesktopSession {
            host_session_id: "local_g2".to_owned(),
            title: Some("Nouvelle session".to_owned()),
            completed_turns: Some(0),
            ..Default::default()
        });
        // A blank name is no name.
        f.live(RegistryEntry {
            name: Some("   ".to_owned()),
            ..unnamed(3, "local_g3")
        });
        // Desktop file with no turn count at all.
        f.live(unnamed(4, "local_g4")).file(DesktopSession {
            host_session_id: "local_g4".to_owned(),
            ..Default::default()
        });
        assert!(f.snapshot().sessions.is_empty());
    }

    #[test]
    fn real_sessions_are_not_ghosts() {
        let mut f = Fixture::new();
        f.settings.include_cli_sessions = true;
        // A name alone makes it real.
        f.live(entry(1, "local_named"));
        // A user prompt, no completed turn yet.
        f.live(unnamed(2, "local_prompted")).file(DesktopSession {
            host_session_id: "local_prompted".to_owned(),
            latest_user_frame_at: Some(NOW - MIN),
            ..Default::default()
        });
        // Completed turns, no prompt time recorded.
        f.live(unnamed(3, "local_turned")).file(DesktopSession {
            host_session_id: "local_turned".to_owned(),
            completed_turns: Some(2),
            ..Default::default()
        });
        // Running or blocked without a name.
        f.live(with_status(unnamed(4, "local_busy"), "busy"));
        f.live(with_status(unnamed(5, "local_waiting"), "waiting"));
        // Ghost rule only applies to Desktop sessions.
        f.live(RegistryEntry {
            name: None,
            ..cli(6)
        });
        assert_eq!(
            f.sorted_keys(),
            [
                "cli:cli-6",
                "local_busy",
                "local_named",
                "local_prompted",
                "local_turned",
                "local_waiting"
            ]
        );
    }

    // --- stopped sessions --------------------------------------------------

    /// The Desktop file of a session without a live process, whose latest
    /// turn ended after the person last looked at it.
    fn stopped_unseen(host: &str) -> DesktopSession {
        DesktopSession {
            cli_session_id: Some(format!("cli-{host}")),
            cwd: Some(REPO_CWD.to_owned()),
            last_turn_edge_at: Some(NOW - 30 * MIN),
            last_focused_at: Some(NOW - 40 * MIN),
            ..desk(host)
        }
    }

    #[test]
    fn stopped_sessions_are_recent_unarchived_and_without_process() {
        let old = NOW - STOPPED_WINDOW_MS - MIN;
        let mut f = Fixture::new();
        f.file(stopped_unseen("local_s"))
            .file(DesktopSession {
                is_archived: true,
                ..stopped_unseen("local_archived")
            })
            .file(DesktopSession {
                last_turn_edge_at: Some(old),
                last_focused_at: Some(old - MIN),
                ..stopped_unseen("local_old")
            })
            // An old turn, but recent activity.
            .file(DesktopSession {
                last_turn_edge_at: Some(old),
                last_activity_at: Some(NOW - HOUR),
                ..stopped_unseen("local_active")
            })
            // Run by a live process, found by Desktop id or by CLI id.
            .live(entry(1, "local_live"))
            .file(stopped_unseen("local_live"))
            .live(RegistryEntry {
                session_id: "cli-local_resumed".to_owned(),
                ..cli(2)
            })
            .file(stopped_unseen("local_resumed"));
        let ids = |registry: &[RegistryEntry], registry_ok: bool| -> Vec<String> {
            stopped_sessions(NOW, registry, registry_ok, &f.desktop)
                .iter()
                .map(|d| d.host_session_id.clone())
                .collect()
        };
        assert_eq!(ids(&f.registry, true), ["local_active", "local_s"]);
        // A failed read keeps the previous entries: still known.
        assert_eq!(ids(&f.registry, false), ["local_active", "local_s"]);
        // Never read: every session would look stopped.
        assert!(ids(&[], false).is_empty());
        assert_eq!(
            ids(&[], true),
            ["local_active", "local_live", "local_resumed", "local_s"],
            "no process at all"
        );
    }

    #[test]
    fn stopped_sessions_are_listed_only_when_they_need_the_person() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_live")).file(desk("local_live"));
        f.file(stopped_unseen("local_unseen"));
        // Seen after its latest turn: idle, hidden.
        f.file(DesktopSession {
            last_focused_at: Some(NOW - 20 * MIN),
            ..stopped_unseen("local_seen")
        });
        // Seen, but its turn ended on a question.
        f.file(DesktopSession {
            last_focused_at: Some(NOW - 5 * MIN),
            ..stopped_unseen("local_asked")
        });
        f.transcripts.insert("cli-local_asked".to_owned(), asked());
        // Seen, but its turn ended on an error.
        f.file(DesktopSession {
            last_focused_at: Some(NOW - 5 * MIN),
            error: error_at(NOW - 30 * MIN, "rate_limit: Limite atteinte"),
            ..stopped_unseen("local_failed")
        });
        assert_eq!(
            f.sorted_keys(),
            ["local_asked", "local_failed", "local_live", "local_unseen"]
        );
        assert_eq!(f.snapshot().counts.waiting, 1);

        let asked = f.view("local_asked");
        assert_eq!(asked.state, Waiting);
        assert_eq!(asked.waiting_for.as_deref(), Some(WAITING_DECISION));
        assert_eq!(asked.question.map(|q| q.options.len()), Some(2));
        assert!(asked.stopped && asked.is_desktop);
        assert_eq!(asked.pid, 0);
        assert_eq!(asked.host_session_id.as_deref(), Some("local_asked"));
        assert_eq!(asked.cli_session_id, "cli-local_asked");
        assert_eq!(asked.cwd, REPO_CWD);
        assert_eq!(asked.project, "atlas-web");
        assert_eq!(asked.title, "Desktop local_asked");
        assert_eq!(asked.version, None);
        let failed = f.view("local_failed");
        assert_eq!(failed.state, Error);
        assert_eq!(failed.last_error.as_deref(), Some("rate_limit: Limite atteinte"));
        let unseen = f.view("local_unseen");
        assert_eq!((unseen.state, unseen.state_since), (Unseen, Some(NOW - 30 * MIN)));
        assert!(!f.view("local_live").stopped);

        // Acknowledged in the monitor: hidden again.
        f.acks.insert("local_unseen".to_owned(), NOW - MIN);
        assert!(!f.keys().contains(&"local_unseen".to_owned()));
    }

    #[test]
    fn a_session_keeps_its_card_when_desktop_stops_its_process() {
        let mut f = Fixture::new();
        f.live(RegistryEntry {
            session_id: "cli-local_a".to_owned(),
            ..entry(1, "local_a")
        })
        .file(stopped_unseen("local_a"));
        let before = f.snapshot();
        let live = f.view("local_a");
        assert_eq!((live.state, live.stopped, live.pid), (Unseen, false, 1));

        f.registry.clear();
        let after = f.snapshot();
        assert_eq!(after.sessions.len(), 1);
        let stopped = f.view("local_a");
        assert_eq!((stopped.state, stopped.stopped, stopped.pid), (Unseen, true, 0));
        assert_eq!(stopped.state_since, live.state_since);
        assert!(transitions(Some(&before), &after).is_empty(), "same episode");
    }

    #[test]
    fn a_stopped_session_without_cli_id_uses_desktop_alone() {
        let mut f = Fixture::new();
        f.file(DesktopSession {
            cli_session_id: None,
            cwd: None,
            ..stopped_unseen("local_a")
        });
        let view = f.view("local_a");
        assert_eq!((view.state, view.cli_session_id.as_str()), (Unseen, ""));
        assert_eq!(view.title, "Desktop local_a");
    }

    // --- proposed tasks ----------------------------------------------------

    fn card(id: &str, created_at: i64) -> TaskSuggestion {
        TaskSuggestion {
            id: id.to_owned(),
            title: format!("Tâche {id}"),
            tldr: Some(format!("Pourquoi **{id}**")),
            created_at,
            ..TaskSuggestion::default()
        }
    }

    fn offer(id: &str, created_at: i64) -> TaskSuggestion {
        TaskSuggestion {
            offer: true,
            expires_at: Some(created_at + PROPOSAL_WINDOW_MS),
            ..card(id, created_at)
        }
    }

    #[test]
    fn a_live_session_waits_on_its_task_offer() {
        let file = DesktopSession {
            suggestions: vec![offer("task_old", NOW - 20 * MIN), offer("task_new", NOW - 2 * MIN)],
            ..desk("local_a")
        };
        // The registry shows the session busy: its tool call blocks.
        let d = derive_state(&StateInput {
            desktop: Some(&file),
            ..input("busy")
        });
        assert_eq!(d.state, Waiting);
        assert_eq!(d.waiting_for.as_deref(), Some(WAITING_OFFER));
        assert_eq!(d.since, Some(NOW - 2 * MIN));
        let q = d.question.expect("question");
        assert_eq!(q.text, "Nouvelle session pour « Tâche task_new », ou ici ?");
        assert_eq!(q.options, ["Nouvelle session", "Ici"]);
        assert_eq!(q.source, "offer");

        // "New session" chosen, expired, or no process: nothing to answer.
        let busy = |suggestion: TaskSuggestion, stopped: bool| {
            let file = DesktopSession {
                suggestions: vec![suggestion],
                ..desk("local_a")
            };
            derive_state(&StateInput {
                desktop: Some(&file),
                stopped,
                ..input("busy")
            })
            .state
        };
        let accepted = TaskSuggestion {
            accepted: true,
            ..offer("task_a", NOW - MIN)
        };
        assert_eq!(busy(accepted, false), Working);
        let expired = TaskSuggestion {
            expires_at: Some(NOW - 1),
            ..offer("task_e", NOW - PROPOSAL_WINDOW_MS - 1)
        };
        assert_eq!(busy(expired, false), Working);
        assert_eq!(busy(offer("task_s", NOW - MIN), true), Working);
        // A proposal the session made on its own is no wait.
        assert_eq!(busy(card("task_p", NOW - MIN), false), Working);
    }

    #[test]
    fn a_stopped_session_with_a_leftover_offer_stays_hidden() {
        let mut f = Fixture::new();
        f.file(DesktopSession {
            suggestions: vec![offer("task_o", NOW - 10 * MIN)],
            last_turn_edge_at: Some(NOW - 30 * MIN),
            last_focused_at: Some(NOW - 20 * MIN),
            ..desk("local_a")
        });
        assert!(f.keys().is_empty());
        assert!(f.snapshot().proposed_tasks.is_empty(), "offers are not cards");
    }

    #[test]
    fn only_a_working_session_shows_its_sub_agents() {
        let mut f = Fixture::new();
        let mut working = entry(1, "local_w");
        working.status = Some("busy".to_owned());
        let idle = entry(2, "local_i");
        f.live(working).live(idle);
        f.file(desk("local_w")).file(desk("local_i"));
        let agent = RunningAgent {
            id: "a1".to_owned(),
            description: "Audit".to_owned(),
            agent_type: "Explore".to_owned(),
        };
        f.agents.insert("cli-1".to_owned(), vec![agent.clone()]);
        // Left behind by a process that died.
        f.agents.insert("cli-2".to_owned(), vec![agent.clone()]);

        assert_eq!(f.view("local_w").agents, [agent]);
        assert_ne!(f.view("local_i").state, SessionState::Working);
        assert!(f.view("local_i").agents.is_empty());
    }

    #[test]
    fn a_card_is_passed_over_once_the_person_wrote_twice_since() {
        let mut f = Fixture::new();
        f.file(DesktopSession {
            suggestions: vec![card("task_new", NOW - HOUR), card("task_old", NOW - 3 * HOUR)],
            cli_session_id: Some("cli-quiet".to_owned()),
            ..desk("local_quiet")
        });
        f.transcripts.insert(
            "cli-quiet".to_owned(),
            TranscriptInfo {
                last_human_prompt_at: Some(NOW - 10 * MIN),
                previous_human_prompt_at: Some(NOW - 2 * HOUR),
                ..TranscriptInfo::default()
            },
        );
        let s = f.snapshot();
        let passed: Vec<(&str, bool)> =
            s.proposed_tasks.iter().map(|t| (t.id.as_str(), t.passed_over)).collect();
        assert_eq!(passed, [("task_new", false), ("task_old", true)]);

        // Their transcripts are read even when the sessions are not listed.
        let ids: Vec<&str> = proposing_sessions(NOW, &f.desktop).collect();
        assert_eq!(ids, ["cli-quiet"]);
    }

    #[test]
    fn proposed_tasks_are_recent_pending_cards() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_live")).file(DesktopSession {
            suggestions: vec![
                card("task_chip", NOW - HOUR),
                TaskSuggestion {
                    expires_at: Some(NOW + HOUR),
                    ..card("task_prop", NOW - 2 * HOUR)
                },
                offer("task_offer", NOW - MIN),
                TaskSuggestion {
                    auto_start: true,
                    ..card("task_cloud", NOW - MIN)
                },
                TaskSuggestion {
                    expires_at: Some(NOW),
                    ..card("task_expired", NOW - 3 * HOUR)
                },
                card("task_old", NOW - PROPOSAL_WINDOW_MS - 1),
                TaskSuggestion {
                    title: " ** ".to_owned(),
                    ..card("task_blank", NOW - MIN)
                },
            ],
            cwd: Some(REPO_CWD.to_owned()),
            ..desk("local_live")
        });
        // A session not listed (idle, no process), in a worktree.
        f.file(DesktopSession {
            suggestions: vec![card("task_wt", NOW - 30 * MIN)],
            cwd: Some(WORKTREE_CWD.to_owned()),
            title: None,
            ..desk("local_quiet")
        });
        f.file(DesktopSession {
            suggestions: vec![card("task_archived", NOW - MIN)],
            is_archived: true,
            ..desk("local_archived")
        });

        let s = f.snapshot();
        assert_eq!(s.sessions.len(), 1);
        let ids: Vec<&str> = s.proposed_tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["task_wt", "task_chip", "task_prop"]);
        let wt = &s.proposed_tasks[0];
        assert_eq!(wt.session_key, "local_quiet");
        assert_eq!(wt.session_title, "affectionate-lalande-433900", "the folder names it");
        assert_eq!(wt.project, "Carnet");
        assert_eq!(wt.tldr.as_deref(), Some("Pourquoi task_wt"));
        assert_eq!(wt.expires_at, None);
        // No expiry in Desktop: the monitor's window ends it.
        assert_eq!(wt.listed_until, NOW - 30 * MIN + PROPOSAL_WINDOW_MS);
        let chip = &s.proposed_tasks[1];
        assert_eq!(chip.title, "Tâche task_chip");
        // Listed: the title and project its card shows.
        assert_eq!(
            (chip.session_title.as_str(), chip.project.as_str()),
            ("Desktop local_live", "atlas-web")
        );
        let prop = &s.proposed_tasks[2];
        assert_eq!((prop.expires_at, prop.listed_until), (Some(NOW + HOUR), NOW + HOUR));
    }

    #[test]
    fn a_task_keeps_its_project_when_its_session_leaves_the_list() {
        let sub = format!(r"{REPO_CWD}\ui");
        let git = GitInfo {
            repo_root: Some(r"D:\Données Utilisateur\Documents\GitHub\Atlas-Web".to_owned()),
            ..GitInfo::default()
        };
        let mut f = Fixture::new();
        f.file(DesktopSession {
            suggestions: vec![card("task_a", NOW - HOUR)],
            cwd: Some(sub.clone()),
            ..stopped_unseen("local_s")
        });
        f.git_pr.insert("local_s".to_owned(), (Some(git), None));
        let listed = f.snapshot();
        assert_eq!(listed.sessions[0].project, "Atlas-Web");
        assert_eq!(listed.proposed_tasks[0].project, "Atlas-Web");
        // Seen (clicking the task does it): the session leaves the list.
        f.acks.insert("local_s".to_owned(), NOW);
        let alone = f.snapshot();
        assert!(alone.sessions.is_empty());
        assert_eq!(alone.proposed_tasks[0].project, "Atlas-Web");
        // Without git yet: the folder's name.
        f.git_pr.clear();
        assert_eq!(f.snapshot().proposed_tasks[0].project, "ui");
    }

    // --- plans, reviews, Auto-fix -----------------------------------------

    #[test]
    fn a_plan_to_approve_is_named() {
        let planning = TranscriptInfo {
            current_tool: Some(ToolActivity {
                name: "ExitPlanMode".to_owned(),
                summary: "Plan : migrer le cache".to_owned(),
                at: Some(NOW - MIN),
            }),
            ..turn(TurnOrigin::Human, NOW - 10 * MIN, None)
        };
        let reason = |registry: Option<&str>, transcript: &TranscriptInfo| {
            derive_state(&StateInput {
                registry_waiting_for: registry,
                transcript: Some(transcript),
                ..input("waiting")
            })
            .waiting_for
        };
        assert_eq!(reason(Some("permission prompt"), &planning).as_deref(), Some(WAITING_PLAN_APPROVAL));
        assert_eq!(reason(None, &planning).as_deref(), Some(WAITING_PLAN_APPROVAL));
        // Another kind of wait keeps its own reason.
        assert_eq!(reason(Some("dialog open"), &planning).as_deref(), Some("dialog open"));
        let editing = TranscriptInfo {
            current_tool: Some(tool("Edit", NOW - MIN)),
            ..planning.clone()
        };
        assert_eq!(reason(Some("permission prompt"), &editing).as_deref(), Some("permission prompt"));
    }

    #[test]
    fn review_findings_show_while_some_are_open() {
        let report = |open: u32| FindingsReport {
            count: 4,
            confirmed: 2,
            open,
            at: Some(NOW - MIN),
        };
        let mut f = Fixture::new();
        f.live(entry(1, "local_a")).file(desk("local_a"));
        f.transcripts.insert(
            "cli-1".to_owned(),
            TranscriptInfo {
                findings: Some(report(3)),
                ..TranscriptInfo::default()
            },
        );
        assert_eq!(f.view("local_a").findings, Some(report(3)));
        if let Some(t) = f.transcripts.get_mut("cli-1") {
            t.findings = Some(report(0));
        }
        assert_eq!(f.view("local_a").findings, None, "all handled");
    }

    #[test]
    fn auto_fix_marks_the_watched_open_pr_while_desktop_runs() {
        let card = |number: u32, repository: Option<&str>| DesktopPrBinding {
            number: Some(number),
            url: None,
            repository: repository.map(str::to_owned),
            state: Some("OPEN".to_owned()),
        };
        let shown = PrInfo {
            repository: Some("Owner/Repo".to_owned()),
            ..pr("OPEN", ChecksState::Fail)
        };
        let marked = |cards: Vec<DesktopPrBinding>, shown: PrInfo, archived: bool, live: bool| {
            let mut f = Fixture::new();
            f.file(DesktopSession {
                auto_fix_prs: cards,
                is_archived: archived,
                ..desk("local_a")
            });
            if live {
                f.live(entry(1, "local_a"));
            }
            f.git_pr.insert("local_a".to_owned(), (None, Some(shown)));
            f.snapshot()
                .sessions
                .iter()
                .find(|s| s.key == "local_a")
                .and_then(|s| s.pr.as_ref())
                .map(|pr| pr.auto_fix)
        };
        assert_eq!(marked(vec![card(7, Some("owner/repo"))], shown.clone(), false, true), Some(true));
        assert_eq!(marked(vec![card(7, None)], shown.clone(), false, true), Some(true));
        // Another PR, or another repository.
        assert_eq!(marked(vec![card(8, Some("owner/repo"))], shown.clone(), false, true), Some(false));
        assert_eq!(marked(vec![card(7, Some("owner/other"))], shown.clone(), false, true), Some(false));
        // By link, whatever the number.
        let by_link = DesktopPrBinding {
            number: None,
            url: Some("https://github.com/o/r/pull/7/".to_owned()),
            repository: None,
            state: None,
        };
        assert_eq!(marked(vec![by_link], shown.clone(), false, true), Some(true));
        // Merged since, or Desktop closed: nothing watches it.
        let merged = PrInfo {
            state: "MERGED".to_owned(),
            ..shown.clone()
        };
        assert_eq!(marked(vec![card(7, None)], merged, false, true), Some(false));
        assert_eq!(marked(vec![card(7, None)], shown.clone(), false, false), None, "not listed");
        let mut f = Fixture::new();
        // Desktop runs (another session's process), this session is stopped.
        f.live(entry(2, "local_other")).file(desk("local_other"));
        f.file(DesktopSession {
            auto_fix_prs: vec![card(7, None)],
            ..stopped_unseen("local_a")
        });
        f.git_pr.insert("local_a".to_owned(), (None, Some(shown.clone())));
        assert_eq!(f.view("local_a").pr.map(|pr| pr.auto_fix), Some(true));
        // No process Desktop started is alive: Desktop is closed.
        f.registry.clear();
        assert_eq!(f.view("local_a").pr.map(|pr| pr.auto_fix), Some(false));
    }

    // --- one session -------------------------------------------------------

    #[test]
    fn waiting_reason_is_only_kept_while_waiting() {
        let mut f = Fixture::new();
        f.live(RegistryEntry {
            waiting_for: Some(" permission prompt ".to_owned()),
            ..with_status(entry(1, "local_w"), "waiting")
        })
        .live(RegistryEntry {
            waiting_for: Some("input needed".to_owned()),
            ..with_status(entry(2, "local_b"), "busy")
        });
        let w = f.view("local_w");
        assert_eq!(w.state, Waiting);
        assert_eq!(w.waiting_for.as_deref(), Some("permission prompt"));
        assert_eq!(f.view("local_b").waiting_for, None);
    }

    #[test]
    fn acks_are_looked_up_by_session_key() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_a")).file(DesktopSession {
            last_turn_edge_at: Some(NOW - 10 * MIN),
            last_focused_at: Some(NOW - HOUR),
            ..desk("local_a")
        });
        assert_eq!(f.view("local_a").state, Unseen);
        f.acks.insert("local_a".to_owned(), NOW - 5 * MIN);
        assert_eq!(f.view("local_a").state, Idle);
        f.acks.insert("local_a".to_owned(), NOW - 20 * MIN);
        assert_eq!(f.view("local_a").state, Unseen);
    }

    #[test]
    fn state_since_depends_on_the_state() {
        let mut f = Fixture::new();
        f.live(RegistryEntry {
            status_updated_at: Some(NOW - 5 * MIN),
            ..with_status(entry(1, "local_working"), "busy")
        })
        .live(RegistryEntry {
            status_updated_at: None,
            updated_at: Some(NOW - 3 * MIN),
            ..with_status(entry(2, "local_waiting"), "waiting")
        })
        .live(entry(3, "local_idle"))
        .file(desk("local_idle"))
        .live(entry(4, "local_no_file"))
        .live(entry(5, "local_unseen"))
        .file(DesktopSession {
            last_turn_edge_at: Some(NOW - 7 * MIN),
            ..desk("local_unseen")
        })
        .live(entry(6, "local_error"))
        .file(DesktopSession {
            latest_user_frame_at: Some(NOW - 9 * MIN),
            last_turn_edge_at: Some(NOW - 8 * MIN),
            error: error_at(NOW - 8 * MIN, "boom"),
            ..desk("local_error")
        })
        .live(RegistryEntry {
            status_updated_at: None,
            updated_at: None,
            ..entry(7, "local_nothing")
        });
        assert_eq!(f.view("local_working").state_since, Some(NOW - 5 * MIN));
        assert_eq!(f.view("local_waiting").state_since, Some(NOW - 3 * MIN));
        assert_eq!(f.view("local_idle").state_since, Some(NOW - 90 * MIN));
        assert_eq!(f.view("local_no_file").state_since, Some(NOW - 30 * MIN));
        let unseen = f.view("local_unseen");
        assert_eq!(unseen.state, Unseen);
        assert_eq!(unseen.state_since, Some(NOW - 7 * MIN));
        let error = f.view("local_error");
        assert_eq!(error.state, Error);
        assert_eq!(error.state_since, Some(NOW - 8 * MIN));
        assert_eq!(f.view("local_nothing").state_since, None);
    }

    #[test]
    fn title_fallbacks() {
        let mut f = Fixture::new();
        f.live(RegistryEntry {
            name: Some("  Mon titre  ".to_owned()),
            ..entry(1, "local_name")
        })
        .live(with_status(unnamed(2, "local_desktop"), "busy"))
        .file(DesktopSession {
            title: Some(" Titre Desktop ".to_owned()),
            ..desk("local_desktop")
        })
        .live(RegistryEntry {
            name: Some(String::new()),
            ..with_status(unnamed(3, "local_folder"), "busy")
        })
        .file(DesktopSession {
            title: Some("  ".to_owned()),
            ..desk("local_folder")
        })
        .live(RegistryEntry {
            cwd: String::new(),
            ..with_status(unnamed(4, "local_nothing"), "busy")
        })
        .live(RegistryEntry {
            cwd: r"C:\Projets\Été\".to_owned(),
            ..with_status(unnamed(5, "local_trailing"), "busy")
        })
        // Desktop renamed the session after its process started.
        .live(RegistryEntry {
            name: Some("btw: en une phrase, on travaille sur quoi là ?".to_owned()),
            ..entry(6, "local_renamed")
        })
        .file(DesktopSession {
            title: Some("Architecture principale Figma".to_owned()),
            ..desk("local_renamed")
        });
        assert_eq!(f.view("local_renamed").title, "Architecture principale Figma");
        assert_eq!(f.view("local_name").title, "Mon titre");
        assert_eq!(f.view("local_desktop").title, "Titre Desktop");
        assert_eq!(f.view("local_folder").title, "atlas-web");
        assert_eq!(f.view("local_nothing").title, "Session");
        assert_eq!(f.view("local_trailing").title, "Été");
    }

    #[test]
    fn cwd_falls_back_to_the_desktop_file() {
        let mut f = Fixture::new();
        f.live(RegistryEntry {
            cwd: "  ".to_owned(),
            ..entry(1, "local_a")
        })
        .file(DesktopSession {
            cwd: Some(WORKTREE_CWD.to_owned()),
            ..desk("local_a")
        });
        let view = f.view("local_a");
        assert_eq!(view.cwd, WORKTREE_CWD);
        assert_eq!(view.project, "Carnet");
    }

    #[test]
    fn worktree_path_gives_project_and_worktree() {
        let carnet = || {
            (
                "Carnet".to_owned(),
                Some("affectionate-lalande-433900".to_owned()),
            )
        };
        assert_eq!(project_and_worktree(WORKTREE_CWD, None, None), carnet());
        let slashes = WORKTREE_CWD.replace('\\', "/");
        assert_eq!(project_and_worktree(&slashes, None, None), carnet());
        // Any case, and a sub-folder inside the worktree.
        let upper =
            r"D:\Données Utilisateur\Documents\GitHub\Carnet\.CLAUDE\Worktrees\affectionate-lalande-433900\src\ui";
        assert_eq!(project_and_worktree(upper, None, None), carnet());
        // The git root of a worktree is the worktree itself: the path wins.
        let git = GitInfo {
            repo_root: Some(WORKTREE_CWD.to_owned()),
            ..Default::default()
        };
        assert_eq!(project_and_worktree(WORKTREE_CWD, Some(&git), None), carnet());
        // Desktop's worktree name wins.
        assert_eq!(
            project_and_worktree(WORKTREE_CWD, None, Some("nom-desktop")),
            ("Carnet".to_owned(), Some("nom-desktop".to_owned()))
        );
        // Nested worktrees: outer repository, inner worktree.
        let nested = r"D:\Repos\app\.claude\worktrees\outer\.claude\worktrees\inner";
        assert_eq!(
            project_and_worktree(nested, None, None),
            ("app".to_owned(), Some("inner".to_owned()))
        );
    }

    #[test]
    fn plain_paths_use_git_root_then_cwd() {
        let cwd = r"D:\Données Utilisateur\Documents\GitHub\atlas-web\src\ui";
        let git = GitInfo {
            repo_root: Some(r"D:\Données Utilisateur\Documents\GitHub\atlas-web".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            project_and_worktree(cwd, Some(&git), None),
            ("atlas-web".to_owned(), None)
        );
        assert_eq!(project_and_worktree(cwd, None, None), ("ui".to_owned(), None));
        let blank_root = GitInfo {
            repo_root: Some("  ".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            project_and_worktree(cwd, Some(&blank_root), None),
            ("ui".to_owned(), None)
        );
        // A `.claude\worktrees` folder without a name is not a worktree.
        assert_eq!(
            project_and_worktree(r"D:\Repos\app\.claude\worktrees\", None, None),
            ("worktrees".to_owned(), None)
        );
        // Desktop's worktree name still applies.
        assert_eq!(
            project_and_worktree(cwd, None, Some("wt")),
            ("ui".to_owned(), Some("wt".to_owned()))
        );
        assert_eq!(project_and_worktree("", None, None), (String::new(), None));
    }

    #[test]
    fn view_of_a_worktree_session() {
        let mut f = Fixture::new();
        f.live(RegistryEntry {
            cwd: WORKTREE_CWD.to_owned(),
            ..entry(1, "local_wt")
        })
        .file(DesktopSession {
            worktree_name: Some("   ".to_owned()),
            ..desk("local_wt")
        });
        let view = f.view("local_wt");
        assert_eq!(view.project, "Carnet");
        assert_eq!(view.worktree.as_deref(), Some("affectionate-lalande-433900"));
        assert_eq!(view.cwd, WORKTREE_CWD);
    }

    #[test]
    fn desktop_and_registry_fields_are_copied() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_a")).file(DesktopSession {
            model: Some("claude-opus-5-5".to_owned()),
            effort: Some("high".to_owned()),
            permission_mode: Some("acceptEdits".to_owned()),
            completed_turns: Some(4),
            context_exceeded_count: Some(2),
            last_activity_at: Some(NOW - 10 * MIN),
            ..desk("local_a")
        });
        f.transcripts.insert(
            "cli-1".to_owned(),
            TranscriptInfo {
                last_event_at: Some(NOW - 20 * MIN),
                ..Default::default()
            },
        );
        let view = f.view("local_a");
        assert_eq!(view.state, Idle);
        assert_eq!(view.host_session_id.as_deref(), Some("local_a"));
        assert_eq!(view.cli_session_id, "cli-1");
        assert_eq!(view.pid, 1);
        assert!(view.is_desktop);
        assert_eq!(view.cwd, REPO_CWD);
        assert_eq!(view.project, "atlas-web");
        assert_eq!(view.worktree, None);
        assert_eq!(view.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(view.effort.as_deref(), Some("high"));
        assert_eq!(view.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(view.completed_turns, Some(4));
        assert_eq!(view.context_exceeded_count, Some(2));
        assert_eq!(view.version.as_deref(), Some("2.1.308"));
        assert_eq!(view.last_activity_at, Some(NOW - 10 * MIN));
        assert_eq!(view.last_error, None);
    }

    #[test]
    fn last_activity_uses_the_newest_source() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_a"))
            .file(DesktopSession {
                last_activity_at: None,
                ..desk("local_a")
            })
            .live(entry(2, "local_b"))
            .file(DesktopSession {
                last_activity_at: Some(NOW - HOUR),
                ..desk("local_b")
            })
            .live(entry(3, "local_c"));
        for cli_id in ["cli-1", "cli-2", "cli-3"] {
            f.transcripts.insert(
                cli_id.to_owned(),
                TranscriptInfo {
                    last_event_at: Some(NOW - 5 * MIN),
                    ..Default::default()
                },
            );
        }
        assert_eq!(f.view("local_a").last_activity_at, Some(NOW - 5 * MIN));
        assert_eq!(f.view("local_b").last_activity_at, Some(NOW - 5 * MIN));
        assert_eq!(f.view("local_c").last_activity_at, Some(NOW - 5 * MIN));
        f.transcripts.clear();
        assert_eq!(f.view("local_b").last_activity_at, Some(NOW - HOUR));
        assert_eq!(f.view("local_c").last_activity_at, None);
    }

    #[test]
    fn last_error_is_only_kept_in_error_state() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_a")).file(DesktopSession {
            latest_user_frame_at: Some(NOW - 10 * MIN),
            last_turn_edge_at: Some(NOW - 5 * MIN),
            error: error_at(NOW - 5 * MIN, " rate_limit "),
            ..desk("local_a")
        });
        let view = f.view("local_a");
        assert_eq!(view.state, Error);
        assert_eq!(view.last_error.as_deref(), Some("rate_limit"));

        f.registry[0].status = Some("busy".to_owned());
        let view = f.view("local_a");
        assert_eq!(view.state, Working);
        assert_eq!(view.last_error, None);
    }

    #[test]
    fn parent_title_is_filled_from_its_desktop_file() {
        let spawned = |host: &str, parent: &str, title: Option<&str>| DesktopSession {
            spawned_from: Some(SessionRef {
                host_session_id: parent.to_owned(),
                title: title.map(str::to_owned),
            }),
            ..desk(host)
        };
        let mut f = Fixture::new();
        // The parent is archived and not live: its file still names it.
        f.file(DesktopSession {
            title: Some("Parent".to_owned()),
            is_archived: true,
            ..desk("local_p")
        })
        .live(entry(1, "local_a"))
        .file(spawned("local_a", "local_p", None))
        .live(entry(2, "local_b"))
        .file(spawned("local_b", "local_p", Some("Titre de l'enfant")))
        .live(entry(3, "local_c"))
        .file(spawned("local_c", "local_missing", Some("Titre de l'enfant")))
        .live(entry(4, "local_d"))
        .file(spawned("local_d", "  ", Some("x")))
        .live(entry(5, "local_e"))
        .file(spawned("local_e", "local_live", Some(" ")))
        .live(entry(6, "local_live"));

        let parent = |key: &str| f.view(key).parent;
        assert_eq!(
            parent("local_a"),
            Some(SessionRef {
                host_session_id: "local_p".to_owned(),
                title: Some("Parent".to_owned()),
            })
        );
        // `spawnedFrom.title` names the child, never the parent.
        assert_eq!(
            parent("local_b").and_then(|p| p.title).as_deref(),
            Some("Parent")
        );
        assert_eq!(
            parent("local_c"),
            Some(SessionRef {
                host_session_id: "local_missing".to_owned(),
                title: None,
            })
        );
        assert_eq!(parent("local_d"), None);
        // No Desktop file for a live parent: its registry name.
        assert_eq!(
            parent("local_e").and_then(|p| p.title).as_deref(),
            Some("Session 6")
        );
        assert_eq!(parent("local_live"), None);
    }

    #[test]
    fn sessions_nest_under_their_nearest_listed_ancestor() {
        let side = |host: &str, parent: &str| DesktopSession {
            spawned_from: Some(SessionRef {
                host_session_id: parent.to_owned(),
                title: None,
            }),
            side_session: true,
            ..desk(host)
        };
        // Started from a "New task" card: no side session.
        let task = |host: &str, parent: &str| DesktopSession {
            side_session: false,
            ..side(host, parent)
        };
        let archived = |d: DesktopSession| DesktopSession {
            is_archived: true,
            ..d
        };
        let mut f = Fixture::new();
        f.live(entry(1, "local_root"))
            .file(desk("local_root"))
            .live(entry(2, "local_child"))
            .file(side("local_child", "local_root"))
            // Not listed (stopped while idle): its child still joins the root.
            .file(side("local_idle", "local_root"))
            .live(entry(3, "local_grandchild"))
            .file(side("local_grandchild", "local_idle"))
            // An archived parent: every kind of link reaches past it.
            .file(archived(task("local_gone", "local_root")))
            .live(entry(4, "local_side_past"))
            .file(side("local_side_past", "local_gone"))
            .live(entry(5, "local_task_past"))
            .file(task("local_task_past", "local_gone"))
            // A fork hangs from the session it was forked from.
            .live(entry(6, "local_fork"))
            .file(DesktopSession {
                forked_from: Some("local_child".to_owned()),
                ..task("local_fork", "local_unknown")
            })
            // Detached by the person in Desktop.
            .live(entry(7, "local_detached"))
            .file(DesktopSession {
                lineage_detached: true,
                ..side("local_detached", "local_root")
            })
            // A loop through sessions that are not listed.
            .file(side("local_loop_a", "local_looped"))
            .live(entry(8, "local_looped"))
            .file(side("local_looped", "local_loop_a"))
            // Its ancestors are unknown.
            .live(entry(9, "local_orphan"))
            .file(side("local_orphan", "local_missing"))
            // A link to itself is ignored: the other one counts.
            .live(entry(10, "local_self"))
            .file(DesktopSession {
                forked_from: Some("local_root".to_owned()),
                ..side("local_self", "local_self")
            });

        let nest = |key: &str| f.view(key).nest_under;
        assert_eq!(nest("local_root"), None);
        assert_eq!(nest("local_child").as_deref(), Some("local_root"));
        assert_eq!(nest("local_grandchild").as_deref(), Some("local_root"));
        assert_eq!(
            f.view("local_grandchild").parent.map(|p| p.host_session_id).as_deref(),
            Some("local_idle"),
            "the parent stays the direct one"
        );
        assert_eq!(nest("local_side_past").as_deref(), Some("local_root"));
        assert_eq!(nest("local_task_past").as_deref(), Some("local_root"));
        assert_eq!(nest("local_self").as_deref(), Some("local_root"));
        assert_eq!(
            f.view("local_self").parent.map(|p| p.host_session_id).as_deref(),
            Some("local_root")
        );
        assert_eq!(nest("local_fork").as_deref(), Some("local_child"));
        assert_eq!(
            f.view("local_fork").parent.map(|p| p.host_session_id).as_deref(),
            Some("local_child")
        );
        assert_eq!(nest("local_detached"), None);
        assert_eq!(f.view("local_detached").parent, None);
        assert_eq!(nest("local_looped"), None);
        assert_eq!(nest("local_orphan"), None);
        assert_eq!(f.view("local_orphan").parent.map(|p| p.host_session_id).as_deref(), Some("local_missing"));
    }

    #[test]
    fn a_forked_side_chat_hangs_from_its_fork_source() {
        // A `/btw` side chat, or a fork of a side session: not a side session
        // (desktop.rs), so the fork source wins over the spawner.
        let mut f = Fixture::new();
        f.live(entry(1, "local_a"))
            .file(desk("local_a"))
            .live(entry(2, "local_b"))
            .file(desk("local_b"))
            .live(entry(3, "local_btw"))
            .file(DesktopSession {
                spawned_from: Some(SessionRef {
                    host_session_id: "local_a".to_owned(),
                    title: None,
                }),
                side_session: false,
                forked_from: Some("local_b".to_owned()),
                ..desk("local_btw")
            });
        assert_eq!(f.view("local_btw").nest_under.as_deref(), Some("local_b"));
    }

    #[test]
    fn recap_prefers_the_wrap_up() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_a"))
            .file(DesktopSession {
                turn_wrap_up: Some("  Correctif livré.  ".to_owned()),
                last_turn_report: Some("Rapport".to_owned()),
                ..desk("local_a")
            })
            .live(entry(2, "local_b"))
            .file(DesktopSession {
                turn_wrap_up: Some("   ".to_owned()),
                last_turn_report: Some(" Rapport ".to_owned()),
                ..desk("local_b")
            })
            .live(entry(3, "local_c"))
            .file(DesktopSession {
                turn_wrap_up: Some(String::new()),
                last_turn_report: Some("\n".to_owned()),
                ..desk("local_c")
            })
            .live(entry(4, "local_d"));
        let said = |text: &str| TranscriptInfo {
            last_assistant_text: Some(text.to_owned()),
            ..TranscriptInfo::default()
        };
        f.transcripts.insert("cli-1".to_owned(), said("Texte final"));
        f.transcripts.insert("cli-3".to_owned(), said(" Texte final "));
        assert_eq!(f.view("local_a").recap.as_deref(), Some("Correctif livré."));
        assert_eq!(f.view("local_b").recap.as_deref(), Some("Rapport"));
        assert_eq!(f.view("local_c").recap.as_deref(), Some("Texte final"));
        assert_eq!(f.view("local_d").recap, None);
        f.transcripts.insert("cli-4".to_owned(), said("  "));
        assert_eq!(f.view("local_d").recap, None);
    }

    #[test]
    fn recap_order_and_plain_text() {
        let wrapped = |recap: &str| TranscriptInfo {
            turn: Some(TurnInfo {
                wrapped_up: true,
                wrap_up_recap: Some(recap.to_owned()),
                ..TurnInfo::default()
            }),
            last_assistant_text: Some("Texte final".to_owned()),
            ..TranscriptInfo::default()
        };
        let mut f = Fixture::new();
        f.live(entry(1, "local_a"))
            .file(DesktopSession {
                turn_summary: verdict("completed", None),
                last_turn_report: Some("Rapport".to_owned()),
                ..desk("local_a")
            })
            .live(entry(2, "local_b"))
            .file(DesktopSession {
                turn_summary: verdict("completed", None),
                last_turn_report: Some("Rapport".to_owned()),
                ..desk("local_b")
            })
            .live(entry(3, "local_c"))
            .file(DesktopSession {
                turn_summary: verdict("completed", None).map(|s| TurnSummary {
                    fresh: false,
                    ..s
                }),
                last_turn_report: Some(
                    "La session [Implémenter #6](#local_6093b050-3490) a écrit `lot.ts`.⏎**Fini**"
                        .to_owned(),
                ),
                ..desk("local_c")
            });
        // The transcript's wrap-up (before Desktop saves its own) comes first.
        f.transcripts.insert("cli-1".to_owned(), wrapped("Lot **prêt**."));
        assert_eq!(f.view("local_a").recap.as_deref(), Some("Lot prêt."));
        // Then Desktop's fresh verdict, then the report.
        assert_eq!(f.view("local_b").recap.as_deref(), Some("PR #311 ouverte"));
        assert_eq!(
            f.view("local_c").recap.as_deref(),
            Some("La session Implémenter #6 a écrit lot.ts. Fini")
        );
    }

    #[test]
    fn decision_view_and_order() {
        let mut f = Fixture::new();
        f.live(with_status(entry(1, "local_working"), "busy"))
            .live(RegistryEntry {
                waiting_for: Some("permission prompt".to_owned()),
                status_updated_at: Some(NOW - 2 * MIN),
                ..with_status(entry(2, "local_permission"), "waiting")
            })
            .live(entry(3, "local_decision"))
            .file(DesktopSession {
                last_focused_at: Some(NOW),
                ..desk("local_decision")
            });
        f.transcripts.insert("cli-3".to_owned(), asked());
        f.acks.insert("local_decision".to_owned(), NOW);
        let s = f.snapshot();
        let keys: Vec<&str> = s.sessions.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(keys, ["local_decision", "local_permission", "local_working"]);
        assert_eq!(s.counts.waiting, 2);
        let decision = &s.sessions[0];
        assert_eq!(decision.state, Waiting);
        assert_eq!(decision.waiting_for.as_deref(), Some("decision"));
        assert_eq!(decision.question, Some(ask(NOW - 10 * MIN - 5_000)));
        assert_eq!(decision.state_since, Some(NOW - 10 * MIN - 5_000));
        assert_eq!(s.sessions[1].question, None);

        // Answered: the transition leaves Waiting.
        let before = f.snapshot();
        f.transcripts.insert(
            "cli-3".to_owned(),
            turn(TurnOrigin::Human, NOW - MIN, None),
        );
        f.registry[2].status = Some("busy".to_owned());
        let after = f.snapshot();
        let change = transitions(Some(&before), &after);
        assert_eq!(change.len(), 1);
        assert_eq!((change[0].from, change[0].to), (Some(Waiting), Working));
    }

    #[test]
    fn engine_turn_end_reaches_the_view() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_a")).file(DesktopSession {
            last_turn_edge_at: Some(NOW - 10 * MIN),
            last_focused_at: Some(NOW - HOUR),
            ..desk("local_a")
        });
        f.acks.insert("local_a".to_owned(), NOW - 5 * MIN);
        assert_eq!(f.view("local_a").state, Idle);
        f.turn_ended.insert("local_a".to_owned(), NOW - 1_000);
        let view = f.view("local_a");
        assert_eq!((view.state, view.state_since), (Unseen, Some(NOW - 1_000)));
    }

    #[test]
    fn activity_follows_the_state() {
        let plan = PlanProgress {
            items: vec![
                PlanItem {
                    content: "Lire".to_owned(),
                    status: "completed".to_owned(),
                },
                PlanItem {
                    content: "Écrire".to_owned(),
                    status: "in_progress".to_owned(),
                },
            ],
            done: 1,
            total: 2,
            current: Some("Écrire".to_owned()),
            updated_at: Some(NOW - MIN),
            wrapped_up: false,
        };
        let both = TranscriptInfo {
            current_tool: Some(tool("Bash", NOW - MIN)),
            last_tool: Some(tool("Edit", NOW - 2 * MIN)),
            plan: Some(plan.clone()),
            ..Default::default()
        };
        let last_only = TranscriptInfo {
            last_tool: Some(tool("Read", NOW - 3 * MIN)),
            ..Default::default()
        };
        let mut f = Fixture::new();
        f.live(with_status(entry(1, "local_working"), "busy"))
            .live(with_status(entry(2, "local_working_last"), "busy"))
            .live(entry(3, "local_idle"))
            .live(with_status(entry(4, "local_no_transcript"), "busy"));
        f.transcripts.insert("cli-1".to_owned(), both.clone());
        f.transcripts.insert("cli-2".to_owned(), last_only);
        f.transcripts.insert("cli-3".to_owned(), both);

        let working = f.view("local_working");
        assert_eq!(working.activity.map(|a| a.name).as_deref(), Some("Bash"));
        assert_eq!(working.plan, Some(plan.clone()));
        assert_eq!(
            f.view("local_working_last").activity.map(|a| a.name).as_deref(),
            Some("Read")
        );
        let idle = f.view("local_idle");
        assert_eq!(idle.activity.map(|a| a.name).as_deref(), Some("Edit"));
        assert_eq!(idle.plan, Some(plan));
        let none = f.view("local_no_transcript");
        assert_eq!(none.activity, None);
        assert_eq!(none.plan, None);
    }

    #[test]
    fn git_and_pr_are_joined_by_key() {
        let git = GitInfo {
            branch: Some("feature/x".to_owned()),
            repo_root: Some(r"D:\Repos\monitor".to_owned()),
            dirty_files: 3,
            is_default_branch: false,
            checked_at: NOW,
        };
        let mut f = Fixture::new();
        f.settings.include_cli_sessions = true;
        f.live(RegistryEntry {
            cwd: r"D:\Repos\monitor\crates".to_owned(),
            ..entry(1, "local_a")
        })
        .live(cli(7))
        .live(entry(2, "local_b"));
        f.git_pr.insert(
            "local_a".to_owned(),
            (Some(git.clone()), Some(pr("OPEN", ChecksState::Pass))),
        );
        f.git_pr.insert("cli:cli-7".to_owned(), (Some(git.clone()), None));

        let a = f.view("local_a");
        assert_eq!(a.git, Some(git.clone()));
        assert_eq!(a.pr, Some(pr("OPEN", ChecksState::Pass)));
        assert_eq!(a.project, "monitor");
        let c = f.view("cli:cli-7");
        assert_eq!(c.git, Some(git));
        assert_eq!(c.pr, None);
        let b = f.view("local_b");
        assert_eq!(b.git, None);
        assert_eq!(b.pr, None);
    }

    // --- messages ----------------------------------------------------------

    #[test]
    fn outbound_messages_are_split_and_windowed() {
        let window = 6 * HOUR;
        let mut f = Fixture::new();
        f.live(entry(1, "local_a"))
            .file(DesktopSession {
                peer_receipts: vec![
                    receipt("m1", "local_b", NOW - HOUR),
                    PeerReceipt {
                        replied_at: Some(NOW - HOUR),
                        ..receipt("m2", "local_b", NOW - 2 * HOUR)
                    },
                    receipt("m3", "local_c", NOW - window),
                    receipt("m4", "local_c", NOW - window - 1),
                    PeerReceipt {
                        delivery: None,
                        delivered_at: None,
                        ..receipt("m5", "local_b", NOW - 30 * MIN)
                    },
                    PeerReceipt {
                        delivery: None,
                        ..receipt("m6", "local_c", NOW - 3 * HOUR)
                    },
                    PeerReceipt {
                        delivery: None,
                        delivered_at: None,
                        ..receipt("m7", "local_b", NOW - 7 * HOUR)
                    },
                    PeerReceipt {
                        delivery: Some("pending".to_owned()),
                        delivered_at: None,
                        ..receipt("m8", "local_x", NOW - 10 * MIN)
                    },
                    PeerReceipt {
                        delivery: None,
                        delivered_at: None,
                        replied_at: Some(NOW - MIN),
                        ..receipt("m9", "local_b", NOW - 20 * MIN)
                    },
                ],
                ..desk("local_a")
            })
            .file(DesktopSession {
                title: Some("Bravo".to_owned()),
                ..desk("local_b")
            })
            .live(RegistryEntry {
                name: Some("Charlie".to_owned()),
                ..entry(3, "local_c")
            });

        let s = f.snapshot();
        let a = s
            .sessions
            .iter()
            .find(|v| v.key == "local_a")
            .expect("local_a");
        let summary = |links: &[PeerLink]| -> Vec<(String, Option<String>, i64)> {
            links
                .iter()
                .map(|l| (l.peer_host_session_id.clone(), l.peer_title.clone(), l.sent_at))
                .collect()
        };
        assert_eq!(
            summary(&a.messages.awaiting_reply),
            [
                ("local_b".to_owned(), Some("Bravo".to_owned()), NOW - HOUR),
                ("local_c".to_owned(), Some("Charlie".to_owned()), NOW - 3 * HOUR),
                ("local_c".to_owned(), Some("Charlie".to_owned()), NOW - window),
            ]
        );
        assert_eq!(
            summary(&a.messages.undelivered),
            [
                ("local_x".to_owned(), None, NOW - 10 * MIN),
                ("local_b".to_owned(), Some("Bravo".to_owned()), NOW - 30 * MIN),
            ]
        );
        assert_eq!(a.messages.awaiting_reply[0].delivered_at, Some(NOW - HOUR + 1_000));
        assert_eq!(a.messages.undelivered[0].delivered_at, None);
        assert!(a.messages.unread_inbound.is_empty());
        assert_eq!(s.counts.messages_pending, 3);
    }

    #[test]
    fn inbound_messages_come_from_every_desktop_file() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_a"))
            .file(DesktopSession {
                // A message to itself is not inbound.
                peer_receipts: vec![receipt("self", "local_a", NOW - 5 * MIN)],
                ..desk("local_a")
            })
            // Archived, not live, unread: counted.
            .file(DesktopSession {
                title: Some("Archivée".to_owned()),
                is_archived: true,
                peer_receipts: vec![receipt("i1", "local_a", NOW - HOUR)],
                ..desk("local_s1")
            })
            // Already read: not counted.
            .file(DesktopSession {
                peer_receipts: vec![PeerReceipt {
                    read_at: Some(NOW - MIN),
                    ..receipt("i2", "local_a", NOW - 2 * HOUR)
                }],
                ..desk("local_s2")
            })
            // Too old, or for someone else: not counted.
            .file(DesktopSession {
                peer_receipts: vec![
                    receipt("i3", "local_a", NOW - 7 * HOUR),
                    receipt("i4", "local_other", NOW - HOUR),
                    receipt("i5", "  ", NOW - HOUR),
                ],
                ..desk("local_s3")
            })
            // Live sender, delivery unknown, target with blanks: counted.
            .file(DesktopSession {
                title: None,
                peer_receipts: vec![PeerReceipt {
                    delivery: None,
                    delivered_at: None,
                    ..receipt("i6", " local_a ", NOW - 30 * MIN)
                }],
                ..desk("local_s4")
            })
            .live(RegistryEntry {
                name: Some("Expéditeur".to_owned()),
                ..entry(4, "local_s4")
            });
        // A file whose sessionId is missing is known by its map key.
        f.desktop.insert(
            "local_s5".to_owned(),
            DesktopSession {
                host_session_id: String::new(),
                peer_receipts: vec![receipt("i7", "local_a", NOW - 3 * HOUR)],
                ..Default::default()
            },
        );

        let a = f.view("local_a");
        let inbound: Vec<(&str, Option<&str>, i64)> = a
            .messages
            .unread_inbound
            .iter()
            .map(|l| (l.peer_host_session_id.as_str(), l.peer_title.as_deref(), l.sent_at))
            .collect();
        assert_eq!(
            inbound,
            [
                ("local_s4", Some("Expéditeur"), NOW - 30 * MIN),
                ("local_s1", Some("Archivée"), NOW - HOUR),
                ("local_s5", None, NOW - 3 * HOUR),
            ]
        );
        assert_eq!(a.messages.unread_inbound[1].delivered_at, Some(NOW - HOUR + 1_000));
        assert_eq!(a.messages.unread_inbound[1].read_at, None);
        // The self-message is still awaiting a reply on the sender side.
        assert_eq!(a.messages.awaiting_reply.len(), 1);
        assert!(f.view("local_s4").messages.unread_inbound.is_empty());
    }

    #[test]
    fn awaiting_reply_resolves_when_the_recipient_answered_in_any_way() {
        let read = |id: &str, target: &str, sent: i64, read: Option<i64>| PeerReceipt {
            read_at: read,
            ..receipt(id, target, sent)
        };
        let mut f = Fixture::new();
        f.live(entry(1, "local_a")).file(DesktopSession {
            peer_receipts: vec![
                // Recipient archived: hidden while it is.
                read("archived", "local_arch", NOW - HOUR, Some(NOW - 50 * MIN)),
                // Recipient wrote back to this session afterwards.
                read("answered", "local_back", NOW - HOUR, Some(NOW - 50 * MIN)),
                // Read, and the recipient finished a turn since.
                read("handled", "local_done", NOW - HOUR, Some(NOW - 50 * MIN)),
                // Read, but the recipient is still working on it.
                read("working", "local_busy", NOW - HOUR, Some(NOW - 50 * MIN)),
                // Not read yet.
                read("unread", "local_done", NOW - 10 * MIN, None),
                // Recipient without a Desktop file.
                read("unknown", "local_nowhere", NOW - HOUR, Some(NOW - 50 * MIN)),
            ],
            ..desk("local_a")
        });
        f.file(DesktopSession {
            is_archived: true,
            last_turn_edge_at: Some(NOW - 40 * MIN),
            ..desk("local_arch")
        })
        .file(DesktopSession {
            // An older message back does not count, a newer one does.
            peer_receipts: vec![
                receipt("old", "local_a", NOW - 2 * HOUR),
                receipt("reply", "local_a", NOW - 30 * MIN),
            ],
            last_turn_edge_at: Some(NOW - 55 * MIN),
            ..desk("local_back")
        })
        .file(DesktopSession {
            last_turn_edge_at: Some(NOW - 40 * MIN),
            ..desk("local_done")
        })
        .file(DesktopSession {
            last_turn_edge_at: Some(NOW - 45 * MIN),
            ..desk("local_busy")
        })
        .live(with_status(entry(5, "local_busy"), "busy"));
        let s = f.snapshot();
        let a = s.sessions.iter().find(|v| v.key == "local_a").expect("local_a");
        let peers: Vec<&str> = a
            .messages
            .awaiting_reply
            .iter()
            .map(|l| l.peer_host_session_id.as_str())
            .collect();
        assert_eq!(peers, ["local_done", "local_busy", "local_nowhere"]);
        assert_eq!(a.messages.awaiting_reply[0].sent_at, NOW - 10 * MIN);
        // local_back has no process and is unseen, so it is listed too, and
        // its reply to local_a awaits an answer.
        assert!(s.sessions.iter().any(|v| v.key == "local_back" && v.stopped));
        assert_eq!(s.counts.messages_pending, 4);

        // Unarchived again: awaiting again (archiving is not final).
        if let Some(arch) = f.desktop.get_mut("local_arch") {
            arch.is_archived = false;
            arch.last_turn_edge_at = Some(NOW - 55 * MIN);
        }
        assert_eq!(f.view("local_a").messages.awaiting_reply.len(), 4);
    }

    #[test]
    fn queued_side_session_notes_are_unread_inbound() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_parent")).file(DesktopSession {
            queued_notes: vec![
                SideNote {
                    session_id: "local_child".to_owned(),
                    title: Some("Titre de la note".to_owned()),
                    at: NOW - 20 * MIN,
                },
                SideNote {
                    session_id: "local_gone".to_owned(),
                    title: Some(" Enfant archivé ".to_owned()),
                    at: NOW - 10 * MIN,
                },
                // Too old, or from itself: skipped.
                SideNote {
                    session_id: "local_child".to_owned(),
                    title: None,
                    at: NOW - 7 * HOUR,
                },
                SideNote {
                    session_id: "local_parent".to_owned(),
                    title: None,
                    at: NOW - MIN,
                },
            ],
            ..desk("local_parent")
        });
        f.file(DesktopSession {
            title: Some("Enfant".to_owned()),
            ..desk("local_child")
        });
        let inbound = f.view("local_parent").messages.unread_inbound;
        let got: Vec<(&str, Option<&str>, i64, Option<i64>)> = inbound
            .iter()
            .map(|l| {
                (
                    l.peer_host_session_id.as_str(),
                    l.peer_title.as_deref(),
                    l.sent_at,
                    l.read_at,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("local_gone", Some("Enfant archivé"), NOW - 10 * MIN, None),
                ("local_child", Some("Enfant"), NOW - 20 * MIN, None),
            ]
        );
    }

    #[test]
    fn message_window_follows_the_settings() {
        let mut f = Fixture::new();
        f.live(entry(1, "local_a")).file(DesktopSession {
            peer_receipts: vec![
                receipt("recent", "local_b", NOW - 30 * MIN),
                receipt("old", "local_b", NOW - 2 * HOUR),
            ],
            ..desk("local_a")
        });
        f.live(entry(2, "local_b"));

        f.settings.message_window_hours = 1;
        assert_eq!(f.view("local_a").messages.awaiting_reply.len(), 1);
        assert_eq!(f.view("local_b").messages.unread_inbound.len(), 1);

        f.settings.message_window_hours = 3;
        assert_eq!(f.view("local_a").messages.awaiting_reply.len(), 2);
        assert_eq!(f.view("local_b").messages.unread_inbound.len(), 2);

        f.settings.message_window_hours = 0;
        assert!(f.view("local_a").messages.awaiting_reply.is_empty());
        assert!(f.view("local_b").messages.unread_inbound.is_empty());

        f.settings.message_window_hours = u64::MAX;
        assert_eq!(f.view("local_a").messages.awaiting_reply.len(), 2);
    }

    #[test]
    fn message_cutoff_saturates() {
        assert_eq!(message_cutoff(NOW, 1), NOW - HOUR);
        assert_eq!(message_cutoff(NOW, 0), NOW);
        assert_eq!(message_cutoff(NOW, u64::MAX), NOW - i64::MAX);
        assert_eq!(message_cutoff(-10, u64::MAX), i64::MIN);
        assert_eq!(message_cutoff(i64::MIN, 5), i64::MIN);
    }

    // --- order and counts --------------------------------------------------

    #[test]
    fn sessions_are_sorted_by_urgency() {
        let mut f = Fixture::new();
        let waiting = |pid: u32, host: &str, since: Option<i64>| RegistryEntry {
            status_updated_at: since,
            updated_at: None,
            ..with_status(entry(pid, host), "waiting")
        };
        let busy = |pid: u32, host: &str, since: i64| RegistryEntry {
            status_updated_at: Some(since),
            ..with_status(entry(pid, host), "busy")
        };
        let finished = |host: &str, edge: i64, seen: i64, activity: Option<i64>| DesktopSession {
            last_turn_edge_at: Some(edge),
            last_focused_at: Some(seen),
            last_activity_at: activity,
            ..desk(host)
        };
        f.live(waiting(1, "w_new", Some(NOW - 5 * MIN)))
            .live(waiting(2, "w_none", None))
            .live(waiting(3, "w_old", Some(NOW - 20 * MIN)))
            .live(busy(4, "k_old", NOW - 40 * MIN))
            .live(busy(5, "k_new", NOW - MIN))
            .live(entry(6, "u_old"))
            .file(finished("u_old", NOW - 30 * MIN, NOW - HOUR, None))
            .live(entry(7, "u_new"))
            .file(finished("u_new", NOW - 10 * MIN, NOW - HOUR, None))
            .live(entry(8, "err"))
            .file(DesktopSession {
                error: error_at(NOW - 2 * HOUR, "boom"),
                latest_user_frame_at: Some(NOW - 3 * HOUR),
                last_turn_edge_at: Some(NOW - 2 * HOUR),
                ..desk("err")
            })
            .live(entry(9, "i_none"))
            .live(entry(10, "i_old"))
            .file(finished("i_old", NOW - 4 * HOUR, NOW - MIN, Some(NOW - 3 * HOUR)))
            .live(entry(11, "i_recent"))
            .file(finished("i_recent", NOW - 4 * HOUR, NOW - MIN, Some(NOW - 2 * MIN)));
        assert_eq!(
            f.keys(),
            [
                "w_old", "w_new", "w_none", "err", "u_new", "u_old", "k_new", "k_old",
                "i_recent", "i_old", "i_none"
            ]
        );
    }

    #[test]
    fn ties_are_broken_by_title_then_key() {
        let named = |pid: u32, host: &str, name: &str| RegistryEntry {
            name: Some(name.to_owned()),
            ..entry(pid, host)
        };
        let mut f = Fixture::new();
        f.live(named(1, "local_z", "beta"))
            .live(named(2, "local_y", "Alpha"))
            .live(named(3, "local_x", "beta"))
            .live(named(4, "local_w", "Beta"));
        assert_eq!(f.keys(), ["local_y", "local_w", "local_x", "local_z"]);
        // HashMap order must not leak into the result.
        assert_eq!(f.snapshot(), f.snapshot());
    }

    #[test]
    fn counts_summarize_the_list() {
        let mut f = Fixture::new();
        f.live(with_status(entry(1, "local_wait"), "waiting"))
            .live(with_status(entry(2, "local_busy1"), "busy"))
            .live(with_status(entry(3, "local_busy2"), "busy"))
            .live(entry(4, "local_idle"))
            .live(entry(5, "local_unseen"))
            .file(DesktopSession {
                last_focused_at: None,
                ..desk("local_unseen")
            })
            .live(entry(6, "local_err"))
            .file(DesktopSession {
                error: error_at(NOW, "boom"),
                ..desk("local_err")
            });
        f.git_pr.insert(
            "local_wait".to_owned(),
            (None, Some(pr("OPEN", ChecksState::Fail))),
        );
        f.git_pr.insert(
            "local_busy1".to_owned(),
            (None, Some(pr("OPEN", ChecksState::Pass))),
        );
        f.git_pr.insert(
            "local_busy2".to_owned(),
            (None, Some(pr(" open ", ChecksState::Pending))),
        );
        f.git_pr.insert(
            "local_idle".to_owned(),
            (None, Some(pr("MERGED", ChecksState::Fail))),
        );
        f.git_pr.insert("local_unseen".to_owned(), (None, None));
        let s = f.snapshot();
        assert_eq!(
            s.counts,
            Counts {
                total: 6,
                waiting: 1,
                error: 1,
                unseen: 1,
                working: 2,
                idle: 1,
                pr_open: 3,
                pr_failing: 1,
                messages_pending: 0,
            }
        );
    }

    #[test]
    fn odd_values_do_not_panic() {
        let mut f = Fixture::new();
        f.now = 0;
        f.settings.message_window_hours = u64::MAX;
        f.live(RegistryEntry {
            pid: 0,
            session_id: String::new(),
            cwd: String::new(),
            started_at: Some(i64::MIN),
            proc_start: Some("-5".to_owned()),
            version: Some(String::new()),
            kind: None,
            entrypoint: Some("claude-desktop".to_owned()),
            host_session_id: Some("local_odd".to_owned()),
            name: None,
            status: Some("busy".to_owned()),
            waiting_for: Some(String::new()),
            updated_at: Some(i64::MAX),
            status_updated_at: Some(i64::MIN),
        })
        .live(RegistryEntry {
            started_at: Some(i64::MAX),
            proc_start: Some(u64::MAX.to_string()),
            ..entry(u32::MAX, "local_big")
        })
        .live(RegistryEntry {
            started_at: None,
            proc_start: Some(u64::MAX.to_string()),
            ..entry(1, "local_big")
        })
        .file(DesktopSession {
            host_session_id: "local_odd".to_owned(),
            completed_turns: Some(u32::MAX),
            last_turn_edge_at: Some(i64::MAX),
            error: Some(ErrorMark {
                at: Some(i64::MIN),
                label: None,
            }),
            latest_user_frame_at: Some(i64::MAX),
            spawned_from: Some(SessionRef::default()),
            peer_receipts: vec![
                receipt("a", "", i64::MIN),
                PeerReceipt {
                    message_id: "b".to_owned(),
                    target_session_id: "local_big".to_owned(),
                    sent_at: i64::MAX,
                    delivered_at: Some(i64::MIN),
                    ..Default::default()
                },
                PeerReceipt::default(),
            ],
            ..Default::default()
        });
        let s = f.snapshot();
        assert_eq!(s.sessions.len(), 2);
        assert_eq!(s.counts.total, 2);
        let odd = f.view("local_odd");
        assert_eq!(odd.title, "Session");
        assert_eq!(odd.state, Working);
        assert_eq!(odd.parent, None);
        assert_eq!(odd.version, None);
        assert_eq!(odd.waiting_for, None);
        let big = f.view("local_big");
        assert_eq!(big.pid, u32::MAX);
        assert_eq!(big.messages.unread_inbound.len(), 1);
    }

    /// Builds a snapshot from the real sources (no acks, no engine edges) and
    /// prints every session. Read-only:
    /// `cargo test live_snapshot -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the live ~/.claude and Claude Desktop folders"]
    fn live_snapshot() {
        use crate::desktop::DesktopIndex;
        use crate::transcript::TranscriptTracker;
        let registry =
            crate::registry::read_live(&crate::paths::sessions_dir()).unwrap_or_default();
        let mut index = DesktopIndex::new(crate::paths::desktop_sessions_root());
        let _ = index.refresh();
        let mut tracker = TranscriptTracker::new(crate::paths::projects_dir());
        let transcripts: HashMap<String, TranscriptInfo> = registry
            .iter()
            .filter_map(|e| Some((e.session_id.clone(), tracker.update(&e.session_id)?)))
            .collect();
        let settings = Settings::default();
        let snapshot = build_snapshot(&AggregateInput {
            now_ms: crate::paths::now_ms(),
            registry: &registry,
            desktop: index.sessions(),
            transcripts: &transcripts,
            agents: &HashMap::new(),
            git_pr: &HashMap::new(),
            acks: &HashMap::new(),
            on_screen: &HashSet::new(),
            turn_ended_at: &HashMap::new(),
            settings: &settings,
            health: SourceHealth::default(),
        });
        for view in &snapshot.sessions {
            let entry = registry.iter().find(|e| e.session_id == view.cli_session_id);
            let desktop = view.host_session_id.as_ref().and_then(|h| index.sessions().get(h));
            let transcript = transcripts.get(&view.cli_session_id);
            println!(
                "{} [{}] registry={:?} -> {:?} {:?} since {:?}",
                view.title,
                view.key,
                entry.and_then(|e| e.status.as_deref()),
                view.state,
                view.waiting_for,
                view.state_since
            );
            if let Some(q) = &view.question {
                println!("    question ({}): {} {:?}", q.source, q.text, q.options);
            }
            if let Some(error) = &view.last_error {
                println!("    error: {error}");
            }
            if let Some(recap) = &view.recap {
                println!("    recap: {}", recap.chars().take(110).collect::<String>());
            }
            if let Some(plan) = &view.plan {
                println!(
                    "    plan {}/{} current={:?} wrapped={}",
                    plan.done, plan.total, plan.current, plan.wrapped_up
                );
            }
            // A Desktop frame newer than the transcript's latest turn start
            // would hide a pending question (the person may have answered).
            let start = transcript.and_then(|t| t.turn.as_ref()?.started_at);
            let frame = desktop.and_then(|d| d.latest_user_frame_at);
            if let (Some(start), Some(frame)) = (start, frame) {
                println!("    desktop frame - transcript turn start = {} ms", frame - start);
            }
            if let Some(t) = transcript {
                println!(
                    "    turn={:?} human_prompt={:?} notable_end={:?} ask={:?}",
                    t.turn.as_ref().map(|t| (t.origin, t.ended, t.ended_at)),
                    t.last_human_prompt_at,
                    t.notable_turn_ended_at,
                    t.pending_ask.as_ref().map(|q| &q.text)
                );
            }
        }
        println!("counts: {:?}", snapshot.counts);

        // Every non-archived Desktop session, as if its process were idle
        // and its transcript unknown (a session the person reopens).
        let mut states: HashMap<String, u32> = HashMap::new();
        for d in index.sessions().values().filter(|d| !d.is_archived) {
            let derived = derive_state(&StateInput {
                now_ms: crate::paths::now_ms(),
                status: Some("idle"),
                desktop: Some(d),
                ..StateInput::default()
            });
            let key = format!("{:?} {:?}", derived.state, derived.waiting_for);
            *states.entry(key).or_default() += 1;
            if matches!(derived.state, Error | Waiting) {
                println!(
                    "reopened {} {:?}: {:?} {:?} {:?}",
                    d.host_session_id,
                    d.title,
                    derived.state,
                    derived.error,
                    derived.question.map(|q| q.text)
                );
            }
        }
        println!("reopened sessions: {states:?}");
    }

    // --- transitions -------------------------------------------------------

    fn view(key: &str, state: SessionState, waiting_for: Option<&str>) -> SessionView {
        SessionView {
            key: key.to_owned(),
            title: format!("Titre {key}"),
            state,
            waiting_for: waiting_for.map(str::to_owned),
            ..Default::default()
        }
    }

    fn snap(sessions: Vec<SessionView>) -> Snapshot {
        Snapshot {
            sessions,
            ..Default::default()
        }
    }

    #[test]
    fn first_snapshot_reports_every_session() {
        let next = snap(vec![view("a", Working, None), view("b", Idle, None)]);
        assert_eq!(
            transitions(None, &next),
            [
                Transition {
                    key: "a".to_owned(),
                    title: "Titre a".to_owned(),
                    from: None,
                    to: Working,
                    waiting_for: None,
                    ..Transition::default()
                },
                Transition {
                    key: "b".to_owned(),
                    title: "Titre b".to_owned(),
                    from: None,
                    to: Idle,
                    waiting_for: None,
                    ..Transition::default()
                },
            ]
        );
        assert!(transitions(None, &snap(Vec::new())).is_empty());
    }

    #[test]
    fn changes_and_new_sessions_are_reported() {
        let prev = snap(vec![
            view("a", Working, None),
            view("b", Idle, None),
            view("gone", Waiting, Some("input needed")),
            view("w", Waiting, Some("permission prompt")),
        ]);
        let next = snap(vec![
            view("a", Waiting, Some("permission prompt")),
            view("b", Idle, None),
            view("new", Working, None),
            view("w", Waiting, Some("input needed")),
        ]);
        assert_eq!(
            transitions(Some(&prev), &next),
            [
                Transition {
                    key: "a".to_owned(),
                    title: "Titre a".to_owned(),
                    from: Some(Working),
                    to: Waiting,
                    waiting_for: Some("permission prompt".to_owned()),
                    ..Transition::default()
                },
                Transition {
                    key: "new".to_owned(),
                    title: "Titre new".to_owned(),
                    from: None,
                    to: Working,
                    waiting_for: None,
                    ..Transition::default()
                },
            ]
        );
    }

    #[test]
    fn unchanged_or_vanished_sessions_are_silent() {
        let prev = snap(vec![view("a", Unseen, None), view("b", Error, None)]);
        let next = snap(vec![view("a", Unseen, None)]);
        assert!(transitions(Some(&prev), &next).is_empty());
        assert!(transitions(Some(&prev), &snap(Vec::new())).is_empty());
    }

    #[test]
    fn transitions_between_built_snapshots() {
        let mut f = Fixture::new();
        f.live(with_status(entry(1, "local_a"), "busy"));
        let before = f.snapshot();
        f.registry[0].status = Some("waiting".to_owned());
        f.registry[0].waiting_for = Some("input needed".to_owned());
        let after = f.snapshot();
        let changes = transitions(Some(&before), &after);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].from, Some(Working));
        assert_eq!(changes[0].to, Waiting);
        assert_eq!(changes[0].title, "Session 1");
        assert_eq!(changes[0].waiting_for.as_deref(), Some("input needed"));
    }
}
