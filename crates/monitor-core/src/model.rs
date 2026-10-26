//! Data contract shared by every module. Mirrored by hand in `src/types.ts`:
//! any change here must be made there too.
//!
//! All timestamps are Unix epoch milliseconds (`i64`). JSON uses camelCase.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Raw inputs, one struct per source
// ---------------------------------------------------------------------------

/// One live Claude Code process, from `~/.claude/sessions/<pid>.json`.
/// Undocumented format: every field except `pid` and `session_id` may be missing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryEntry {
    pub pid: u32,
    /// CLI session id. Changes when Desktop restarts the process.
    pub session_id: String,
    pub cwd: String,
    pub started_at: Option<i64>,
    /// Process creation time as a decimal string: `procStartFt`, else
    /// `procStart`. Usually a Windows FILETIME; values above 3e17 are .NET
    /// DateTime ticks in local time.
    pub proc_start: Option<String>,
    pub version: Option<String>,
    /// "interactive" for every session seen so far.
    pub kind: Option<String>,
    /// "claude-desktop" for sessions run by Claude Desktop's Code tab.
    pub entrypoint: Option<String>,
    /// Desktop id ("local_…"), stable across process restarts.
    pub host_session_id: Option<String>,
    pub name: Option<String>,
    /// Raw status: "busy" | "idle" | "waiting" (others possible).
    pub status: Option<String>,
    /// Raw reason when waiting: "input needed", "permission prompt",
    /// "sandbox request", "worker request", "dialog open".
    pub waiting_for: Option<String>,
    pub updated_at: Option<i64>,
    pub status_updated_at: Option<i64>,
}

/// One Desktop session, from
/// `%APPDATA%\Claude\claude-code-sessions\<account>\<org>\local_<id>.json`.
/// Undocumented format: every field except `host_session_id` may be missing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSession {
    /// The file's `sessionId` field ("local_…").
    pub host_session_id: String,
    pub cli_session_id: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub is_archived: bool,
    pub is_starred: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub created_at: Option<i64>,
    pub last_activity_at: Option<i64>,
    /// Set when the user switches to this session in Desktop. Not refreshed
    /// while the user keeps looking at it.
    pub last_focused_at: Option<i64>,
    /// End of the latest turn.
    pub last_turn_edge_at: Option<i64>,
    pub latest_user_frame_at: Option<i64>,
    pub completed_turns: Option<u32>,
    /// `turnWrapUp.description`: the recap of the latest turn.
    pub turn_wrap_up: Option<String>,
    /// `lastTurnReport`: up to 400 characters, sent to linked sessions.
    pub last_turn_report: Option<String>,
    pub branch: Option<String>,
    pub source_branch: Option<String>,
    pub worktree_name: Option<String>,
    pub worktree_path: Option<String>,
    /// PR that Desktop itself bound to the session: the top-level
    /// `prNumber`/`prUrl`/`prRepository`/`prState` fields, else the latest
    /// open (else latest) non-dismissed card of the newer `prs` list.
    pub pr: Option<DesktopPrBinding>,
    /// `spawnedFrom`: the session that started this one. Its `title` is the
    /// title Desktop gave this new session, not the parent's title.
    pub spawned_from: Option<SessionRef>,
    /// A side session another session started: `spawnedFrom.sideSession`
    /// without `spawnedFrom.userFork`, unless `forkedFromSessionId` names
    /// another session (then it is a fork of that one).
    pub side_session: bool,
    /// `forkedFromSessionId`: the session this one was forked from.
    pub forked_from: Option<String>,
    /// `lineageDetached`: the person detached it from its parent in Desktop.
    pub lineage_detached: bool,
    /// Messages this session sent to other sessions.
    pub peer_receipts: Vec<PeerReceipt>,
    /// `queuedSideSessionNotes`: turn reports of other sessions waiting to be
    /// delivered to this one.
    pub queued_notes: Vec<SideNote>,
    pub context_exceeded_count: Option<u32>,
    /// Desktop's own `postTurnSummary`, merged with `turnWrapUp` the way
    /// Desktop does it before showing a session as waiting or failed.
    pub turn_summary: Option<TurnSummary>,
    /// `classifierSummaryEnabled`: Desktop writes a `postTurnSummary` after
    /// each turn.
    pub classifier_summary_enabled: bool,
    /// Desktop's own `error` / `errorCategory` / `errorAt`, the error its
    /// sidebar shows (for 7 days, until the next message).
    pub error: Option<ErrorMark>,
    /// `priorErrorMark`, kept only when a turn was pending (`hadPendingTurn`)
    /// or Desktop waits for the person after an interruption: the other marks
    /// are written when Desktop stops an idle process.
    pub prior_error: Option<ErrorMark>,
    /// Latest entry of `queryCrashes`.
    pub last_crash: Option<ErrorMark>,
    /// `backgroundTaskSuggestions`: the "New task" cards the session
    /// proposed and the person has not acted on (Desktop removes a card from
    /// the list once it is started, dismissed or expired).
    pub suggestions: Vec<TaskSuggestion>,
    /// PRs of the `prs` list that Desktop's Auto-fix watches: `autoFix` on,
    /// not inherited, not dismissed, not merged or closed, on GitHub (the
    /// rule Desktop applies). Desktop wakes the session when their CI fails,
    /// they conflict or get review comments.
    pub auto_fix_prs: Vec<DesktopPrBinding>,
}

/// One entry of `backgroundTaskSuggestions`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSuggestion {
    /// "task_" and 8 hex digits.
    pub id: String,
    pub title: String,
    /// `tldr`: the pitch shown on the card.
    pub tldr: Option<String>,
    pub created_at: i64,
    /// Set on proposals and offers, 7 days after their creation; the older
    /// cards (`spawn_task`) have none and never expire.
    pub expires_at: Option<i64>,
    /// `offer`: the person asked for this task, and the session waits until
    /// they choose "New session" or "Do it here".
    pub offer: bool,
    /// `accepted`: "New session" was chosen; the session writes the brief.
    pub accepted: bool,
    /// `autoStart` is set: a cloud request that starts on its own.
    pub auto_start: bool,
}

/// Desktop's verdict on the latest turn (`DS()` in Desktop's code).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSummary {
    /// "completed" | "review_ready" | "blocked" | "need_input" | "failed"…
    pub category: Option<String>,
    /// `status_detail`: a short recap.
    pub detail: Option<String>,
    /// What the person has to do, when anything.
    pub needs_action: Option<String>,
    /// The summary describes the latest assistant message
    /// (`postTurnSummaryFor` == `lastAssistantUuid`).
    pub fresh: bool,
}

/// An error recorded by Desktop or found in a transcript.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorMark {
    pub at: Option<i64>,
    /// "category: message", one line.
    pub label: Option<String>,
}

/// One entry of `queuedSideSessionNotes`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SideNote {
    /// Desktop id of the session that wrote the report.
    pub session_id: String,
    pub title: Option<String>,
    pub at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopPrBinding {
    pub number: Option<u32>,
    pub url: Option<String>,
    /// "owner/name".
    pub repository: Option<String>,
    /// "OPEN" | "MERGED" | "CLOSED".
    pub state: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRef {
    pub host_session_id: String,
    pub title: Option<String>,
}

/// One entry of a Desktop file's `peerReceipts`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerReceipt {
    pub message_id: String,
    /// Desktop id of the recipient ("local_…").
    pub target_session_id: String,
    pub sent_at: i64,
    /// "delivered" is the only value seen so far.
    pub delivery: Option<String>,
    pub delivered_at: Option<i64>,
    pub read_at: Option<i64>,
    pub replied_at: Option<i64>,
}

/// What the transcript (`~/.claude/projects/<dir>/<cliSessionId>.jsonl`) tells.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptInfo {
    pub path: String,
    pub last_event_at: Option<i64>,
    /// Latest tool call that has no result yet.
    pub current_tool: Option<ToolActivity>,
    /// Latest tool call, finished or not.
    pub last_tool: Option<ToolActivity>,
    /// Task list, None when it was last changed before the person's latest
    /// prompt: Desktop shows it under an older message.
    pub plan: Option<PlanProgress>,
    /// `gitBranch` of the latest line that has one.
    pub git_branch: Option<String>,
    /// Latest assistant text, plain text on one line, at most 200 characters.
    pub last_assistant_text: Option<String>,
    /// The latest turn; None before the first prompt.
    pub turn: Option<TurnInfo>,
    /// Latest prompt written by the person.
    pub last_human_prompt_at: Option<i64>,
    /// End of the latest turn started by the person or by the session's own
    /// background task, not by another session.
    pub notable_turn_ended_at: Option<i64>,
    /// Question asked through `wrap_up` (`ask` or `blocked`) that the person
    /// has not answered: kept across turns other sessions or tasks start,
    /// cleared by the person's next prompt or a `wrap_up` without question.
    pub pending_ask: Option<PendingQuestion>,
    /// API error (rate limit, expired login…) that ended a turn, until the
    /// person's next prompt or a successful answer in the same turn.
    pub api_error: Option<ErrorMark>,
    /// The latest code review the session reported (`ReportFindings`, call
    /// accepted), until the person's next prompt.
    pub findings: Option<FindingsReport>,
}

/// A code review's findings, as the session reported them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindingsReport {
    /// Findings reported.
    pub count: u32,
    /// Of them, verified as real (`verdict` "CONFIRMED").
    pub confirmed: u32,
    /// Of them, not handled yet (no `outcome`: fixed, skipped…).
    pub open: u32,
    pub at: Option<i64>,
}

/// Who started a turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnOrigin {
    /// The person (or the session's first prompt).
    #[default]
    Human,
    /// A background task of the session itself finished (`<task-notification>`).
    Task,
    /// Another session or Desktop woke it (child ping, peer message, reminder).
    Peer,
}

/// The latest turn as the transcript shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInfo {
    pub started_at: Option<i64>,
    pub origin: TurnOrigin,
    /// The turn is over: a stop marker, an interruption or a final API error
    /// came after its prompt.
    pub ended: bool,
    pub ended_at: Option<i64>,
    /// `wrap_up` was called in this turn (with or without a question).
    pub wrapped_up: bool,
    /// That `wrap_up`'s recap, plain text on one line.
    pub wrap_up_recap: Option<String>,
    /// Last sentence of the closing message (text after the last tool call)
    /// when it ends with a question mark, plain text on one line.
    pub closing_question: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolActivity {
    /// Short display name: "Bash", "Edit", "update_plan"… (MCP prefix removed).
    pub name: String,
    /// One line, at most 120 characters: the command, file name, pattern…
    pub summary: String,
    pub at: Option<i64>,
}

/// Task list rebuilt from `mcp__ccd_turn__update_plan` / `wrap_up` (Desktop)
/// or `TodoWrite` (CLI).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanProgress {
    pub items: Vec<PlanItem>,
    pub done: u32,
    pub total: u32,
    /// Content of the in-progress item, if any.
    pub current: Option<String>,
    pub updated_at: Option<i64>,
    /// True once `wrap_up` was called after the latest change. Information
    /// only: like Desktop, `wrap_up` changes no status. A later `update_plan`
    /// call sets it back to false.
    pub wrapped_up: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanItem {
    pub content: String,
    /// "pending" | "in_progress" | "completed".
    pub status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitInfo {
    pub branch: Option<String>,
    pub repo_root: Option<String>,
    /// Lines of `git status --porcelain` (modified + untracked).
    pub dirty_files: u32,
    pub is_default_branch: bool,
    pub checked_at: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChecksState {
    Pass,
    Fail,
    Pending,
    #[default]
    None,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrInfo {
    pub number: u32,
    pub url: String,
    pub title: Option<String>,
    /// "owner/name".
    pub repository: Option<String>,
    /// "OPEN" | "MERGED" | "CLOSED".
    pub state: String,
    pub is_draft: bool,
    /// "APPROVED" | "CHANGES_REQUESTED" | "REVIEW_REQUIRED" | None.
    pub review_decision: Option<String>,
    pub checks: ChecksState,
    /// `mergeStateStatus`: "CLEAN" | "BLOCKED" | "DIRTY" | "BEHIND" | "UNSTABLE"…
    pub merge_state: Option<String>,
    pub checked_at: i64,
    /// "desktop" when found through Desktop's binding, "gh-branch" when found
    /// by `gh pr list --head <branch>`.
    pub source: String,
    /// Desktop's Auto-fix watches this open PR and Desktop runs: a failing
    /// CI, a conflict or a review comment wakes the session to handle it.
    /// Set when the snapshot is built.
    #[serde(default)]
    pub auto_fix: bool,
}

// ---------------------------------------------------------------------------
// View model sent to the window
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    /// Blocked on the user: permission, question, dialog…
    Waiting,
    /// The latest turn ended on an error.
    Error,
    /// Turn finished and the user has not looked at it since.
    Unseen,
    Working,
    #[default]
    Idle,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub generated_at: i64,
    /// False until the engine has finished its first pass: an empty list then
    /// means "not read yet", not "no session".
    pub ready: bool,
    /// Increases with every snapshot the engine publishes. Order snapshots by
    /// this, never by `generated_at` (the wall clock can go back).
    pub seq: u64,
    /// Sorted: waiting (longest first), error, unseen, working, idle (most recent first).
    pub sessions: Vec<SessionView>,
    pub counts: Counts,
    pub health: SourceHealth,
    /// Plan usage, None when Claude Desktop recorded none.
    pub quota: Option<Quota>,
    /// "New task" cards waiting for the person, newest first.
    pub proposed_tasks: Vec<ProposedTask>,
}

/// A "New task" card a session proposed, still pending in Desktop. The
/// person starts or dismisses it in the session that proposed it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedTask {
    /// "task_…".
    pub id: String,
    /// Plain text, one line.
    pub title: String,
    /// The card's pitch, plain text on one line.
    pub tldr: Option<String>,
    pub created_at: i64,
    /// When Desktop drops it unstarted; None for cards that never expire.
    pub expires_at: Option<i64>,
    /// When the monitor stops listing it: its expiry, at the latest 7 days
    /// after it was proposed (Desktop keeps the cards without expiry forever).
    pub listed_until: i64,
    /// Desktop id of the session that proposed it.
    pub session_key: String,
    /// That session's title.
    pub session_title: String,
    /// Project of that session, as on its card.
    pub project: String,
}

/// Plan usage, from the history Claude Desktop keeps
/// (`%APPDATA%\Claude\plan-usage-history.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    /// When Desktop recorded the latest sample.
    pub sampled_at: i64,
    /// The 5-hour window.
    pub five_hour: Option<QuotaWindow>,
    /// The 7-day window.
    pub seven_day: Option<QuotaWindow>,
}

/// One usage window of the plan, as of the latest sample.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    /// Percent used.
    pub percent: f64,
    /// Length of the window: 5 hours or 7 days.
    pub length_ms: i64,
    /// Estimated end of the current window, the middle of the range the
    /// history allows. None when nothing is used, or when the history cannot
    /// place it closely enough.
    pub resets_at: Option<i64>,
    /// Half the width of that range.
    pub reset_margin_ms: i64,
    /// The latest the window holding the latest sample can end, from the
    /// reset range when known, else from when the window can have started.
    /// A later sample belongs to another window.
    pub ends_by: i64,
    /// `ends_by` is past: `percent` is out of date.
    pub expired: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub total: u32,
    pub waiting: u32,
    pub error: u32,
    pub unseen: u32,
    pub working: u32,
    pub idle: u32,
    pub pr_open: u32,
    pub pr_failing: u32,
    /// Messages sent and still waiting for a reply, across sessions.
    pub messages_pending: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceHealth {
    pub registry_ok: bool,
    pub desktop_ok: bool,
    /// None until `gh` has been tried.
    pub gh_ok: Option<bool>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    /// `host_session_id`, or `"cli:" + cli_session_id` for sessions outside Desktop.
    pub key: String,
    pub host_session_id: Option<String>,
    pub cli_session_id: String,
    /// 0 when `stopped`.
    pub pid: u32,
    pub is_desktop: bool,
    /// No live process: Desktop stopped it (it keeps only a few alive).
    /// Listed only while waiting, in error or unseen. It starts again with
    /// the person's next message.
    pub stopped: bool,
    /// Desktop title (what its sidebar shows), else registry name, else
    /// folder name.
    pub title: String,
    /// Repository folder name; for a worktree, the repository that owns it.
    pub project: String,
    pub cwd: String,
    pub worktree: Option<String>,
    pub state: SessionState,
    /// Raw registry reason ("input needed", "permission prompt", "sandbox
    /// request", "worker request", "dialog open"), or "decision" when the turn
    /// ended on a question (wrap_up `ask`/`blocked`, Desktop's `blocked`
    /// summary), "probable question" when the last message ends with a
    /// question mark and nothing more reliable says so, "offer" when the
    /// session waits for "New session" or "Do it here" on a task offer, or
    /// "plan approval" when the permission asked is to approve a plan
    /// (`ExitPlanMode`).
    pub waiting_for: Option<String>,
    /// The question the session ended its turn on, when known.
    pub question: Option<PendingQuestion>,
    /// When the current state started.
    pub state_since: Option<i64>,
    /// Shown in a pane of the Claude Desktop window while that window is in
    /// front: the user can already see it.
    pub on_screen: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub completed_turns: Option<u32>,
    pub last_activity_at: Option<i64>,
    /// Plain text: `turn_wrap_up`, else the transcript's wrap-up recap of the
    /// latest turn, else Desktop's fresh summary, else `last_turn_report`,
    /// else the transcript's `last_assistant_text`.
    pub recap: Option<String>,
    /// `current_tool` while working, else `last_tool`.
    pub activity: Option<ToolActivity>,
    pub plan: Option<PlanProgress>,
    pub git: Option<GitInfo>,
    pub pr: Option<PrInfo>,
    pub messages: MessageSummary,
    /// The session this one comes from in Desktop: started as a side
    /// session, forked, or started from one of its "New task" cards.
    pub parent: Option<SessionRef>,
    /// Key of the listed session this one shows under, as Desktop's sidebar
    /// nests it: its nearest ancestor in this snapshot. None at top level.
    pub nest_under: Option<String>,
    pub context_exceeded_count: Option<u32>,
    pub last_error: Option<String>,
    pub version: Option<String>,
    /// The latest code review's findings, while some are not handled and the
    /// person has not written since.
    pub findings: Option<FindingsReport>,
}

/// A question the session asked at the end of its turn and that the user has
/// not answered yet.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingQuestion {
    /// Single line, at most 300 characters.
    pub text: String,
    /// Labels of the answer buttons, if any.
    pub options: Vec<String>,
    /// "wrap_up" | "desktop" (Desktop's post-turn summary) | "text" (last
    /// message) | "offer" (a task offer).
    pub source: String,
    pub at: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummary {
    /// Sent by this session, delivered, no reply yet (within the message window).
    pub awaiting_reply: Vec<PeerLink>,
    /// Sent by this session and not delivered.
    pub undelivered: Vec<PeerLink>,
    /// Sent to this session by others and not read yet.
    pub unread_inbound: Vec<PeerLink>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerLink {
    pub peer_host_session_id: String,
    pub peer_title: Option<String>,
    pub sent_at: i64,
    pub delivered_at: Option<i64>,
    pub read_at: Option<i64>,
}

/// A session whose state changed between two snapshots (drives toasts).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transition {
    pub key: String,
    pub title: String,
    /// None when the session just appeared.
    pub from: Option<SessionState>,
    pub to: SessionState,
    pub waiting_for: Option<String>,
    /// `state_since` of the new state: with `key` and `to`, identifies one
    /// episode, so the same wait seen twice is not announced twice.
    pub state_since: Option<i64>,
    /// Copied from the session: the user can already see it.
    pub on_screen: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub notify_on_waiting: bool,
    pub notify_on_finished: bool,
    pub notify_on_error: bool,
    /// Toast when the plan's usage crosses a threshold (5 hours: 80 % and
    /// 95 %; 7 days: 90 %).
    pub notify_on_quota: bool,
    /// Also list sessions started outside Claude Desktop.
    pub include_cli_sessions: bool,
    pub pr_poll_secs: u64,
    /// Messages older than this are no longer reported as awaiting a reply.
    pub message_window_hours: u64,
    pub compact: bool,
    pub always_on_top: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify_on_waiting: true,
            notify_on_finished: false,
            notify_on_error: true,
            notify_on_quota: true,
            include_cli_sessions: false,
            pr_poll_secs: 60,
            message_window_hours: 6,
            compact: false,
            always_on_top: true,
        }
    }
}
