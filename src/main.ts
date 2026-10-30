// Window UI: renders the engine's snapshot and sends the user's actions back.
// Runs against the Tauri backend, or against src/mock.ts in a plain browser.

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen } from "@tauri-apps/api/event";
import { dateLocale, getLang, isLang, setLang, t } from "./i18n";
import type { Moment } from "./i18n";
import { miniCrab, paintSprite, propOf } from "./sprites";
import type { Prop } from "./sprites";
import { CMD, EVENT_SETTINGS, EVENT_SNAPSHOT, EVENT_VISIBILITY } from "./types";
import type {
  BackgroundKind,
  BackgroundWait,
  Counts,
  FindingsReport,
  PeerLink,
  PendingQuestion,
  PlanProgress,
  PrInfo,
  ProposedTask,
  Quota,
  QuotaWindow,
  RunningAgent,
  SessionState,
  SessionView,
  Settings,
  Snapshot,
  SourceHealth,
  ToolActivity,
} from "./types";

// ---------------------------------------------------------------------------
// Bridge to the backend
// ---------------------------------------------------------------------------

type InvokeFn = <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;
type ListenFn = (event: string, handler: (payload: unknown) => void) => Promise<void>;

/** Backend seen by the UI: Tauri, or the in-browser mock. */
interface Bridge {
  readonly mock: boolean;
  readonly invoke: InvokeFn;
  readonly listen: ListenFn;
}

async function createBridge(): Promise<Bridge> {
  if ("__TAURI_INTERNALS__" in window) {
    return {
      mock: false,
      invoke: <T>(cmd: string, args?: Record<string, unknown>) => tauriInvoke<T>(cmd, args),
      listen: async (event, handler) => {
        await tauriListen<unknown>(event, (e) => handler(e.payload));
      },
    };
  }
  const { createMockBackend } = await import("./mock");
  const backend = createMockBackend();
  return {
    mock: true,
    invoke: <T>(cmd: string, args?: Record<string, unknown>) => backend.invoke<T>(cmd, args),
    listen: async (event, handler) => {
      backend.listen(event, handler);
    },
  };
}

// ---------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------

const STATES: readonly SessionState[] = ["waiting", "error", "unseen", "working", "idle"];

/** Waiting reasons guessed from the last message alone: shown less assertively. */
const TENTATIVE_REASONS = new Set(["probable question"]);

/** Waiting reasons for which the pending tool call is worth showing. */
const REASONS_WITH_TOOL = new Set(["permission prompt", "sandbox request"]);

/** The values of `Settings.language`. */
const LANGUAGES: readonly string[] = ["auto", "en", "fr"];

/** Label of a raw backend value; undefined for a value the table does not know. */
const labelOf = (table: Record<string, string>, key: string): string | undefined => table[key];

const DEFAULT_SETTINGS: Settings = {
  notifyOnWaiting: true,
  notifyOnFinished: false,
  notifyOnError: true,
  notifyOnQuota: true,
  includeCliSessions: false,
  prPollSecs: 60,
  messageWindowHours: 6,
  compact: false,
  alwaysOnTop: true,
  language: "auto",
};

const reasonKey = (raw: string | null): string => (raw ?? "").trim().toLowerCase();

/** Reason in words for a raw `waitingFor` value; unknown values are shown as is. */
function reasonLabel(raw: string | null): string {
  const key = reasonKey(raw);
  if (!key) return t().states.waiting;
  return labelOf(t().reasons, key) ?? (raw ?? "").trim();
}

/** The plan's title when the session waits for its approval (the pending call is the plan). */
function planTitle(s: SessionView): string | null {
  if (reasonKey(s.waitingFor) !== "plan approval") return null;
  return str(s.activity?.summary).trim() || null;
}

/** Why a session waits, in a few words: the reason, with the plan's title for a plan. */
function reasonText(s: SessionView): string {
  const plan = planTitle(s);
  const reason = reasonLabel(s.waitingFor);
  return plan ? t().labeled(reason, plan) : reason;
}

/** "Review: 13 findings, 5 confirmed", or what is left to handle. */
function findingsText(f: FindingsReport): string {
  const msg = t();
  if (f.open < f.count) return msg.findingsOpen(f.open, f.count);
  if (f.confirmed >= f.count) return msg.findingsAllConfirmed(f.count);
  return msg.findings(f.count, f.confirmed);
}

/** True when the question is only guessed from the wording of the last message. */
function isTentative(s: SessionView): boolean {
  return TENTATIVE_REASONS.has(reasonKey(s.waitingFor)) || s.question?.source === "text";
}

/** "12 s", "3 min", "2 h", "4 d". */
function formatAgo(ms: number): string {
  const msg = t();
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return msg.seconds(s);
  const m = Math.floor(s / 60);
  if (m < 60) return msg.minutes(m);
  const h = Math.floor(m / 60);
  if (h < 24) return msg.hours(h);
  return msg.days(Math.floor(h / 24));
}

/** The time of `ts`, with its date unless that is today. */
function momentOf(ts: number): Moment {
  const date = new Date(ts);
  const time = date.toLocaleTimeString(dateLocale(), { hour: "2-digit", minute: "2-digit" });
  if (date.toDateString() === new Date().toDateString()) return { time, date: null };
  return { time, date: date.toLocaleDateString(dateLocale(), { day: "numeric", month: "short" }) };
}

function formatClock(ts: number): string {
  return t().clock(momentOf(ts));
}

/** A moment after a verb: "at 14:05" today, else "on 29 Oct at 14:05". */
function formatWhen(ts: number): string {
  return t().when(momentOf(ts));
}

/** One-line message of a rejected command (Tauri rejects with strings). */
function errorText(err: unknown): string {
  let text = "";
  if (err instanceof Error) text = err.message;
  else if (typeof err === "string") text = err;
  else if (typeof err === "object" && err !== null && "message" in err && typeof err.message === "string") {
    text = err.message;
  }
  const line = text.replace(/\s+/g, " ").trim() || t().unknownError;
  return line.length > 160 ? `${line.slice(0, 159)}…` : line;
}

const oneLine = (text: string): string => text.replace(/\s+/g, " ").trim();

/**
 * Inline Markdown as plain text: links keep their label, code spans their
 * content, and `**` bold markers go (`__` is left alone: `__init__` is code
 * more often than bold). The backend should already do it; this keeps
 * `[label](#local_…)` from filling the clamped lines if it does not.
 */
function plainText(md: string): string {
  return md
    .split(/(`[^`\n]*`)/)
    .map((part, i) =>
      i % 2 === 1
        ? part.slice(1, -1)
        : part
            .replace(/!?\[([^\]\n]*)\]\([^)\s]*\)/g, "$1")
            .replace(/(^|[^\w*])\*\*(?=\S)(.+?)(?<=\S)\*\*(?![\w*])/g, "$1$2"),
    )
    .join("");
}

/** Tools whose summary is prose written by the agent (Markdown), not a command or a path. */
const isProseTool = (name: string): boolean => /(^|__)wrap_up$/.test(name);

// ---------------------------------------------------------------------------
// Payload sanitizing: the backend is trusted for shape, but stay tolerant
// ---------------------------------------------------------------------------

type Obj = Record<string, unknown>;

const isObj = (v: unknown): v is Obj => typeof v === "object" && v !== null && !Array.isArray(v);
const str = (v: unknown): string => (typeof v === "string" ? v : "");
const num = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);

function peerLinks(v: unknown): PeerLink[] {
  if (!Array.isArray(v)) return [];
  return v.filter((l): l is PeerLink => isObj(l) && typeof l.peerHostSessionId === "string" && num(l.sentAt) !== null);
}

function sanitizePlan(v: unknown): PlanProgress | null {
  if (!isObj(v) || num(v.total) === null || num(v.done) === null) return null;
  const plan = v as unknown as PlanProgress;
  const items = Array.isArray(v.items) ? plan.items.filter((i) => isObj(i) && typeof i.content === "string") : [];
  return { ...plan, items };
}

function sanitizeQuestion(v: unknown): PendingQuestion | null {
  if (!isObj(v)) return null;
  const text = oneLine(plainText(str(v.text)));
  if (!text) return null;
  const options = (Array.isArray(v.options) ? v.options : [])
    .map((o) => oneLine(plainText(str(o))))
    .filter((o) => o !== "");
  return { text, options, source: str(v.source), at: num(v.at) };
}

function sanitizeActivity(v: unknown): ToolActivity | null {
  if (!isObj(v) || typeof v.name !== "string" || v.name.trim() === "") return null;
  const name = v.name.trim();
  const summary = str(v.summary);
  return { name, summary: oneLine(isProseTool(name) ? plainText(summary) : summary), at: num(v.at) };
}

function sanitizeBackground(v: unknown): BackgroundWait | null {
  if (!isObj(v) || typeof v.kind !== "string") return null;
  return {
    kind: v.kind as BackgroundKind,
    summary: oneLine(str(v.summary)).trim(),
    count: Math.max(num(v.count) ?? 1, 1),
    endsAt: num(v.endsAt),
  };
}

/** A review worth showing: some findings, some still open. */
function sanitizeFindings(v: unknown): FindingsReport | null {
  if (!isObj(v)) return null;
  const count = Math.max(num(v.count) ?? 0, 0);
  const open = Math.min(Math.max(num(v.open) ?? 0, 0), count);
  if (count === 0 || open === 0) return null;
  const confirmed = Math.min(Math.max(num(v.confirmed) ?? 0, 0), count);
  return { count, confirmed, open, at: num(v.at) };
}

function sanitizeAgents(v: unknown): RunningAgent[] {
  if (!Array.isArray(v)) return [];
  return v.flatMap((a) =>
    isObj(a) && str(a.id) !== "" ? [{ id: str(a.id), description: oneLine(str(a.description)), type: str(a.type) }] : [],
  );
}

function sanitizeSession(raw: unknown): SessionView | null {
  if (!isObj(raw) || typeof raw.key !== "string" || raw.key === "") return null;
  const s = raw as unknown as SessionView;
  const messages = isObj(raw.messages) ? raw.messages : {};
  const pr = isObj(raw.pr) && num(raw.pr.number) !== null && s.pr ? { ...s.pr, autoFix: raw.pr.autoFix === true } : null;
  return {
    ...s,
    title: str(s.title).trim() || str(s.project).trim() || t().untitledSession,
    project: str(s.project),
    cwd: str(s.cwd),
    // Only Desktop sessions can be opened; "cli:" keys never are.
    isDesktop: s.isDesktop !== false && !raw.key.startsWith("cli:"),
    stopped: raw.stopped === true,
    state: STATES.includes(s.state) ? s.state : "idle",
    waitingFor: str(s.waitingFor).trim() || null,
    question: sanitizeQuestion(raw.question),
    stateSince: num(s.stateSince),
    onScreen: raw.onScreen === true,
    recap: plainText(str(s.recap)).trim() || null,
    lastError: str(s.lastError).trim() || null,
    activity: sanitizeActivity(raw.activity),
    plan: sanitizePlan(raw.plan),
    git: isObj(raw.git) ? s.git : null,
    pr,
    parent: isObj(raw.parent) ? s.parent : null,
    nestUnder: typeof raw.nestUnder === "string" && raw.nestUnder !== "" && raw.nestUnder !== raw.key ? raw.nestUnder : null,
    messages: {
      awaitingReply: peerLinks(messages.awaitingReply),
      undelivered: peerLinks(messages.undelivered),
      unreadInbound: peerLinks(messages.unreadInbound),
    },
    findings: sanitizeFindings(raw.findings),
    background: sanitizeBackground(raw.background),
    agents: sanitizeAgents(raw.agents),
  };
}

function sanitizeHealth(raw: unknown): SourceHealth {
  const h = isObj(raw) ? raw : {};
  return {
    registryOk: h.registryOk !== false,
    desktopOk: h.desktopOk !== false,
    ghOk: typeof h.ghOk === "boolean" ? h.ghOk : null,
    errors: Array.isArray(h.errors) ? h.errors.filter((e): e is string => typeof e === "string" && e.trim() !== "") : [],
  };
}

/** State counts recomputed from the list so the summary always matches it. */
function countSessions(sessions: readonly SessionView[], raw: unknown): Counts {
  const given = isObj(raw) ? raw : {};
  const counts: Counts = {
    total: sessions.length,
    waiting: 0,
    error: 0,
    unseen: 0,
    working: 0,
    idle: 0,
    prOpen: 0,
    prFailing: 0,
    messagesPending: 0,
  };
  for (const s of sessions) {
    counts[s.state] += 1;
    if (s.pr && str(s.pr.state).toUpperCase() === "OPEN") {
      counts.prOpen += 1;
      if (s.pr.checks === "fail") counts.prFailing += 1;
    }
    counts.messagesPending += s.messages.awaitingReply.length;
  }
  counts.prOpen = num(given.prOpen) ?? counts.prOpen;
  counts.prFailing = num(given.prFailing) ?? counts.prFailing;
  counts.messagesPending = num(given.messagesPending) ?? counts.messagesPending;
  return counts;
}

function sanitizeWindow(v: unknown): QuotaWindow | null {
  if (!isObj(v)) return null;
  const percent = num(v.percent);
  const lengthMs = num(v.lengthMs);
  if (percent === null || lengthMs === null || lengthMs <= 0) return null;
  return {
    percent: Math.max(percent, 0),
    lengthMs,
    resetsAt: num(v.resetsAt),
    resetMarginMs: Math.max(num(v.resetMarginMs) ?? 0, 0),
    endsBy: num(v.endsBy) ?? 0,
    expired: v.expired === true,
  };
}

function sanitizeQuota(v: unknown): Quota | null {
  if (!isObj(v)) return null;
  const sampledAt = num(v.sampledAt);
  const fiveHour = sanitizeWindow(v.fiveHour);
  const sevenDay = sanitizeWindow(v.sevenDay);
  if (sampledAt === null || (!fiveHour && !sevenDay)) return null;
  return { sampledAt, fiveHour, sevenDay, fedBySessions: v.fedBySessions === true };
}

function sanitizeTasks(v: unknown): ProposedTask[] {
  if (!Array.isArray(v)) return [];
  const seen = new Set<string>();
  const out: ProposedTask[] = [];
  const noTitle = t().aSession;
  for (const t of v) {
    if (!isObj(t)) continue;
    const id = str(t.id);
    const title = oneLine(str(t.title));
    const sessionKey = str(t.sessionKey).trim();
    const createdAt = num(t.createdAt);
    if (!id || !title || !sessionKey || createdAt === null || seen.has(id)) continue;
    seen.add(id);
    out.push({
      id,
      title,
      tldr: oneLine(str(t.tldr)) || null,
      createdAt,
      expiresAt: num(t.expiresAt),
      listedUntil: num(t.listedUntil) ?? num(t.expiresAt) ?? createdAt,
      sessionKey,
      sessionTitle: oneLine(str(t.sessionTitle)) || noTitle,
      project: str(t.project),
      passedOver: t.passedOver === true,
    });
  }
  return out;
}

function sanitizeSnapshot(raw: unknown): Snapshot | null {
  if (!isObj(raw)) return null;
  const seen = new Set<string>();
  const sessions: SessionView[] = [];
  for (const item of Array.isArray(raw.sessions) ? raw.sessions : []) {
    const s = sanitizeSession(item);
    if (s && !seen.has(s.key)) {
      seen.add(s.key);
      sessions.push(s);
    }
  }
  const generatedAt = num(raw.generatedAt) ?? 0;
  return {
    generatedAt,
    // An older backend without the flag: its pre-start default has generatedAt 0.
    ready: typeof raw.ready === "boolean" ? raw.ready : generatedAt > 0,
    seq: num(raw.seq) ?? 0,
    sessions,
    counts: countSessions(sessions, raw.counts),
    health: sanitizeHealth(raw.health),
    quota: sanitizeQuota(raw.quota),
    proposedTasks: sanitizeTasks(raw.proposedTasks),
    // An older backend without it: the language in use stays.
    lang: isLang(raw.lang) ? raw.lang : getLang(),
  };
}

function sanitizeSettings(raw: unknown, base: Settings): Settings {
  if (!isObj(raw)) return base;
  const out: Settings = { ...base };
  const target = out as unknown as Obj;
  for (const key of Object.keys(base) as Array<keyof Settings>) {
    if (typeof raw[key] === typeof base[key]) target[key] = raw[key];
  }
  if (!LANGUAGES.includes(out.language)) out.language = base.language;
  return out;
}

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

function byId<T extends HTMLElement = HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) throw new Error(`Element #${id} missing from index.html`);
  return el as T;
}

function h<K extends keyof HTMLElementTagNameMap>(tag: K, cls = "", text?: string): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (cls) el.className = cls;
  if (text !== undefined) el.textContent = text;
  return el;
}

/** Marks a non-interactive element as a window drag handle. */
function dragHandle<T extends Element>(el: T): T {
  el.setAttribute("data-tauri-drag-region", "");
  return el;
}

function setText(el: HTMLElement, text: string): void {
  if (el.textContent !== text) el.textContent = text;
}

function setAttr(el: Element, name: string, value: string | null): void {
  if (value === null) el.removeAttribute(name);
  else if (el.getAttribute(name) !== value) el.setAttribute(name, value);
}

const SVG_NS = "http://www.w3.org/2000/svg";
const circle = (cx: number, cy: number, r: number): string =>
  `M${cx - r} ${cy}a${r} ${r} 0 1 0 ${2 * r} 0a${r} ${r} 0 1 0 ${-2 * r} 0`;

const ICONS = {
  branch: [circle(5, 3.5, 1.5), circle(5, 12.5, 1.5), circle(11, 4.5, 1.5), "M5 5v6", "M11 6v.5c0 2-2 3-5.2 4.2"],
  prOpen: [
    circle(4.5, 3.5, 1.5),
    circle(4.5, 12.5, 1.5),
    circle(11.5, 12.5, 1.5),
    "M4.5 5v6",
    "M11.5 11V6.5a2 2 0 0 0-2-2H7.5",
    "M9 3 7.5 4.5 9 6",
  ],
  merged: [circle(4.5, 3.5, 1.5), circle(4.5, 12.5, 1.5), circle(11.5, 9, 1.5), "M4.5 5v6", "M4.5 5c.3 2.6 2.3 4 5.5 4"],
  prClosed: [
    circle(4.5, 3.5, 1.5),
    circle(4.5, 12.5, 1.5),
    circle(11.5, 12.5, 1.5),
    "M4.5 5v6",
    "M10 3.5l3 3m0-3-3 3",
    "M11.5 8.5V11",
  ],
  check: ["M3.5 8.5 6.5 11.5 12.5 4.5"],
  cross: ["M4.5 4.5l7 7m0-7-7 7"],
  clock: [circle(8, 8, 5.5), "M8 5v3.2l2 1.3"],
  warn: ["M8 2.5 14.5 13.5h-13z", "M8 6.5v3", "M8 11.5v.2"],
  eye: ["M1.5 8S3.9 3.5 8 3.5 14.5 8 14.5 8 12.1 12.5 8 12.5 1.5 8 1.5 8z", circle(8, 8, 2)],
  plus: ["M8 3.5v9", "M3.5 8h9"],
  // Points right; turned down while what it folds is shown.
  chevron: ["M6.5 4l4 4-4 4"],
  // Circling arrow: something keeps an eye on it and acts by itself.
  autoFix: ["M12.5 8a4.5 4.5 0 1 1-1.3-3.2", "M12.5 2.5v2.8H9.7"],
} satisfies Record<string, string[]>;

type IconName = keyof typeof ICONS;

/** Inline stroke icon built from constant path data. */
function icon(name: IconName): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  for (const d of ICONS[name]) {
    const path = document.createElementNS(SVG_NS, "path");
    path.setAttribute("d", d);
    svg.append(path);
  }
  return svg;
}

/** Text that the 1 s ticker keeps up to date ("3 min"). */
function sinceSpan(ts: number, cls: string, tooltip: string): HTMLSpanElement {
  const el = h("span", cls, formatAgo(Date.now() - ts));
  el.dataset.since = String(ts);
  el.title = tooltip;
  return el;
}

// ---------------------------------------------------------------------------
// Static elements
// ---------------------------------------------------------------------------

const root = document.documentElement;
const ui = {
  app: byId("app"),
  summary: byId("summary"),
  quota: byId("quota"),
  compactQuota: byId("compact-quota"),
  btnCompact: byId<HTMLButtonElement>("btn-compact"),
  btnPin: byId<HTMLButtonElement>("btn-pin"),
  btnHide: byId<HTMLButtonElement>("btn-hide"),
  compactDot: byId("compact-dot"),
  compactSprite: byId("compact-sprite"),
  compactOpen: byId<HTMLButtonElement>("compact-open"),
  compactOpenTitle: byId("compact-open-title"),
  compactOpenReason: byId("compact-open-reason"),
  compactOpenMore: byId("compact-open-more"),
  compactSummary: byId("compact-summary"),
  compactNotice: byId("compact-notice"),
  compactWarn: byId<HTMLButtonElement>("compact-warn"),
  btnExpand: byId<HTMLButtonElement>("btn-expand"),
  scroller: byId("scroller"),
  cards: byId("cards"),
  empty: byId("empty"),
  emptyTitle: byId("empty-title"),
  emptyHint: byId("empty-hint"),
  health: byId("health"),
  live: byId("live"),
};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

let bridge: Bridge | null = null;
let snapshot: Snapshot | null = null;
/** An event snapshot was applied: a get_snapshot reply with the same seq brings nothing new. */
let gotEvent = false;
let settings: Settings = { ...DEFAULT_SETTINGS };
/** Text that outlives a render: worded when shown, so that it follows the language. */
type Message = () => string;
/** Blocking problem reaching the backend (shown in the health line). */
let fatalError: Message | null = null;
/** Persistent warning about live updates. */
let liveWarning: Message | null = null;
/** Short-lived message after a failed command. */
let notice: Message | null = null;
/** "error" for a failed command, "info" for a hint (not a problem). */
let noticeTone: "error" | "info" = "error";
let noticeTimer = 0;
let renderQueued = false;
/** Something changed since the last render. */
let dirty = false;
/** The shell says the window is shown (false while hidden to the tray). */
let shown = true;
let ticker = 0;
/** The demo's caption; null in Tauri. */
let mockCaption: HTMLElement | null = null;

interface CardPart {
  sig: string;
  el: HTMLElement;
}

interface CardEntry {
  el: HTMLElement;
  /** Full-card button that opens the session; null for sessions outside Desktop. */
  hit: HTMLButtonElement | null;
  /** Hidden text referenced by the button's aria-describedby. */
  desc: HTMLElement | null;
  /** Signature of everything the card displays or announces. */
  sig: string;
  /** Rendered rows by section id, each with its own signature. */
  parts: Map<string, CardPart>;
}
const cardEntries = new Map<string, CardEntry>();
let descSeq = 0;

/** What a session's character shows of its tools. */
interface Walk {
  /** The latest tool call: a new one is a step. */
  mark: string;
  steps: number;
  /** Kept through the tools that are not work of their own (a plan update…). */
  prop: Prop | null;
}
let walks = new Map<string, Walk>();

function nextWalks(sessions: readonly SessionView[]): Map<string, Walk> {
  const next = new Map<string, Walk>();
  for (const s of sessions) {
    const before = walks.get(s.key);
    const a = s.activity;
    const mark = a ? `${a.name}|${a.at ?? ""}|${a.summary}` : "";
    next.set(s.key, {
      mark,
      steps: (before?.steps ?? 0) + (before && before.mark !== mark ? 1 : 0),
      prop: (a && propOf(a.name)) ?? before?.prop ?? null,
    });
  }
  return next;
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/** Runs a backend command; failures are shown as a notice, never thrown. */
async function call(cmd: string, args?: Record<string, unknown>): Promise<{ ok: boolean; value: unknown }> {
  if (!bridge) return { ok: false, value: null };
  try {
    return { ok: true, value: await bridge.invoke<unknown>(cmd, args) };
  } catch (err) {
    showNotice(() => t().labeled(labelOf(t().commandFailures, cmd) ?? t().commandFailed, errorText(err)));
    return { ok: false, value: null };
  }
}

function openSession(key: string): void {
  void call(CMD.openSession, { key });
}

function openUrl(url: string): void {
  void call(CMD.openUrl, { url });
}

async function setCompact(compact: boolean): Promise<void> {
  const previous = settings;
  applySettings({ ...settings, compact });
  const res = await call(CMD.setCompact, { compact });
  applySettings(res.ok ? sanitizeSettings(res.value, settings) : previous);
}

/** Height at or below which the window can only show the compact bar (see styles.css). */
const BAR_MAX_HEIGHT = 72;

async function expand(): Promise<void> {
  // Not compact but too short: the bar shows only because of the window height.
  const forcedBar = !settings.compact && window.innerHeight <= BAR_MAX_HEIGHT;
  await setCompact(false);
  if (!forcedBar) return;
  window.setTimeout(() => {
    if (window.innerHeight <= BAR_MAX_HEIGHT) {
      showNotice(() => t().windowTooShort, "info");
    }
  }, 600);
}

async function setAlwaysOnTop(value: boolean): Promise<void> {
  const previous = settings;
  applySettings({ ...settings, alwaysOnTop: value });
  const res = await call(CMD.setAlwaysOnTop, { value });
  applySettings(res.ok ? sanitizeSettings(res.value, settings) : previous);
}

function showNotice(message: Message, tone: "error" | "info" = "error"): void {
  notice = message;
  noticeTone = tone;
  window.clearTimeout(noticeTimer);
  noticeTimer = window.setTimeout(() => {
    notice = null;
    scheduleRender();
  }, 6000);
  scheduleRender();
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

function applySettings(next: Settings): void {
  const modeChanged = next.compact !== settings.compact;
  settings = next;
  root.classList.toggle("is-compact", next.compact);
  ui.btnPin.setAttribute("aria-pressed", String(next.alwaysOnTop));
  ui.btnPin.title = pinTitle();
  if (modeChanged) {
    keepFocusVisible();
    // The list is not kept up to date while compact.
    scheduleRender();
  }
}

const pinTitle = (): string => (settings.alwaysOnTop ? t().pinOn : t().pinOff);

/** After a mode switch, moves focus off an element that just got hidden. */
function keepFocusVisible(): void {
  const active = document.activeElement;
  if (!(active instanceof HTMLElement) || active === document.body) return;
  if (active.getClientRects().length > 0) return;
  (settings.compact ? ui.btnExpand : ui.btnCompact).focus({ preventScroll: true });
}

// ---------------------------------------------------------------------------
// Visibility: nothing is rendered while the window is hidden to the tray
// ---------------------------------------------------------------------------

const paused = (): boolean => !shown || document.hidden;

/** Runs the 1 s ticker only while the window can be seen. */
function syncTicker(): void {
  if (paused()) {
    window.clearInterval(ticker);
    ticker = 0;
  } else if (!ticker) {
    ticker = window.setInterval(tickTimes, 1000);
  }
}

function onVisibility(): void {
  syncTicker();
  if (paused()) return;
  tickTimes();
  scheduleRender();
}

function setShown(value: boolean): void {
  if (value === shown) return;
  shown = value;
  onVisibility();
  // The shell may have skipped snapshots while hidden: catch up (seq drops a stale reply).
  if (value) void loadSnapshot();
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/** What render() does not reach: the document, the fixed labels of index.html, the demo's caption. */
function renderStatic(): void {
  const msg = t();
  root.lang = getLang();
  ui.btnCompact.title = msg.compactTitle;
  setAttr(ui.btnCompact, "aria-label", msg.compactLabel);
  ui.btnPin.title = pinTitle();
  setAttr(ui.btnPin, "aria-label", msg.pinLabel);
  ui.btnHide.title = msg.hideTitle;
  setAttr(ui.btnHide, "aria-label", msg.hideLabel);
  ui.btnExpand.title = msg.expandTitle;
  setAttr(ui.btnExpand, "aria-label", msg.expandLabel);
  setAttr(ui.scroller, "aria-label", msg.sessionsRegion);
  if (mockCaption) renderMockCaption(mockCaption);
}

function scheduleRender(): void {
  dirty = true;
  if (renderQueued || paused()) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    if (paused() || !dirty) return;
    dirty = false;
    render();
  });
}

function render(): void {
  const rows = healthRows();
  renderSummary();
  renderQuota();
  renderCompact(rows);
  // Hidden while compact: brought up to date when the mode changes.
  if (!settings.compact) renderList();
  renderHealth(rows);
  renderLive(rows);
}

const isReady = (): boolean => snapshot?.ready === true;

function pendingText(): string {
  if (fatalError) return t().engineUnreachable;
  return snapshot ? t().reading : t().connecting;
}

interface SummaryPart {
  text: string;
  tone: SessionState | "task";
}

function summaryParts(c: Counts): SummaryPart[] {
  const msg = t();
  const parts: SummaryPart[] = [];
  if (c.waiting) parts.push({ text: msg.waitingCount(c.waiting), tone: "waiting" });
  if (c.error) parts.push({ text: msg.errorCount(c.error), tone: "error" });
  if (c.working) parts.push({ text: msg.workingCount(c.working), tone: "working" });
  if (c.unseen) parts.push({ text: msg.unseenCount(c.unseen), tone: "unseen" });
  return parts;
}

/** One-line summary when no part applies: empty list or idle sessions only. */
function quietSummary(c: Counts): string {
  if (c.total === 0) return t().noSessions;
  return t().idleSessions(c.idle);
}

function summaryText(c: Counts): string {
  const parts = summaryParts(c);
  return parts.length ? parts.map((p) => p.text).join(" · ") : quietSummary(c);
}

function renderSummary(): void {
  const el = ui.summary;
  const ready = snapshot !== null && isReady();
  const text = snapshot && ready ? summaryText(snapshot.counts) : pendingText();
  if (el.dataset.text === text) return;
  el.dataset.text = text;
  el.title = text;
  const parts = snapshot && ready ? summaryParts(snapshot.counts) : [];
  if (!parts.length) {
    el.replaceChildren(dragHandle(h("span", "summary-quiet", text)));
    return;
  }
  const nodes: HTMLElement[] = [];
  parts.forEach((part, i) => {
    if (i > 0) nodes.push(dragHandle(h("span", "summary-sep", " · ")));
    nodes.push(dragHandle(h("span", `summary-part tone--${part.tone}`, part.text)));
  });
  el.replaceChildren(...nodes);
}

function mostUrgent(sessions: readonly SessionView[]): SessionState | null {
  return STATES.find((state) => sessions.some((s) => s.state === state)) ?? null;
}

/** What a waiting session needs, in a few words: its question if known. */
const waitingText = (s: SessionView): string => s.question?.text ?? reasonText(s);

function renderCompact(rows: readonly HealthRow[]): void {
  const sessions = snapshot?.sessions ?? [];
  const urgent = sessions.find((s) => s.state === mostUrgent(sessions));
  ui.compactDot.hidden = urgent !== undefined;
  ui.compactSprite.hidden = urgent === undefined;
  if (urgent) paintSession(ui.compactSprite, urgent);

  // Health problems and failed commands: a warning button that opens the list.
  const noticeText = notice?.() ?? null;
  const problems = noticeText && noticeTone === "error" ? [...rows, { text: noticeText, tone: "error" as const }] : [...rows];
  ui.compactWarn.hidden = problems.length === 0;
  if (problems.length) {
    const tone = problems.some((r) => r.tone === "error") ? "error" : "warn";
    ui.compactWarn.className = `icon-btn compact-warn compact-warn--${tone}`;
    ui.compactWarn.title = [...problems.map((r) => r.text), t().clickToShowList].join("\n");
    setAttr(ui.compactWarn, "aria-label", t().problems(problems.map((r) => r.text)));
  }

  // A failed command replaces the line for a few seconds, so the click visibly did something.
  setText(ui.compactNotice, noticeText ?? "");
  ui.compactNotice.title = noticeText ?? "";
  ui.compactNotice.hidden = noticeText === null;
  ui.compactNotice.classList.toggle("is-info", noticeTone === "info");

  const waiting = sessions.filter((s) => s.state === "waiting");
  const first = noticeText === null ? waiting[0] : undefined;
  const openHadFocus = document.activeElement === ui.compactOpen;
  ui.compactOpen.hidden = !first;
  ui.compactSummary.hidden = Boolean(first) || noticeText !== null;
  if (openHadFocus && !first) (ui.compactWarn.hidden ? ui.btnExpand : ui.compactWarn).focus({ preventScroll: true });

  if (first) {
    renderCompactOpen(first, waiting.length - 1);
    return;
  }

  delete ui.compactOpen.dataset.key;
  let text = snapshot && isReady() ? summaryText(snapshot.counts) : pendingText();
  const prOpen = snapshot && isReady() ? snapshot.counts.prOpen : 0;
  if (prOpen > 0) text += ` · ${t().prCount(prOpen)}`;
  const tasks = listedTasks().filter((t) => !t.passedOver).length;
  if (tasks > 0) text += ` · ${t().proposedTasks(tasks)}`;
  setText(ui.compactSummary, text);
  ui.compactSummary.title = text;
}

/** The compact line for the most urgent waiting session: "<title> · <question or reason>". */
function renderCompactOpen(first: SessionView, more: number): void {
  const msg = t();
  const reason = reasonText(first);
  const question = first.question?.text ?? null;
  const options = first.question?.options ?? [];
  setText(ui.compactOpenTitle, first.title);
  setText(ui.compactOpenReason, ` · ${waitingText(first)}`);
  ui.compactOpenReason.classList.toggle("is-tentative", isTentative(first));
  setText(ui.compactOpenMore, `+${more}`);
  ui.compactOpenMore.hidden = more <= 0;
  ui.compactOpen.dataset.key = first.key;
  ui.compactOpen.classList.toggle("is-static", !first.isDesktop);
  setAttr(ui.compactOpen, "aria-disabled", first.isDesktop ? null : "true");

  const others = more > 0 ? msg.othersWaiting(more) : null;
  const lines = [first.title, reason];
  if (question) lines.push(question);
  if (options.length) lines.push(msg.suggestedAnswers(options.join(" · ")));
  if (others) lines.push(others);
  if (!first.isDesktop) lines.push(msg.cliTooltip);
  ui.compactOpen.title = lines.join("\n");

  const what = question ? msg.labeled(reason, question) : reason;
  const label = first.isDesktop ? msg.openTitled(first.title, what) : msg.terminalTitled(first.title, what);
  setAttr(ui.compactOpen, "aria-label", others ? `${label}. ${others}` : label);
}

function onCompactOpen(): void {
  const key = ui.compactOpen.dataset.key;
  if (!key) return;
  if (ui.compactOpen.getAttribute("aria-disabled") === "true") {
    const title = ui.compactOpenTitle.textContent ?? "";
    showNotice(() => t().terminalNotice(title), "info");
    return;
  }
  openSession(key);
}

/** Parts read by screen readers, one span each in #live: what needs the user, problems,
 * failed commands. Not the working counts. Empty strings for parts that do not apply. */
function liveParts(rows: readonly HealthRow[]): string[] {
  let waitingLine = "";
  let others = "";
  let errors = "";
  let unseen = "";
  if (snapshot && isReady()) {
    const msg = t();
    const waiting = snapshot.sessions.filter((s) => s.state === "waiting");
    const first = waiting[0];
    if (first) {
      waitingLine = isTentative(first)
        ? msg.liveTentative(first.title, waitingText(first))
        : msg.liveWaiting(first.title, waitingText(first));
      const more = waiting.length - 1;
      if (more > 0) others = msg.othersWaiting(more);
    }
    const c = snapshot.counts;
    if (c.error) errors = msg.errorCount(c.error);
    if (c.unseen) unseen = msg.liveUnseen(c.unseen);
  }
  return [waitingLine, others, errors, unseen, rows[0]?.text ?? "", notice?.() ?? ""].map(sentence);
}

/** Text as a sentence: a final period unless it already ends with one, "?", "!" or "…". */
function sentence(text: string): string {
  const t = text.trim();
  return t === "" || /[.?!…]$/.test(t) ? t : `${t}.`;
}

/** Updates #live part by part (aria-atomic false), so only the part that changed is read:
 * a session being seen does not repeat the waiting question. */
function renderLive(rows: readonly HealthRow[]): void {
  const parts = liveParts(rows);
  while (ui.live.childElementCount < parts.length) ui.live.append(h("span", "live-part"));
  parts.forEach((text, i) => {
    const span = ui.live.children[i] as HTMLElement;
    // A trailing space keeps the parts apart when read as one text.
    setText(span, text ? `${text} ` : "");
  });
}

// ---------------------------------------------------------------------------
// Plan quota: one gauge per window under the title bar, a chip when compact
// ---------------------------------------------------------------------------

/** While Desktop samples the quota, it does so every 15 minutes at most: an
 * older latest sample means it stopped. */
const QUOTA_STALE_MS = 20 * 60_000;
/** From these percents on, a window turns amber, then red. */
const QUOTA_WARN = 75;
const QUOTA_HIGH = 90;
/** The compact bar shows the fullest window from this percent on. */
const QUOTA_COMPACT_MIN = 80;

type QuotaTone = "ok" | "warn" | "high";

interface MeterModel {
  label: string;
  /** Rounded percent; null when the window reset since the latest sample. */
  percent: number | null;
  tone: QuotaTone;
  /** Percent of the window's time already gone, when its reset is known. */
  elapsed: number | null;
  /** Reset shown after the percent: "05:25", "Thu"; empty when unknown. */
  reset: string;
  tooltip: string;
  /** Spoken value. */
  spoken: string;
}

const clock = (ts: number): string => new Date(ts).toLocaleTimeString(dateLocale(), { hour: "2-digit", minute: "2-digit" });

const quotaTone = (percent: number): QuotaTone =>
  percent >= QUOTA_HIGH ? "high" : percent >= QUOTA_WARN ? "warn" : "ok";

/** Share of the window's time already gone, from its estimated reset, to the half percent. */
function elapsedOf(w: QuotaWindow, now: number): number | null {
  if (w.resetsAt === null || w.expired) return null;
  const share = ((now - (w.resetsAt - w.lengthMs)) / w.lengthMs) * 100;
  return Math.round(Math.min(Math.max(share, 0), 100) * 2) / 2;
}

function meterModel(week: boolean, w: QuotaWindow, quota: Quota, now: number): MeterModel {
  const msg = t();
  const label = week ? msg.gaugeWeek : msg.gaugeFiveHours;
  const name = week ? msg.windowWeek : msg.windowFiveHours;
  const shown = Math.round(w.percent);
  const sampled = msg.labeled(quota.fedBySessions ? msg.lastSample : msg.sampledByDesktop, formatClock(quota.sampledAt));
  if (w.expired) {
    return {
      label,
      percent: null,
      tone: "ok",
      elapsed: null,
      reset: "",
      tooltip: `${msg.labeled(name, msg.quotaExpired(shown))}\n${sampled}`,
      spoken: msg.quotaExpiredSpoken,
    };
  }
  const lines = [msg.labeled(name, msg.percentUsed(shown))];
  let reset = "";
  let resetPhrase = "";
  if (w.resetsAt !== null) {
    const lo = w.resetsAt - w.resetMarginMs;
    const hi = w.resetsAt + w.resetMarginMs;
    const earliest = clock(lo);
    const latest = clock(hi);
    const exact = w.resetMarginMs === 0;
    if (week) {
      const day = (ts: number, weekday: "long" | "short"): string =>
        new Date(ts).toLocaleDateString(dateLocale(), { weekday, day: "numeric", month: "short" });
      reset = new Date(w.resetsAt).toLocaleDateString(dateLocale(), { weekday: "short" });
      if (exact) resetPhrase = msg.resetsDayAt(day(lo, "long"), earliest);
      else if (new Date(lo).toDateString() === new Date(hi).toDateString()) {
        resetPhrase = msg.resetsDayBetween(day(lo, "long"), earliest, latest);
      } else {
        resetPhrase = msg.resetsBetween(day(lo, "short"), earliest, day(hi, "short"), latest);
      }
    } else {
      reset = clock(w.resetsAt);
      resetPhrase = exact ? msg.resetsAt(reset) : msg.resetsAround(reset, earliest, latest);
    }
    lines.push(resetPhrase.charAt(0).toUpperCase() + resetPhrase.slice(1));
  } else if (w.percent > 0) {
    lines.push(msg.resetUnknown);
  }
  const elapsed = elapsedOf(w, now);
  if (elapsed !== null) {
    lines.push(msg.timeElapsed(Math.round(elapsed), week));
  }
  lines.push(sampled);
  return {
    label,
    percent: shown,
    tone: quotaTone(w.percent),
    elapsed,
    reset,
    tooltip: lines.join("\n"),
    spoken: [msg.percentUsed(shown), resetPhrase].filter(Boolean).join(", "),
  };
}

function quotaModels(now: number): MeterModel[] {
  const quota = snapshot && isReady() ? snapshot.quota : null;
  if (!quota) return [];
  const out: MeterModel[] = [];
  if (quota.fiveHour) out.push(meterModel(false, quota.fiveHour, quota, now));
  if (quota.sevenDay) out.push(meterModel(true, quota.sevenDay, quota, now));
  return out;
}

function buildMeter(m: MeterModel): HTMLElement {
  const meter = dragHandle(h("div", `meter meter--${m.tone}`));
  meter.title = m.tooltip;
  if (m.percent === null) {
    meter.classList.add("meter--expired");
    meter.setAttribute("role", "img");
    meter.setAttribute("aria-label", t().labeled(t().quotaGauge(m.label), m.spoken));
  } else {
    meter.setAttribute("role", "meter");
    meter.setAttribute("aria-label", t().quotaGauge(m.label));
    meter.setAttribute("aria-valuemin", "0");
    meter.setAttribute("aria-valuemax", "100");
    meter.setAttribute("aria-valuenow", String(Math.min(m.percent, 100)));
    meter.setAttribute("aria-valuetext", m.spoken);
  }
  // The time band first, under the thinner usage bar.
  const track = dragHandle(h("span", "meter-track"));
  if (m.elapsed !== null) {
    const elapsed = dragHandle(h("span", "meter-elapsed"));
    elapsed.style.width = `${m.elapsed}%`;
    track.append(elapsed);
  }
  if (m.percent !== null) {
    const fill = dragHandle(h("span", "meter-fill"));
    fill.style.width = `${Math.min(m.percent, 100)}%`;
    track.append(fill);
  }
  meter.append(
    dragHandle(h("span", "meter-label", m.label)),
    track,
    dragHandle(h("span", "meter-value", m.percent === null ? "—" : t().percent(m.percent))),
  );
  if (m.reset) meter.append(dragHandle(h("span", "meter-reset", m.reset)));
  return meter;
}

/** Called on every render and every second: rebuilds only what changed. */
function renderQuota(): void {
  const now = Date.now();
  const models = quotaModels(now);
  // Fed by sessions, a pause only means none ran: nothing to start again.
  const sampledAt = snapshot?.quota?.fedBySessions ? null : (snapshot?.quota?.sampledAt ?? null);
  const since = models.length && sampledAt !== null && now - sampledAt > QUOTA_STALE_MS ? formatClock(sampledAt) : "";

  ui.quota.hidden = models.length === 0;
  const sig = JSON.stringify([models, since]);
  if (ui.quota.dataset.sig !== sig) {
    ui.quota.dataset.sig = sig;
    ui.quota.classList.toggle("is-stale", since !== "");
    const meters = dragHandle(h("div", "quota-meters"));
    meters.append(...models.map(buildMeter));
    const nodes: HTMLElement[] = [meters];
    if (since) {
      const stale = dragHandle(h("span", "quota-stale", t().quotaStale(since)));
      stale.title = t().quotaResumeHint;
      nodes.push(stale);
    }
    ui.quota.replaceChildren(...nodes);
  }

  // Compact: the fullest window, only when it gets close to the limit.
  const top = models
    .filter((m) => m.percent !== null && m.percent >= QUOTA_COMPACT_MIN)
    .sort((a, b) => (b.percent ?? 0) - (a.percent ?? 0))[0];
  ui.compactQuota.hidden = !top;
  if (top) {
    setText(ui.compactQuota, `${top.label} ${t().percent(top.percent ?? 0)}`);
    ui.compactQuota.className = `compact-quota compact-quota--${top.tone}`;
    ui.compactQuota.title = models.map((m) => m.tooltip).join("\n\n");
  }
}

// ---------------------------------------------------------------------------
// Session list: one group per project, one card per session
// ---------------------------------------------------------------------------

const STATE_RANK: Record<SessionState, number> = { waiting: 0, error: 1, unseen: 2, working: 3, idle: 4 };

interface Group {
  key: string;
  label: string;
  /** Every session of the group, nested ones included. */
  sessions: SessionView[];
  /** Every "New task" card of the group, nested ones included. */
  tasks: ProposedTask[];
  families: Family[];
  /** Tasks whose session is not listed: after the families, newest first. */
  loose: ProposedTask[];
  /** Index of the group's first session in the backend order (tasks count after every session). */
  first: number;
}

/**
 * A session and what hangs from it, nested as in Desktop's sidebar: at one
 * level under the root, each session followed by what hangs from it, its
 * sessions before the tasks it proposed.
 */
interface Family {
  root: SessionView;
  members: Member[];
  /** Index of the family's first session in the backend order. */
  first: number;
}

type Member = { kind: "session"; s: SessionView } | { kind: "task"; t: ProposedTask };

/**
 * Sessions and proposed tasks by project. Groups holding a waiting session come
 * first (the one waiting longest first), then error, unseen, working, idle-only
 * groups, and last the groups with tasks only. A session shows under the one
 * it comes from (`nestUnder`), with the tasks it proposed: the family goes
 * where its root's project is, at the place of its most urgent session, as
 * Desktop places a family at its most recent member. Otherwise the backend
 * order (by urgency) is kept.
 */
function groupSessions(sessions: readonly SessionView[], tasks: readonly ProposedTask[]): Group[] {
  const index = new Map(sessions.map((s, i) => [s.key, i]));
  // Links up, each kept unless it closes a loop: the backend never sends
  // one, but a loop would drop its sessions from the list.
  const up = new Map<string, string>();
  const rootOf = (key: string): string => {
    let k = key;
    for (let hops = 0; hops <= sessions.length; hops++) {
      const next = up.get(k);
      if (next === undefined) break;
      k = next;
    }
    return k;
  };
  for (const s of sessions) {
    const parent = s.nestUnder;
    if (parent !== null && parent !== s.key && index.has(parent) && rootOf(parent) !== s.key) up.set(s.key, parent);
  }
  const children = new Map<string, SessionView[]>();
  for (const s of sessions) {
    const parent = up.get(s.key);
    if (parent !== undefined) children.set(parent, [...(children.get(parent) ?? []), s]);
  }
  const proposed = new Map<string, ProposedTask[]>();
  const loose: ProposedTask[] = [];
  for (const t of tasks) {
    if (index.has(t.sessionKey)) proposed.set(t.sessionKey, [...(proposed.get(t.sessionKey) ?? []), t]);
    else loose.push(t);
  }

  const byKey = new Map<string, Group>();
  const groupOf = (project: string, first: number): Group => {
    const key = project.trim();
    let group = byKey.get(key);
    if (!group) {
      group = { key, label: key || t().noProject, sessions: [], tasks: [], families: [], loose: [], first };
      byKey.set(key, group);
    }
    group.first = Math.min(group.first, first);
    return group;
  };
  for (const root of sessions) {
    if (up.has(root.key)) continue;
    const members: Member[] = [];
    const visit = (key: string): void => {
      for (const s of children.get(key) ?? []) {
        members.push({ kind: "session", s });
        visit(s.key);
      }
      for (const t of proposed.get(key) ?? []) members.push({ kind: "task", t });
    };
    visit(root.key);
    const family = [root, ...members.flatMap((m) => (m.kind === "session" ? [m.s] : []))];
    const first = Math.min(...family.map((s) => index.get(s.key) ?? 0));
    const group = groupOf(root.project, first);
    group.families.push({ root, members, first });
    group.sessions.push(...family);
    group.tasks.push(...members.flatMap((m) => (m.kind === "task" ? [m.t] : [])));
  }
  loose.forEach((t, i) => {
    const group = groupOf(t.project, sessions.length + i);
    group.loose.push(t);
    group.tasks.push(t);
  });
  for (const group of byKey.values()) group.families.sort((a, b) => a.first - b.first);
  const rank = (g: Group): number =>
    g.sessions.length ? Math.min(...g.sessions.map((s) => STATE_RANK[s.state])) : STATE_RANK.idle + 1;
  const waitingSince = (g: Group): number =>
    Math.min(...g.sessions.filter((s) => s.state === "waiting").map((s) => s.stateSince ?? Number.POSITIVE_INFINITY));
  return [...byKey.values()].sort((a, b) => {
    const ra = rank(a);
    const diff = ra - rank(b);
    if (diff !== 0) return diff;
    if (ra === STATE_RANK.waiting) {
      const wa = waitingSince(a);
      const wb = waitingSince(b);
      if (wa !== wb) return wa < wb ? -1 : 1;
    }
    return a.first - b.first;
  });
}

interface GroupEntry {
  el: HTMLElement;
  name: HTMLElement;
  count: HTMLElement;
  list: HTMLElement;
  sig: string;
}
const groupEntries = new Map<string, GroupEntry>();
let groupSeq = 0;

function createGroup(): GroupEntry {
  const el = h("section", "group");
  const head = h("h2", "group-head");
  head.id = `group-head-${++groupSeq}`;
  el.setAttribute("aria-labelledby", head.id);
  const name = h("span", "group-name");
  const count = h("span", "group-count");
  head.append(name, count);
  const list = h("div", "group-list");
  list.setAttribute("role", "list");
  el.append(head, list);
  return { el, name, count, list, sig: "" };
}

/** What needs the user in a group, in a few words ("1 waiting · 1 error · 2 proposed tasks"). */
function groupNeeds(g: Group): SummaryPart[] {
  const msg = t();
  const waiting = g.sessions.filter((s) => s.state === "waiting").length;
  const error = g.sessions.filter((s) => s.state === "error").length;
  const tasks = g.tasks.filter((t) => !t.passedOver).length;
  const parts: SummaryPart[] = [];
  if (waiting) parts.push({ text: msg.waitingCount(waiting), tone: "waiting" });
  if (error) parts.push({ text: msg.errorCount(error), tone: "error" });
  if (tasks) parts.push({ text: msg.proposedTasks(tasks), tone: "task" });
  return parts;
}

function fillGroup(entry: GroupEntry, g: Group): void {
  const needs = groupNeeds(g);
  const sig = JSON.stringify([g.label, needs]);
  if (sig === entry.sig) return;
  entry.sig = sig;
  setText(entry.name, g.label);
  entry.name.title = g.label;
  entry.count.hidden = needs.length === 0;
  const nodes: Array<Node | string> = needs.length ? [h("span", "sr-only", ", ")] : [];
  needs.forEach((part, i) => {
    if (i > 0) nodes.push(h("span", "group-count-sep", " · "));
    nodes.push(h("span", `tone--${part.tone}`, part.text));
  });
  entry.count.replaceChildren(...nodes);
}

/** Every card in display order, across groups. */
const cardsInOrder = (): HTMLElement[] => Array.from(ui.cards.querySelectorAll<HTMLElement>(".card"));

/** Puts `el` right after `prev` (or first) in `parent`, moving it only if needed. */
function place(parent: Element, el: Element, prev: Element | null): void {
  const expected: Element | null = prev ? prev.nextElementSibling : parent.firstElementChild;
  if (expected !== el) parent.insertBefore(el, expected);
}

function renderList(): void {
  const sessions = snapshot?.sessions ?? [];
  const tasks = listedTasks();
  // Removing or moving a node blurs it: remember the focus by session key.
  const active = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const activeCard = active && ui.cards.contains(active) ? active.closest<HTMLElement>(".card") : null;
  const focusKey = activeCard?.dataset.key ?? null;
  const focusPart = active?.dataset.focus ?? null;
  const focusIndex = activeCard ? cardsInOrder().indexOf(activeCard) : -1;
  const keepCards = new Set<string>();
  const keepGroups = new Set<string>();
  let prevGroup: HTMLElement | null = null;

  for (const g of groupSessions(sessions, tasks)) {
    keepGroups.add(g.key);
    let group = groupEntries.get(g.key);
    if (!group) {
      group = createGroup();
      groupEntries.set(g.key, group);
    }
    fillGroup(group, g);
    place(ui.cards, group.el, prevGroup);
    prevGroup = group.el;

    let prev: HTMLElement | null = null;
    const put = (key: string, entry: CardEntry, level: 1 | 2): void => {
      keepCards.add(key);
      setAttr(entry.el, "aria-level", String(level));
      place(group.list, entry.el, prev);
      prev = entry.el;
    };
    const putSession = (s: SessionView, nest: Nest): void => {
      let entry = cardEntries.get(s.key);
      if (entry && (entry.hit !== null) !== s.isDesktop) {
        entry.el.remove();
        entry = undefined;
      }
      if (!entry) {
        entry = createCard(s);
        cardEntries.set(s.key, entry);
      }
      fillCard(entry, s, nest);
      put(s.key, entry, nest.nested ? 2 : 1);
    };
    const putTask = (t: ProposedTask, nest: Nest): void => {
      const key = taskKey(t);
      let entry = cardEntries.get(key);
      if (!entry) {
        entry = createTaskCard(t);
        cardEntries.set(key, entry);
      }
      fillTaskCard(entry, t, nest);
      put(key, entry, nest.nested ? 2 : 1);
    };
    const putFold = (key: string, onToggle: () => void, nest: Nest, hidden: readonly Member[], open: boolean): void => {
      let entry = cardEntries.get(key);
      if (!entry) {
        entry = createFoldCard(key, onToggle);
        cardEntries.set(key, entry);
      }
      fillFoldCard(entry, hidden, nest, open);
      put(key, entry, nest.nested ? 2 : 1);
    };
    const putTaskFold = (scope: string, fold: TaskFold, nest: Nest): void =>
      putFold(
        taskFoldKey(scope),
        () => toggleTasks(scope),
        nest,
        fold.rest.map((t) => ({ kind: "task", t })),
        fold.open,
      );

    for (const family of g.families) {
      const { root, members } = family;
      const open = !collapsedFamilies.includes(root.key);
      const tasks = taskFold(root.key, members.flatMap((m) => (m.kind === "task" ? [m.t] : [])));
      // Folded, the sessions that need the person stay in view.
      const shown = members.filter((m) =>
        open ? m.kind === "session" || tasks.shown.includes(m.t) : m.kind === "session" && needsYou(m.s),
      );
      const hidden = open ? [] : members.filter((m) => !shown.includes(m));
      const moreTasks = open && tasks.rest.length > 0;
      putSession(root, { ...TOP, fold: members.length ? { key: root.key, open } : null });
      shown.forEach((m, i) => {
        const last = i === shown.length - 1 && hidden.length === 0 && !moreTasks;
        const nest: Nest = { nested: true, last, fold: null };
        if (m.kind === "session") putSession(m.s, nest);
        else putTask(m.t, nest);
      });
      if (hidden.length) {
        putFold(foldKey(root.key), () => toggleFamily(root.key), NESTED_LAST, hidden, false);
      }
      if (moreTasks) putTaskFold(root.key, tasks, NESTED_LAST);
    }
    const loose = taskFold(`group:${g.key}`, g.loose);
    for (const t of loose.shown) putTask(t, TOP);
    if (loose.rest.length) putTaskFold(`group:${g.key}`, loose, TOP);
  }

  for (const [key, entry] of cardEntries) {
    if (keepCards.has(key)) continue;
    entry.el.remove();
    cardEntries.delete(key);
  }
  for (const [key, group] of groupEntries) {
    if (keepGroups.has(key)) continue;
    group.el.remove();
    groupEntries.delete(key);
  }

  if (focusKey && !ui.cards.contains(document.activeElement)) restoreFocus(focusKey, focusPart, focusIndex);

  ui.empty.hidden = sessions.length > 0 || tasks.length > 0;
  if (!ui.empty.hidden) {
    const ready = isReady();
    setText(ui.emptyTitle, ready ? t().noSessions : pendingText());
    setText(ui.emptyHint, ready ? t().emptyHint : "");
  }
}

/**
 * Puts the focus back on the same card (on its PR chip or fold button when
 * `part` names it), or on the card now at its place if it left.
 */
function restoreFocus(key: string, part: string | null, index: number): void {
  const entry = cardEntries.get(key);
  let target: HTMLElement | null = null;
  if (entry) {
    const inner = part ? entry.el.querySelector<HTMLElement>(`[data-focus="${part}"]`) : null;
    target = inner ?? entry.hit;
  } else {
    const cards = cardsInOrder();
    const at = Math.max(index, 0);
    const order = [...cards.slice(at), ...cards.slice(0, at).reverse()];
    target = order.map(hitOf).find((hit) => hit !== null) ?? null;
  }
  (target ?? ui.scroller).focus({ preventScroll: true });
}

function hitOf(card: Element): HTMLElement | null {
  const first = card.firstElementChild;
  return first instanceof HTMLElement && first.classList.contains("card-hit") ? first : null;
}

function createCard(s: SessionView): CardEntry {
  const el = h("div", "card");
  el.setAttribute("role", "listitem");
  el.dataset.key = s.key;
  if (!s.isDesktop) {
    // Not actionable: Desktop cannot show a session that runs in a terminal.
    return { el, hit: null, desc: null, sig: "", parts: new Map() };
  }
  const hit = h("button", "card-hit");
  hit.type = "button";
  const desc = h("span");
  desc.id = `card-desc-${++descSeq}`;
  desc.hidden = true;
  hit.setAttribute("aria-describedby", desc.id);
  el.append(hit, desc);
  const key = s.key;
  el.addEventListener("click", () => openSession(key));
  return { el, hit, desc, sig: "", parts: new Map() };
}

/** Where a card sits in its family. */
interface Nest {
  /** Under the family's root, indented and joined to it by a line. */
  nested: boolean;
  /** The family's last card: the line turns into it and stops. */
  last: boolean;
  /** A root with something under it: its fold button. */
  fold: Fold | null;
}

interface Fold {
  key: string;
  open: boolean;
}

const TOP: Nest = { nested: false, last: false, fold: null };
const NESTED_LAST: Nest = { nested: true, last: true, fold: null };

const nestClass = (n: Nest): string =>
  n.nested ? ` card--nested${n.last ? " card--nested-last" : ""}` : "";

/** Waiting or in error: shown even when its family is folded. */
const needsYou = (s: SessionView): boolean => s.state === "waiting" || s.state === "error";

function fillCard(entry: CardEntry, s: SessionView, nest: Nest): void {
  const actionable = entry.hit !== null;
  fillEntry(entry, {
    className: `card card--${s.state} ${actionable ? "card--action" : "card--static"}${nestClass(nest)}`,
    name: actionable ? cardName(s) : "",
    description: actionable ? cardDescription(s) : "",
    tooltip: cardTooltip(s),
    sections: cardSections(s, nest.fold),
  });
  // Not in the head's signature: a step must not rebuild the row and its PR chip.
  const sprite = entry.parts.get("head")?.el.querySelector<HTMLElement>(".sprite");
  if (sprite) paintSession(sprite, s);
}

function paintSession(el: HTMLElement, s: SessionView): void {
  const walk = walks.get(s.key);
  paintSprite(el, {
    state: s.state,
    prop: walk?.prop ?? null,
    steps: walk?.steps ?? 0,
    tentative: s.state === "waiting" && isTentative(s),
  });
}

// ---------------------------------------------------------------------------
// Folded families: the roots whose nested cards are hidden, kept in this
// window's storage (a convenience: lost storage only unfolds them)
// ---------------------------------------------------------------------------

const FOLDED_STORE = "monitor.foldedFamilies";
/** Oldest folds are forgotten past this many. */
const FOLDED_MAX = 50;

const collapsedFamilies: string[] = loadKeys(FOLDED_STORE, FOLDED_MAX);

function loadKeys(store: string, max: number): string[] {
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(store) ?? "[]");
    return Array.isArray(raw) ? raw.filter((k): k is string => typeof k === "string").slice(-max) : [];
  } catch {
    return [];
  }
}

function saveKeys(store: string, keys: readonly string[]): void {
  try {
    localStorage.setItem(store, JSON.stringify(keys));
  } catch {
    // Not kept: it lasts until the window reloads.
  }
}

function toggleFamily(key: string): void {
  const at = collapsedFamilies.indexOf(key);
  if (at >= 0) collapsedFamilies.splice(at, 1);
  else collapsedFamilies.push(key);
  collapsedFamilies.splice(0, Math.max(collapsedFamilies.length - FOLDED_MAX, 0));
  saveKeys(FOLDED_STORE, collapsedFamilies);
  scheduleRender();
}

/** Card key of a folded family's summary: never a session or task key. */
const foldKey = (rootKey: string): string => `fold:${rootKey}`;

/** The row standing for the cards folded away: a click shows or hides them. */
function createFoldCard(key: string, onToggle: () => void): CardEntry {
  const el = h("div", "card");
  el.setAttribute("role", "listitem");
  el.dataset.key = key;
  const hit = h("button", "card-hit");
  hit.type = "button";
  const desc = h("span");
  desc.id = `card-desc-${++descSeq}`;
  desc.hidden = true;
  hit.setAttribute("aria-describedby", desc.id);
  el.append(hit, desc);
  el.addEventListener("click", onToggle);
  return { el, hit, desc, sig: "", parts: new Map() };
}

/** "2 nested sessions · 1 working · 1 proposed task". */
function foldSummary(hidden: readonly Member[]): string {
  const msg = t();
  const sessions = hidden.flatMap((m) => (m.kind === "session" ? [m.s] : []));
  const tasks = hidden.length - sessions.length;
  const unseen = sessions.filter((s) => s.state === "unseen").length;
  const working = sessions.filter((s) => s.state === "working").length;
  const parts: string[] = [];
  if (sessions.length) parts.push(msg.nestedSessions(sessions.length));
  if (unseen) parts.push(msg.unseenCount(unseen));
  if (working) parts.push(msg.nestedWorking(working));
  if (tasks) parts.push(msg.proposedTasks(tasks));
  return parts.join(" · ");
}

function fillFoldCard(entry: CardEntry, folded: readonly Member[], nest: Nest, open: boolean): void {
  const msg = t();
  const summary = foldSummary(folded);
  const titles = folded.map((m) => (m.kind === "session" ? m.s.title : msg.proposedTask(m.t.title)));
  const nested = folded.some((m) => m.kind === "session");
  setAttr(entry.hit as HTMLElement, "aria-expanded", String(open));
  fillEntry(entry, {
    className: `card card--fold card--action${nestClass(nest)}`,
    name: msg.foldAction(open, summary.split(" · ").join(", ")),
    description: titles.join(". "),
    tooltip: [...titles, msg.foldHint(open, nested)].join("\n"),
    sections: [section("head", { summary: open ? msg.foldAction(open, summary) : summary, open }, buildFoldHead)],
  });
}

function buildFoldHead(m: { summary: string; open: boolean }): HTMLElement {
  const head = h("div", "card-head");
  const mark = h("span", m.open ? "fold-mark is-open" : "fold-mark");
  mark.append(icon("chevron"));
  head.append(mark, h("span", "card-title", m.summary));
  return head;
}

/** What a card shows and says: its look, its button's name and description, its tooltip and rows. */
interface CardContent {
  className: string;
  name: string;
  description: string;
  tooltip: string;
  sections: Section[];
}

/** Updates a card in place: only the rows whose displayed content changed are rebuilt. */
function fillEntry(entry: CardEntry, c: CardContent): void {
  const { sections } = c;
  const actionable = entry.hit !== null;
  const sig = JSON.stringify([c.className, c.name, c.description, c.tooltip, sections.map((p) => [p.id, p.sig])]);
  if (sig === entry.sig) return;
  entry.sig = sig;

  entry.el.className = c.className;
  setAttr(entry.el, "title", c.tooltip || null);
  if (entry.hit && entry.desc) {
    setAttr(entry.hit, "aria-label", c.name);
    setText(entry.desc, c.description);
  }

  const next = new Map<string, CardPart>();
  let anchor: Element | null = entry.desc;
  for (const part of sections) {
    const old = entry.parts.get(part.id);
    let el = old?.el;
    if (!old || old.sig !== part.sig) {
      el = part.build();
      // The button already says it all: keep the rows out of the reading order.
      if (actionable) hideFromReader(el);
      old?.el.remove();
    }
    if (!el) continue;
    next.set(part.id, { sig: part.sig, el });
    const expected: Element | null = anchor ? anchor.nextElementSibling : entry.el.firstElementChild;
    if (expected !== el) entry.el.insertBefore(el, expected);
    anchor = el;
  }
  for (const [id, old] of entry.parts) {
    if (!next.has(id)) old.el.remove();
  }
  entry.parts = next;
}

// ---------------------------------------------------------------------------
// Proposed task: a "New task" card of a session, started or dismissed in Desktop
// ---------------------------------------------------------------------------

/** Card key of a task: never a session key ("local_…", "cli:…"). */
const taskKey = (t: ProposedTask): string => `task:${t.id}`;

/** Cards set aside here, kept in this window's storage: Desktop still has them,
 * and lost storage only shows them again. */
const HIDDEN_TASKS_STORE = "monitor.hiddenTasks";
/** Oldest ones are forgotten past this many. */
const HIDDEN_TASKS_MAX = 200;

const hiddenTasks: string[] = loadKeys(HIDDEN_TASKS_STORE, HIDDEN_TASKS_MAX);

function hideTask(id: string): void {
  if (!hiddenTasks.includes(id)) hiddenTasks.push(id);
  hiddenTasks.splice(0, Math.max(hiddenTasks.length - HIDDEN_TASKS_MAX, 0));
  saveKeys(HIDDEN_TASKS_STORE, hiddenTasks);
  showNotice(() => t().taskHidden, "info");
}

/** The "New task" cards to show. */
const listedTasks = (): ProposedTask[] =>
  snapshot && isReady() ? snapshot.proposedTasks.filter((t) => !hiddenTasks.includes(t.id)) : [];

/** Family roots and projects whose folded tasks are shown, until the window reloads. */
const expandedTasks = new Set<string>();

function toggleTasks(scope: string): void {
  if (!expandedTasks.delete(scope)) expandedTasks.add(scope);
  scheduleRender();
}

/** Card key of the row folding a family's or project's tasks: never a session or task key. */
const taskFoldKey = (scope: string): string => `tasks:${scope}`;

interface TaskFold {
  shown: ProposedTask[];
  /** All but the lead task: what the row after the cards folds. */
  rest: ProposedTask[];
  open: boolean;
}

/**
 * One task at a time asks for attention, in a family or a project: the newest
 * the person has not passed over. The others wait behind a row.
 */
function taskFold(scope: string, tasks: readonly ProposedTask[]): TaskFold {
  const lead = tasks.reduce<ProposedTask | null>(
    (best, t) => (!t.passedOver && (best === null || t.createdAt > best.createdAt) ? t : best),
    null,
  );
  const open = expandedTasks.has(scope);
  return {
    shown: tasks.filter((t) => open || t === lead),
    rest: tasks.filter((t) => t !== lead),
    open,
  };
}

/** A click opens the session that proposed the task, where its card is. */
function createTaskCard(t: ProposedTask): CardEntry {
  const el = h("div", "card");
  el.setAttribute("role", "listitem");
  el.dataset.key = taskKey(t);
  const hit = h("button", "card-hit");
  hit.type = "button";
  const desc = h("span");
  desc.id = `card-desc-${++descSeq}`;
  desc.hidden = true;
  hit.setAttribute("aria-describedby", desc.id);
  el.append(hit, desc);
  const sessionKey = t.sessionKey;
  el.addEventListener("click", () => openSession(sessionKey));
  return { el, hit, desc, sig: "", parts: new Map() };
}

function fillTaskCard(entry: CardEntry, task: ProposedTask, nest: Nest): void {
  const msg = t();
  const lines = [msg.proposedTask(task.title)];
  if (task.tldr) lines.push(clip(task.tldr, 300));
  lines.push(msg.proposedByAt(task.sessionTitle, formatClock(task.createdAt)));
  if (task.passedOver) lines.push(msg.taskPassedOver);
  if (task.expiresAt !== null && task.expiresAt <= task.listedUntil) {
    lines.push(msg.taskExpires(formatWhen(task.expiresAt)));
  } else {
    // Desktop keeps it; the monitor only lists current work.
    lines.push(msg.taskListedUntil(formatWhen(task.listedUntil)));
  }
  lines.push(msg.taskHint);
  // Under its session, the pitch or nothing: the session above says who proposed it.
  const line = task.tldr ?? (nest.nested ? null : msg.proposedBy(task.sessionTitle));
  const head = section("head", { id: task.id, title: task.title, createdAt: task.createdAt }, buildTaskHead);
  fillEntry(entry, {
    className: `card card--task card--action${nestClass(nest)}`,
    name: msg.taskCardName(task.title, task.sessionTitle),
    description: task.tldr ?? "",
    tooltip: lines.join("\n"),
    sections: line === null ? [head] : [head, section("line", { line }, (m) => h("div", "card-line", m.line))],
  });
}

function hideTaskButton(id: string, title: string): HTMLElement {
  const button = h("button", "card-dismiss");
  button.type = "button";
  button.dataset.focus = "hide";
  button.title = t().hideTaskHint;
  button.setAttribute("aria-label", t().hideTaskLabel(title));
  button.append(icon("cross"));
  button.addEventListener("click", (e) => {
    // The card behind opens the session.
    e.stopPropagation();
    hideTask(id);
  });
  return button;
}

function buildTaskHead(m: { id: string; title: string; createdAt: number }): HTMLElement {
  const head = h("div", "card-head");
  const mark = h("span", "task-mark");
  mark.append(icon("plus"));
  const status = h("span", "card-status");
  status.append(
    h("span", "status-label", t().stateProposed),
    " · ",
    sinceSpan(m.createdAt, "status-since", t().proposedAt(formatClock(m.createdAt))),
  );
  head.append(mark, h("span", "card-title", m.title), status, hideTaskButton(m.id, m.title));
  return head;
}

/** Hides a row from screen readers, except the buttons in it (the PR chip). */
function hideFromReader(el: HTMLElement): void {
  if (!el.querySelector("button")) {
    el.setAttribute("aria-hidden", "true");
    return;
  }
  for (const child of Array.from(el.children)) {
    if (child.tagName !== "BUTTON") child.setAttribute("aria-hidden", "true");
  }
}

// ---------------------------------------------------------------------------
// Card content: each row is built from a small model of what it shows, and
// the model's JSON is the row's signature (no hidden field can trigger a rebuild)
// ---------------------------------------------------------------------------

interface Section {
  id: string;
  sig: string;
  build: () => HTMLElement;
}

function section<M>(id: string, model: M, build: (m: M) => HTMLElement): Section {
  // The language counts: most models leave the wording to `build`.
  return { id, sig: JSON.stringify([getLang(), model]), build: () => build(model) };
}

/** Line 1 (state, title, time) and, when it answers "does it need me / where is it", line 2. */
function cardSections(s: SessionView, fold: Fold | null): Section[] {
  const out: Section[] = [section("head", headModel(s, fold), buildHead)];
  const line = lineModel(s);
  if (line) out.push(section("line", line, buildLine));
  return out;
}

function activityText(a: ToolActivity): string {
  const summary = str(a.summary).trim();
  return summary ? `${a.name} · ${summary}` : a.name;
}

const branchOf = (s: SessionView): string => str(s.git?.branch).trim() || str(s.worktree).trim();
const dirtyOf = (s: SessionView): number => Math.max(num(s.git?.dirtyFiles) ?? 0, 0);
const parentTitle = (s: SessionView): string | null =>
  s.parent ? str(s.parent.title).trim() || t().anotherSession : null;
const peerName = (link: PeerLink): string => str(link.peerTitle).trim() || t().unknownSession;
const clip = (text: string, max: number): string => (text.length > max ? `${text.slice(0, max - 1).trimEnd()}…` : text);

/** What a session waits for once its turn is over: "Waiting for a command: cargo test". */
function backgroundText(b: BackgroundWait): string {
  const msg = t();
  const named = (label: string): string => (b.summary ? msg.labeled(label, b.summary) : label);
  if (b.kind === "sessions") return b.count > 1 ? msg.waitingForSessions(b.count) : named(msg.waitingForSession);
  if (b.count > 1) return named(msg.waitingForTasks(b.count));
  if (b.kind === "wakeup") return named(b.endsAt !== null ? msg.resumesAt(momentOf(b.endsAt)) : msg.willResume);
  return named(labelOf(msg.background, b.kind) ?? msg.waitingForTask);
}

/** Accessible name of a card's button: title, state and what it is about. */
function cardName(s: SessionView): string {
  const parts = [s.title, t().states[s.state]];
  switch (s.state) {
    case "waiting":
      parts.push(reasonText(s));
      if (s.question) parts.push(s.question.text);
      else if (s.activity && REASONS_WITH_TOOL.has(reasonKey(s.waitingFor))) parts.push(activityText(s.activity));
      break;
    case "working":
      if (s.background) parts.push(backgroundText(s.background));
      else if (s.activity) parts.push(activityText(s.activity));
      break;
    case "error":
      parts.push(s.lastError ?? t().errorFallback);
      break;
    case "unseen":
    case "idle":
      break;
  }
  return parts.join(", ");
}

/** Accessible description: what the tooltip and the card tell beyond the name. */
function cardDescription(s: SessionView): string {
  const msg = t();
  const out: string[] = [];
  if ((s.state === "unseen" || s.state === "idle") && s.recap) out.push(s.recap);
  if (s.state === "waiting" && s.question?.options.length) {
    out.push(msg.suggestedAnswers(s.question.options.join(", ")));
  }
  if (s.findings) out.push(findingsText(s.findings));
  if (s.onScreen) out.push(msg.shownInDesktop);
  const where = [s.project ? msg.inProject(s.project) : "", branchOf(s) ? msg.onBranch(branchOf(s)) : ""].filter(Boolean);
  if (where.length) out.push(where.join(", "));
  const dirty = dirtyOf(s);
  if (dirty > 0) out.push(msg.dirtyFiles(dirty));
  const plan = planModel(s);
  if (plan) out.push(msg.planProgress(plan.done, plan.total, plan.current));
  const pr = prModel(s);
  if (pr) out.push(`PR #${pr.number}, ${prStatus(pr, s.state)}`);
  out.push(...extrasLines(extrasModel(s)));
  if (s.agents.length) out.push(agentsText(s.agents));
  const parent = parentTitle(s);
  if (parent) out.push(msg.startedFrom(parent));
  return out.map(sentence).join(" ");
}

/** The card's tooltip: the details that no longer fit on it, one per line. */
function cardTooltip(s: SessionView): string {
  const msg = t();
  const lines: string[] = [];
  if (!s.isDesktop) lines.push(msg.cliTooltip);
  if (s.state === "waiting") {
    const reason = reasonLabel(s.waitingFor);
    const plan = planTitle(s);
    if (s.question) {
      lines.push(msg.labeled(reason, clip(s.question.text, 300)));
      if (s.question.options.length) lines.push(msg.suggestedAnswers(s.question.options.join(" · ")));
    }
    if (plan) {
      const at = s.activity?.at ?? null;
      lines.push(`${msg.labeled(reason, clip(plan, 300))}${at !== null ? ` (${formatClock(at)})` : ""}`);
    }
    if (s.activity && REASONS_WITH_TOOL.has(reasonKey(s.waitingFor))) {
      lines.push(msg.requestLine(clip(activityText(s.activity), 160)));
    }
  }
  if (s.state === "error" && s.lastError) lines.push(msg.errorLine(clip(s.lastError, 300)));
  if (s.background) lines.push(clip(backgroundText(s.background), 300));
  if (s.findings) lines.push(findingsText(s.findings));
  const branch = str(s.git?.branch).trim();
  const worktree = str(s.worktree).trim();
  if (branch) lines.push(msg.branchLine(branch));
  if (worktree) lines.push(msg.worktreeLine(worktree));
  const dirty = dirtyOf(s);
  if (dirty > 0) lines.push(msg.dirtyFiles(dirty));
  const pr = prModel(s);
  if (pr && !isOpen(pr)) lines.push(`PR #${pr.number} ${prStatus(pr, s.state)}`);
  if (s.recap) lines.push(msg.recapLine(clip(oneLine(s.recap), 200)));
  // For a plan to approve, the activity is the plan call itself, already shown above.
  const planShown = planTitle(s) !== null;
  if (s.activity && !planShown) {
    const at = s.activity.at !== null ? ` (${formatClock(s.activity.at)})` : "";
    lines.push(msg.activityLine(`${clip(activityText(s.activity), 120)}${at}`));
  } else if (!s.activity && s.lastActivityAt !== null && num(s.lastActivityAt) !== null) {
    lines.push(msg.activityLine(formatClock(s.lastActivityAt)));
  }
  if (s.agents.length) lines.push(clip(agentsText(s.agents), 300));
  const parent = parentTitle(s);
  if (parent) lines.push(msg.startedFrom(parent));
  lines.push(...extrasLines(extrasModel(s)));
  if (s.stopped) lines.push(msg.stoppedTooltip);
  if (s.onScreen) lines.push(msg.onScreenTooltip);
  return lines.join("\n");
}

// Line 1: character, title, eye, CLI tag, open PR chip, "waiting · 3 h".

interface HeadModel {
  state: SessionState;
  title: string;
  since: number | null;
  onScreen: boolean;
  cli: boolean;
  agents: RunningAgent[];
  pr: PrModel | null;
  fold: Fold | null;
}

const headModel = (s: SessionView, fold: Fold | null): HeadModel => {
  const pr = prModel(s);
  return {
    state: s.state,
    title: s.title,
    since: s.stateSince,
    onScreen: s.onScreen,
    cli: !s.isDesktop,
    agents: s.agents,
    pr: pr && isOpen(pr) ? pr : null,
    fold,
  };
};

function buildHead(m: HeadModel): HTMLElement {
  const head = h("div", m.fold ? "card-head card-head--fold" : "card-head");
  head.append(h("span", "sprite"), h("span", "card-title", m.title));
  if (m.onScreen) {
    const eye = h("span", "card-eye");
    eye.title = t().onScreenTooltip;
    eye.append(icon("eye"));
    head.append(eye);
  }
  if (m.cli) {
    const tag = h("span", "card-tag", "CLI");
    tag.title = t().cliTooltip;
    head.append(tag);
  }
  if (m.agents.length) head.append(crew(m.agents));
  if (m.fold) head.append(foldButton(m.fold));
  if (m.pr) head.append(prChip(m.pr, m.state));
  const label = t().statesShort[m.state];
  if (label || m.since !== null) {
    const status = h("span", `card-status status--${m.state}`);
    if (label) status.append(h("span", "status-label", label));
    if (label && m.since !== null) status.append(" · ");
    if (m.since !== null) {
      status.append(sinceSpan(m.since, "status-since", t().stateSince(t().states[m.state], formatClock(m.since))));
    }
    head.append(status);
  }
  return head;
}

/** Mini crabs shown for a session's sub-agents; the others are counted after them. */
const CREW_SHOWN = 3;

/** "2 sub-agents: Code audit, Tests". */
function agentsText(agents: readonly RunningAgent[]): string {
  const names = agents.map((a) => a.description || a.type || t().unnamedTask);
  return t().subAgents(agents.length, names.join(", "));
}

/** One small crab per running sub-agent. */
function crew(agents: readonly RunningAgent[]): HTMLElement {
  const el = h("span", "crew");
  el.title = agentsText(agents);
  el.append(...agents.slice(0, CREW_SHOWN).map(miniCrab));
  if (agents.length > CREW_SHOWN) el.append(h("span", "", `+${agents.length - CREW_SHOWN}`));
  return el;
}

/** After the title, as in Desktop's sidebar: folds or unfolds what hangs from the session. */
function foldButton(f: Fold): HTMLElement {
  const button = h("button", "card-fold");
  button.type = "button";
  button.dataset.focus = "fold";
  const label = f.open ? t().collapseNested : t().expandNested;
  button.title = label;
  button.setAttribute("aria-label", label);
  button.setAttribute("aria-expanded", String(f.open));
  button.append(icon("chevron"));
  const key = f.key;
  button.addEventListener("click", (e) => {
    // The card behind opens the session.
    e.stopPropagation();
    toggleFamily(key);
  });
  return button;
}

// Line 2: the question or reason, the error, what it waits for, the plan, or a finished review's findings.

type LineModel =
  | { kind: "question" | "reason"; text: string; tentative: boolean }
  | { kind: "error" | "findings" | "background"; text: string }
  | ({ kind: "plan" } & PlanModel);

function lineModel(s: SessionView): LineModel | null {
  switch (s.state) {
    case "waiting":
      return s.question
        ? { kind: "question", text: s.question.text, tentative: isTentative(s) }
        : { kind: "reason", text: reasonText(s), tentative: isTentative(s) };
    case "error":
      return { kind: "error", text: oneLine(s.lastError ?? t().errorFallback) };
    case "working": {
      if (s.background) return { kind: "background", text: backgroundText(s.background) };
      const plan = planModel(s);
      return plan ? { kind: "plan", ...plan } : null;
    }
    case "unseen":
      // A review just finished: how much it found is the news.
      return s.findings ? { kind: "findings", text: findingsText(s.findings) } : null;
    case "idle":
      return null;
  }
}

function buildLine(m: LineModel): HTMLElement {
  switch (m.kind) {
    case "question":
      return h("div", `card-line line--question clamp-2${m.tentative ? " is-tentative" : ""}`, m.text);
    case "reason":
      return h("div", `card-line line--reason${m.tentative ? " is-tentative" : ""}`, m.text);
    case "error":
      return h("div", "card-line line--error", m.text);
    case "findings":
      return h("div", "card-line line--findings", m.text);
    case "background":
      return h("div", "card-line line--background", m.text);
    case "plan":
      return buildPlan(m);
  }
}

// Plan: progress bar, "3/7 · current step", every step in the tooltip.

interface PlanModel {
  done: number;
  total: number;
  current: string;
  items: Array<[status: string, content: string]>;
}

function planModel(s: SessionView): PlanModel | null {
  const plan = s.plan;
  if (!plan || plan.total <= 0) return null;
  const done = Math.min(Math.max(plan.done, 0), plan.total);
  // A finished plan on a session already seen tells nothing more. Statuses are
  // shown as Desktop has them: a wrap_up does not tick anything off.
  if (s.state === "idle" && done >= plan.total) return null;
  return {
    done,
    total: plan.total,
    current: str(plan.current).trim(),
    items: plan.items.map((i) => [str(i.status), str(i.content)]),
  };
}

const PLAN_MARKS: Record<string, string> = { completed: "✓", in_progress: "▸" };

function buildPlan(m: PlanModel): HTMLElement {
  const row = h("div", `card-line plan${m.done >= m.total ? " plan--done" : ""}`);
  const track = h("span", "plan-track");
  const fill = h("span", "plan-fill");
  fill.style.width = `${(m.done / m.total) * 100}%`;
  track.append(fill);

  const text = h("span", "plan-text");
  text.append(h("span", "plan-count", `${m.done}/${m.total}`));
  if (m.current) text.append(` · ${m.current}`);
  row.append(track, text);
  row.title = m.items.map(([status, content]) => `${PLAN_MARKS[status] ?? "○"} ${content}`).join("\n");
  return row;
}

// PR chip (open PRs only, on line 1).

type PrModel = Omit<PrInfo, "checkedAt" | "source">;

function prModel(s: SessionView): PrModel | null {
  if (!s.pr) return null;
  return {
    number: s.pr.number,
    url: str(s.pr.url),
    title: s.pr.title,
    repository: s.pr.repository,
    state: s.pr.state,
    isDraft: s.pr.isDraft === true,
    reviewDecision: s.pr.reviewDecision,
    checks: s.pr.checks,
    mergeState: s.pr.mergeState,
    autoFix: s.pr.autoFix === true,
  };
}

const isOpen = (pr: PrModel): boolean => str(pr.state).toUpperCase() === "OPEN";

/**
 * Auto-fix watches this open PR: "handling" while the session works on its failing CI or
 * conflict, else "on". Desktop wakes the session once per failure: once that turn ends,
 * waits on the person or fails, nothing is being handled any more.
 */
function autoFixState(pr: PrModel, state: SessionState): "on" | "handling" | null {
  if (!pr.autoFix || !isOpen(pr)) return null;
  const trouble = pr.checks === "fail" || str(pr.mergeState).toUpperCase() === "DIRTY";
  return trouble && state === "working" ? "handling" : "on";
}

type PrVariant = "pass" | "fail" | "pending" | "none";

const PR_ICON: Record<PrVariant, IconName> = {
  pass: "check",
  fail: "cross",
  pending: "clock",
  none: "prOpen",
};

function prVariant(pr: PrModel): PrVariant {
  return pr.checks === "pass" || pr.checks === "fail" || pr.checks === "pending" ? pr.checks : "none";
}

function prStatus(pr: PrModel, session: SessionState): string {
  const msg = t();
  const state = str(pr.state).toUpperCase();
  if (state === "MERGED") return msg.prMerged;
  if (state === "CLOSED") return msg.prClosed;
  const checks = labelOf(msg.checksPhrases, pr.checks) ?? msg.checksPhraseUnknown;
  const autoFix = autoFixState(pr, session);
  const status = pr.isDraft ? msg.prDraftStatus(checks) : msg.prOpenStatus(checks);
  if (!autoFix) return status;
  return `${status}, ${autoFix === "handling" ? msg.autoFixHandling : msg.autoFixOn}`;
}

function prTooltip(pr: PrModel, session: SessionState): string {
  const msg = t();
  const title = str(pr.title).trim();
  const lines = [`PR #${pr.number}${title ? ` · ${title}` : ""}`];
  lines.push(pr.isDraft ? msg.prOpenDraft : msg.prOpen);
  lines.push(msg.prChecks(labelOf(msg.checks, pr.checks) ?? msg.checksUnknown));
  const review = str(pr.reviewDecision).trim();
  if (review) lines.push(msg.prReview(labelOf(msg.reviews, review) ?? review));
  const merge = str(pr.mergeState).trim();
  if (merge) lines.push(msg.prMerge(labelOf(msg.merges, merge) ?? merge.toLowerCase()));
  const repo = str(pr.repository).trim();
  if (repo) lines.push(repo);
  const autoFix = autoFixState(pr, session);
  if (autoFix) lines.push(msg.autoFixTooltip);
  if (autoFix === "handling") lines.push(msg.autoFixWorking);
  return lines.join("\n");
}

function prChip(pr: PrModel, session: SessionState): HTMLElement {
  const variant = prVariant(pr);
  const autoFix = autoFixState(pr, session) !== null;
  const btn = h("button", `chip chip--pr chip--${variant}${pr.isDraft ? " chip--draft" : ""}${autoFix ? " chip--autofix" : ""}`);
  btn.type = "button";
  btn.dataset.focus = "pr";
  btn.title = prTooltip(pr, session);
  btn.setAttribute("aria-label", t().prOpenLabel(pr.number, prStatus(pr, session)));
  btn.append(icon(PR_ICON[variant]), h("span", "", `#${pr.number}`));
  if (autoFix) {
    const mark = icon("autoFix");
    mark.classList.add("chip-autofix");
    btn.append(mark);
  }
  const url = pr.url;
  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    if (url) openUrl(url);
  });
  return btn;
}

// Messages and warnings: tooltip and description only.

interface ExtrasModel {
  awaiting: string[];
  unread: Array<{ name: string; count: number }>;
  undelivered: string[];
  exceeded: number;
}

function extrasModel(s: SessionView): ExtrasModel {
  const unread = new Map<string, { name: string; count: number }>();
  for (const link of s.messages.unreadInbound) {
    const entry = unread.get(link.peerHostSessionId) ?? { name: peerName(link), count: 0 };
    entry.count += 1;
    unread.set(link.peerHostSessionId, entry);
  }
  return {
    awaiting: s.messages.awaitingReply.map(peerName),
    unread: [...unread.values()],
    undelivered: s.messages.undelivered.map(peerName),
    exceeded: Math.max(num(s.contextExceededCount) ?? 0, 0),
  };
}

function extrasLines(m: ExtrasModel): string[] {
  const msg = t();
  const out: string[] = [];
  for (const peer of m.awaiting) out.push(msg.awaitingReply(peer));
  for (const u of m.unread) out.push(msg.unreadMessages(u.count, u.name));
  for (const peer of m.undelivered) out.push(msg.undelivered(peer));
  if (m.exceeded > 0) out.push(msg.contextExceeded(m.exceeded));
  return out;
}

// ---------------------------------------------------------------------------
// Health line
// ---------------------------------------------------------------------------

interface HealthRow {
  text: string;
  tone: "warn" | "error";
}

/** Source and connection problems, most serious first (failed commands apart). */
function healthRows(): HealthRow[] {
  const rows: HealthRow[] = [];
  if (fatalError) rows.push({ text: fatalError(), tone: "error" });
  if (liveWarning) rows.push({ text: liveWarning(), tone: "warn" });
  if (snapshot) {
    const health = snapshot.health;
    // Before the engine's first pass the source flags are defaults, not failures.
    if (snapshot.ready) {
      if (!health.registryOk) rows.push({ text: t().registryMissing, tone: "warn" });
      if (!health.desktopOk) rows.push({ text: t().desktopUnreadable, tone: "warn" });
      if (health.ghOk === false) rows.push({ text: t().ghDown, tone: "warn" });
    }
    if (health.errors.length) rows.push({ text: health.errors[0], tone: "warn" });
  }
  return rows;
}

function renderHealth(base: readonly HealthRow[]): void {
  const tone = noticeTone === "info" ? ("warn" as const) : ("error" as const);
  const rows = notice ? [...base, { text: notice(), tone }] : base;
  const sig = JSON.stringify(rows);
  if (ui.health.dataset.sig === sig) return;
  ui.health.dataset.sig = sig;
  ui.health.hidden = rows.length === 0;
  ui.health.replaceChildren(
    ...rows.map((row) => {
      const el = h("div", `health-row health-row--${row.tone}`);
      el.title = row.text;
      el.append(icon("warn"), h("span", "health-text", row.text));
      return el;
    }),
  );
}

/** Refreshes every relative time in place, without touching the rest. */
function tickTimes(): void {
  const now = Date.now();
  document.querySelectorAll<HTMLElement>("[data-since]").forEach((el) => {
    const since = Number(el.dataset.since);
    if (Number.isFinite(since)) setText(el, formatAgo(now - since));
  });
  // The time marks move and the windows expire with the clock.
  renderQuota();
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/**
 * Applies a snapshot unless an older one. Order is by `seq`, never by the
 * wall clock: events arrive in order, but a get_snapshot reply can be
 * overtaken by an event that is then newer or equal.
 */
function acceptSnapshot(raw: unknown, origin: "event" | "fetch"): void {
  let next = sanitizeSnapshot(raw);
  if (!next) return;
  if (snapshot) {
    if (next.seq < snapshot.seq) return;
    if (origin === "fetch" && gotEvent && next.seq === snapshot.seq) return;
  }
  if (next.lang !== getLang()) {
    setLang(next.lang);
    // Sanitizing wrote its fallback titles in the language it replaces.
    next = sanitizeSnapshot(raw) ?? next;
    renderStatic();
  }
  if (origin === "event") gotEvent = true;
  walks = nextWalks(next.sessions);
  snapshot = next;
  fatalError = null;
  scheduleRender();
}

function siblingHit(card: HTMLElement, step: 1 | -1): HTMLElement | null {
  const cards = cardsInOrder();
  for (let i = cards.indexOf(card) + step; i >= 0 && i < cards.length; i += step) {
    const hit = hitOf(cards[i]);
    if (hit) return hit;
  }
  return null;
}

function edgeHit(last: boolean): HTMLElement | null {
  const cards = cardsInOrder();
  if (last) cards.reverse();
  return cards.map(hitOf).find((hit) => hit !== null) ?? null;
}

function onCardKeydown(e: KeyboardEvent): void {
  const hit = e.target instanceof HTMLElement && e.target.classList.contains("card-hit") ? e.target : null;
  const card = hit?.parentElement;
  if (!hit || !card) return;
  let target: HTMLElement | null = null;
  switch (e.key) {
    case "Enter":
      // One open per key press, not one per auto-repeat.
      if (e.repeat) e.preventDefault();
      return;
    case "ArrowDown":
      target = siblingHit(card, 1);
      break;
    case "ArrowUp":
      target = siblingHit(card, -1);
      break;
    case "Home":
      target = edgeHit(false);
      break;
    case "End":
      target = edgeHit(true);
      break;
    default:
      return;
  }
  e.preventDefault();
  target?.focus();
}

function bindUi(): void {
  ui.btnCompact.addEventListener("click", () => void setCompact(true));
  ui.btnExpand.addEventListener("click", () => void expand());
  ui.compactWarn.addEventListener("click", () => void expand());
  ui.btnPin.addEventListener("click", () => void setAlwaysOnTop(!settings.alwaysOnTop));
  ui.btnHide.addEventListener("click", () => void call(CMD.hideWindow));
  ui.compactOpen.addEventListener("click", onCompactOpen);
  ui.cards.addEventListener("keydown", onCardKeydown);
  document.addEventListener("visibilitychange", onVisibility);
}

/** Outside Tauri: frame the UI like the real window and list the demo scenarios. */
function setupMockChrome(): void {
  root.classList.add("is-mock");
  // ?wide: the window as the user often widens it.
  root.classList.toggle("is-mock-wide", new URLSearchParams(window.location.search).has("wide"));
  mockCaption = h("p", "mock-caption");
  renderMockCaption(mockCaption);
  ui.app.after(mockCaption);
}

function renderMockCaption(caption: HTMLElement): void {
  const msg = t();
  const lang = new URLSearchParams(window.location.search).get("lang");
  const scenarios: Array<[string, string]> = [
    [msg.mockFull, "?"],
    [msg.mockWide, "?wide"],
    [msg.mockEmpty, "?scenario=empty"],
    [msg.mockHealth, "?scenario=health"],
  ];
  const nodes: Array<Node | string> = [msg.mockCaption];
  scenarios.forEach(([label, href], i) => {
    if (i > 0) nodes.push(" · ");
    const a = h("a", "", label);
    // ?lang=… chose the language: following a link keeps it.
    a.href = lang === null ? href : `${href}${href === "?" ? "" : "&"}lang=${encodeURIComponent(lang)}`;
    nodes.push(a);
  });
  caption.replaceChildren(...nodes);
}

async function loadSnapshot(): Promise<void> {
  if (!bridge) return;
  try {
    acceptSnapshot(await bridge.invoke<unknown>(CMD.getSnapshot), "fetch");
  } catch (err) {
    fatalError = () => t().stateUnreadable(errorText(err));
    scheduleRender();
  }
}

async function loadSettings(): Promise<void> {
  if (!bridge) return;
  try {
    applySettings(sanitizeSettings(await bridge.invoke<unknown>(CMD.getSettings), settings));
  } catch (err) {
    showNotice(() => t().settingsUnreadable(errorText(err)));
  }
}

async function start(): Promise<void> {
  bindUi();
  renderStatic();
  applySettings(settings);
  scheduleRender();

  try {
    bridge = await createBridge();
  } catch (err) {
    fatalError = () => t().startFailed(errorText(err));
    scheduleRender();
    return;
  }

  if (bridge.mock) setupMockChrome();
  else document.addEventListener("contextmenu", (e) => e.preventDefault());

  let live = true;
  try {
    await bridge.listen(EVENT_SNAPSHOT, (payload) => acceptSnapshot(payload, "event"));
    await bridge.listen(EVENT_SETTINGS, (payload) => applySettings(sanitizeSettings(payload, settings)));
    await bridge.listen(EVENT_VISIBILITY, (payload) => {
      if (typeof payload === "boolean") setShown(payload);
    });
  } catch (err) {
    live = false;
    liveWarning = () => t().liveUnavailable(errorText(err));
  }

  await Promise.all([loadSnapshot(), loadSettings()]);
  if (!live) window.setInterval(() => void loadSnapshot(), 2000);
  syncTicker();
}

void start();
