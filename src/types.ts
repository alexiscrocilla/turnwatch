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
   * "probable question" (last message ends with a question mark) or "offer"
   * (blocked until the person picks "New session" or "Do it here"). */
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
  parent: SessionRef | null;
  contextExceededCount: number | null;
  lastError: string | null;
  version: string | null;
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
}

/** Plan usage, from the history Claude Desktop keeps. */
export interface Quota {
  /** When Desktop recorded the latest sample. */
  sampledAt: number;
  fiveHour: QuotaWindow | null;
  sevenDay: QuotaWindow | null;
}

/** One usage window of the plan, as of the latest sample. */
export interface QuotaWindow {
  percent: number;
  /** 5 hours or 7 days. */
  lengthMs: number;
  /** Estimated end of the current window (middle of the possible range); null when unknown. */
  resetsAt: number | null;
  /** Half the width of that range. */
  resetMarginMs: number;
  /** The window reset after the latest sample: `percent` is out of date. */
  expired: boolean;
}

export interface Settings {
  notifyOnWaiting: boolean;
  notifyOnFinished: boolean;
  notifyOnError: boolean;
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
