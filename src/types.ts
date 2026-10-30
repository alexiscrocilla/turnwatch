// Mirror of crates/monitor-core/src/model.rs (view model and settings only).
// Any change there must be made here too. Timestamps are epoch milliseconds.

export type SessionState = "waiting" | "error" | "unseen" | "working" | "idle";
export type ChecksState = "pass" | "fail" | "pending" | "none";

export interface ToolActivity {
  name: string;
  summary: string;
  at: number | null;
}

export interface PlanItem {
  content: string;
  status: "pending" | "in_progress" | "completed" | string;
}

export interface PlanProgress {
  items: PlanItem[];
  done: number;
  total: number;
  current: string | null;
  updatedAt: number | null;
  /** wrap_up was called after the latest change. Information only: statuses
   * are exactly Desktop's (wrap_up ticks nothing off). */
  wrappedUp: boolean;
}

export interface GitInfo {
  branch: string | null;
  repoRoot: string | null;
  dirtyFiles: number;
  isDefaultBranch: boolean;
  checkedAt: number;
}

export interface PrInfo {
  number: number;
  url: string;
  title: string | null;
  repository: string | null;
  state: "OPEN" | "MERGED" | "CLOSED" | string;
  isDraft: boolean;
  reviewDecision: string | null;
  checks: ChecksState;
  mergeState: string | null;
  checkedAt: number;
  source: "desktop" | "gh-branch" | string;
  /** Desktop's Auto-fix watches this open PR (and Desktop runs): a failing CI, a
   * conflict or a review comment wakes the session to handle it. */
  autoFix: boolean;
}

export type BackgroundKind = "command" | "agent" | "workflow" | "monitor" | "wakeup" | "sessions";

/** What a session whose turn is over waits for, the person apart: it starts again on its own. */
export interface BackgroundWait {
  kind: BackgroundKind;
  /** The main piece of work: a command, an agent's description, a session's title… */
  summary: string;
  /** How many pieces of work it waits for, that one included. */
  count: number;
  /** When that piece of work ends on its own: a wake-up's time, a monitor's timeout. */
  endsAt: number | null;
}

/** A code review's findings, as the session reported them. */
export interface FindingsReport {
  count: number;
  /** Verified as real. */
  confirmed: number;
  /** Not handled yet (fixed, skipped…). */
  open: number;
  at: number | null;
}

export interface SessionRef {
  hostSessionId: string;
  title: string | null;
}

export interface PeerLink {
  peerHostSessionId: string;
  peerTitle: string | null;
  sentAt: number;
  deliveredAt: number | null;
  readAt: number | null;
}

/** Question the session ended its turn on and that the user has not answered. */
export interface PendingQuestion {
  text: string;
  /** Labels of the answer buttons, if any. */
  options: string[];
  source: "wrap_up" | "desktop" | "text" | "offer" | string;
  at: number | null;
}

export interface MessageSummary {
  awaitingReply: PeerLink[];
  undelivered: PeerLink[];
  unreadInbound: PeerLink[];
}

export interface SessionView {
  key: string;
  hostSessionId: string | null;
  cliSessionId: string;
  /** 0 when `stopped`. */
  pid: number;
  isDesktop: boolean;
  /** No live process: Desktop stopped it. Listed only while waiting, in error
   * or unseen; it starts again with the person's next message. */
  stopped: boolean;
  /** Desktop title, else registry name, else folder name. */
  title: string;
  project: string;
  cwd: string;
  worktree: string | null;
  state: SessionState;
  /** Registry reason ("input needed", "permission prompt", "sandbox request",
   * "worker request", "dialog open"), "decision" (turn ended on a question),
   * "probable question" (last message ends with a question mark), "offer"
   * (blocked until the person picks "New session" or "Do it here") or
   * "plan approval" (a plan to approve; `activity` holds its title). */
  waitingFor: string | null;
  question: PendingQuestion | null;
  stateSince: number | null;
  /** Shown in a pane of the Claude Desktop window while it is in front. */
  onScreen: boolean;
  model: string | null;
  effort: string | null;
  permissionMode: string | null;
  completedTurns: number | null;
  lastActivityAt: number | null;
  /** Plain text: Desktop's wrap-up, else the transcript's wrap-up recap,
   * else Desktop's fresh summary, else lastTurnReport, else the last message. */
  recap: string | null;
  activity: ToolActivity | null;
  /** Null when the list belongs to an earlier prompt of the person. */
  plan: PlanProgress | null;
  git: GitInfo | null;
  pr: PrInfo | null;
  messages: MessageSummary;
  /** The session this one comes from in Desktop: started as a side session,
   * forked, or started from one of its "New task" cards. */
  parent: SessionRef | null;
  /** Key of the listed session this one shows under, as Desktop's sidebar
   * nests it: its nearest ancestor in this snapshot. Null at top level. */
  nestUnder: string | null;
  contextExceededCount: number | null;
  lastError: string | null;
  version: string | null;
  /** The latest review's findings, while some are open and the person has not written since. */
  findings: FindingsReport | null;
  /** Why a session is working with no turn running: what it waits for. */
  background: BackgroundWait | null;
  /** Running sub-agents, as the monitor-compagnon mod reports them; empty without it. */
  agents: RunningAgent[];
}

/** A sub-agent a session has running. */
export interface RunningAgent {
  id: string;
  /** The task in a few words. */
  description: string;
  /** "Explore", "general-purpose"… */
  type: string;
}

export interface Counts {
  total: number;
  waiting: number;
  error: number;
  unseen: number;
  working: number;
  idle: number;
  prOpen: number;
  prFailing: number;
  messagesPending: number;
}

export interface SourceHealth {
  registryOk: boolean;
  desktopOk: boolean;
  ghOk: boolean | null;
  errors: string[];
}

export interface Snapshot {
  generatedAt: number;
  /** False until the engine's first pass: an empty list then means "not read yet". */
  ready: boolean;
  /** Increases with every published snapshot: order by this, not generatedAt. */
  seq: number;
  sessions: SessionView[];
  counts: Counts;
  health: SourceHealth;
  /** Plan usage, null when Claude Desktop recorded none. */
  quota: Quota | null;
  /** "New task" cards waiting for the person, newest first. */
  proposedTasks: ProposedTask[];
}

/** A "New task" card a session proposed, still pending in Desktop: the person
 * starts or dismisses it in that session. */
export interface ProposedTask {
  /** "task_…". */
  id: string;
  title: string;
  /** The card's pitch, one line. */
  tldr: string | null;
  createdAt: number;
  /** When Desktop drops it unstarted; null for cards that never expire. */
  expiresAt: number | null;
  /** When the monitor stops listing it: its expiry, at the latest 7 days after it was proposed. */
  listedUntil: number;
  /** Desktop id of the session that proposed it. */
  sessionKey: string;
  sessionTitle: string;
  /** Project of that session, as on its card. */
  project: string;
  /** The person wrote to that session twice since: it no longer asks for attention. */
  passedOver: boolean;
}

/** Plan usage, from the history Claude Desktop keeps and, when installed, the monitor-compagnon mod. */
export interface Quota {
  /** When the latest sample was taken. */
  sampledAt: number;
  fiveHour: QuotaWindow | null;
  sevenDay: QuotaWindow | null;
  /** The mod reports it as sessions run: no sample for a while only means none ran. */
  fedBySessions: boolean;
}

/** One usage window of the plan, as of the latest sample. */
export interface QuotaWindow {
  percent: number;
  /** 5 hours or 7 days. */
  lengthMs: number;
  /** End of the current window: exact from the mod, else the middle of the possible range; null when unknown. */
  resetsAt: number | null;
  /** Half the width of that range; 0 for an exact time. */
  resetMarginMs: number;
  /** The latest the window holding the latest sample can end. */
  endsBy: number;
  /** `endsBy` is past: `percent` is out of date. */
  expired: boolean;
}

export interface Settings {
  notifyOnWaiting: boolean;
  notifyOnFinished: boolean;
  notifyOnError: boolean;
  /** Toast when the plan's usage crosses 80 % / 95 % (5 hours) or 90 % (7 days). */
  notifyOnQuota: boolean;
  includeCliSessions: boolean;
  prPollSecs: number;
  messageWindowHours: number;
  compact: boolean;
  alwaysOnTop: boolean;
}

// Tauri commands (src-tauri/src/lib.rs) and events, shared by both sides.
export const EVENT_SNAPSHOT = "snapshot"; // payload: Snapshot
export const EVENT_SETTINGS = "settings"; // payload: Settings
export const EVENT_VISIBILITY = "visibility"; // payload: boolean, the window was shown (true) or hidden to the tray (false)
export const CMD = {
  getSnapshot: "get_snapshot", // () -> Snapshot
  getSettings: "get_settings", // () -> Settings
  setSettings: "set_settings", // ({ settings }) -> Settings
  openSession: "open_session", // ({ key }) -> void; focuses the session in Claude Desktop (error for non-Desktop keys)
  ackSession: "ack_session", // ({ key }) -> void
  setCompact: "set_compact", // ({ compact }) -> Settings; resizes the window
  setAlwaysOnTop: "set_always_on_top", // ({ value }) -> Settings
  openUrl: "open_url", // ({ url }) -> void; https only
  hideWindow: "hide_window", // () -> void; stays in the tray
  quitApp: "quit_app", // () -> void
} as const;
