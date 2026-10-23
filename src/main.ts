// Window UI: renders the engine's snapshot and sends the user's actions back.
// Runs against the Tauri backend, or against src/mock.ts in a plain browser.

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen } from "@tauri-apps/api/event";
import { CMD, EVENT_SETTINGS, EVENT_SNAPSHOT, EVENT_VISIBILITY } from "./types";
import type {
  Counts,
  FindingsReport,
  PeerLink,
  PendingQuestion,
  PlanProgress,
  PrInfo,
  ProposedTask,
  Quota,
  QuotaWindow,
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
// Labels (French UI)
// ---------------------------------------------------------------------------

const STATES: readonly SessionState[] = ["waiting", "error", "unseen", "working", "idle"];

const STATE_LABEL: Record<SessionState, string> = {
  waiting: "En attente",
  error: "En erreur",
  unseen: "Terminée, non vue",
  working: "Au travail",
  idle: "Inactive",
};

const REASONS: Record<string, string> = {
  "permission prompt": "Permission demandée",
  "input needed": "Question en attente",
  "sandbox request": "Accès sandbox demandé",
  "worker request": "Requête d'un worker",
  "dialog open": "Boîte de dialogue ouverte",
  decision: "Décision attendue",
  "probable question": "Question dans le dernier message",
  offer: "Choix demandé",
  "plan approval": "Plan à approuver",
};

/** Waiting reasons guessed from the last message alone: shown less assertively. */
const TENTATIVE_REASONS = new Set(["probable question"]);

/** Waiting reasons for which the pending tool call is worth showing. */
const REASONS_WITH_TOOL = new Set(["permission prompt", "sandbox request"]);

const CHECKS_LABEL: Record<string, string> = {
  pass: "réussis",
  fail: "en échec",
  pending: "en cours",
  none: "aucun",
};

/** Checks status as a phrase, for spoken labels. */
const CHECKS_PHRASE: Record<string, string> = {
  pass: "checks réussis",
  fail: "checks en échec",
  pending: "checks en cours",
  none: "aucun check",
};

const REVIEW_LABEL: Record<string, string> = {
  APPROVED: "approuvée",
  CHANGES_REQUESTED: "modifications demandées",
  REVIEW_REQUIRED: "en attente de revue",
};

const MERGE_LABEL: Record<string, string> = {
  CLEAN: "prête à fusionner",
  BLOCKED: "bloquée",
  DIRTY: "conflits à résoudre",
  BEHIND: "en retard sur la branche de base",
  UNSTABLE: "checks facultatifs en échec",
  HAS_HOOKS: "prête (hooks)",
  DRAFT: "brouillon",
  UNKNOWN: "état inconnu",
};

const COMMAND_FAILURE: Record<string, string> = {
  [CMD.openSession]: "Impossible d'ouvrir la session",
  [CMD.openUrl]: "Impossible d'ouvrir la PR",
  [CMD.setCompact]: "Impossible de changer de mode",
  [CMD.setAlwaysOnTop]: "Impossible de changer l'épinglage",
  [CMD.hideWindow]: "Impossible de masquer la fenêtre",
};

const READING = "Lecture des sessions…";
const CLI_TOOLTIP = "Session lancée dans un terminal : elle ne peut pas être ouverte dans Claude Desktop";
const ON_SCREEN_TOOLTIP = "Affichée dans Claude Desktop, au premier plan";
const STOPPED_TOOLTIP = "En pause : Claude Desktop a arrêté son processus. Elle reprend dès que tu lui écris.";

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
};

const reasonKey = (raw: string | null): string => (raw ?? "").trim().toLowerCase();

/** French reason for a raw `waitingFor` value; unknown values are shown as is. */
function reasonFr(raw: string | null): string {
  const key = reasonKey(raw);
  if (!key) return "En attente";
  return REASONS[key] ?? (raw ?? "").trim();
}

/** The plan's title when the session waits for its approval (the pending call is the plan). */
function planTitle(s: SessionView): string | null {
  if (reasonKey(s.waitingFor) !== "plan approval") return null;
  return str(s.activity?.summary).trim() || null;
}

/** Why a session waits, in a few words: the reason, with the plan's title for a plan. */
function reasonText(s: SessionView): string {
  const plan = planTitle(s);
  const reason = reasonFr(s.waitingFor);
  return plan ? `${reason} : ${plan}` : reason;
}

/** "Revue : 13 résultats dont 5 confirmés", or what is left to handle. */
function findingsText(f: FindingsReport): string {
  const results = (n: number): string => `${n} ${plural(n, "résultat", "résultats")}`;
  if (f.open < f.count) return `Revue : ${results(f.open)} sur ${f.count} encore à traiter`;
  if (f.confirmed >= f.count) {
    return f.count > 1 ? `Revue : ${results(f.count)}, tous confirmés` : "Revue : 1 résultat confirmé";
  }
  const confirmed = f.confirmed > 0 ? ` dont ${f.confirmed} ${plural(f.confirmed, "confirmé", "confirmés")}` : "";
  return `Revue : ${results(f.count)}${confirmed}`;
}

/** True when the question is only guessed from the wording of the last message. */
function isTentative(s: SessionView): boolean {
  return TENTATIVE_REASONS.has(reasonKey(s.waitingFor)) || s.question?.source === "text";
}

/** French plural: 0 and 1 take the singular. */
const plural = (n: number, one: string, many: string): string => (n > 1 ? many : one);

/** "12 s", "3 min", "2 h", "4 j" (non-breaking space). */
function formatAgo(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s} s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h} h`;
  return `${Math.floor(h / 24)} j`;
}

function formatClock(ts: number): string {
  const date = new Date(ts);
  const time = date.toLocaleTimeString("fr-FR", { hour: "2-digit", minute: "2-digit" });
  if (date.toDateString() === new Date().toDateString()) return time;
  return `${date.toLocaleDateString("fr-FR", { day: "numeric", month: "short" })} à ${time}`;
}

/** A moment after a verb: "à 14:05" today, else "le 29 oct. à 14:05". */
function formatWhen(ts: number): string {
  const clock = formatClock(ts);
  return clock.includes(" à ") ? `le ${clock}` : `à ${clock}`;
}

/** One-line message of a rejected command (Tauri rejects with strings). */
function errorText(err: unknown): string {
  let text = "";
  if (err instanceof Error) text = err.message;
  else if (typeof err === "string") text = err;
  else if (typeof err === "object" && err !== null && "message" in err && typeof err.message === "string") {
    text = err.message;
  }
  const line = text.replace(/\s+/g, " ").trim() || "erreur inconnue";
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

/** A review worth showing: some findings, some still open. */
function sanitizeFindings(v: unknown): FindingsReport | null {
  if (!isObj(v)) return null;
  const count = Math.max(num(v.count) ?? 0, 0);
  const open = Math.min(Math.max(num(v.open) ?? 0, 0), count);
  if (count === 0 || open === 0) return null;
  const confirmed = Math.min(Math.max(num(v.confirmed) ?? 0, 0), count);
  return { count, confirmed, open, at: num(v.at) };
}

function sanitizeSession(raw: unknown): SessionView | null {
  if (!isObj(raw) || typeof raw.key !== "string" || raw.key === "") return null;
  const s = raw as unknown as SessionView;
  const messages = isObj(raw.messages) ? raw.messages : {};
  const pr = isObj(raw.pr) && num(raw.pr.number) !== null && s.pr ? { ...s.pr, autoFix: raw.pr.autoFix === true } : null;
  return {
    ...s,
    title: str(s.title).trim() || str(s.project).trim() || "Session sans titre",
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
    messages: {
      awaitingReply: peerLinks(messages.awaitingReply),
      undelivered: peerLinks(messages.undelivered),
      unreadInbound: peerLinks(messages.unreadInbound),
    },
    findings: sanitizeFindings(raw.findings),
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
  return { sampledAt, fiveHour, sevenDay };
}

function sanitizeTasks(v: unknown): ProposedTask[] {
  if (!Array.isArray(v)) return [];
  const seen = new Set<string>();
  const out: ProposedTask[] = [];
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
      sessionTitle: oneLine(str(t.sessionTitle)) || "une session",
      project: str(t.project),
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
  };
}

function sanitizeSettings(raw: unknown, base: Settings): Settings {
  if (!isObj(raw)) return base;
  const out: Settings = { ...base };
  const target = out as unknown as Obj;
  for (const key of Object.keys(base) as Array<keyof Settings>) {
    if (typeof raw[key] === typeof base[key]) target[key] = raw[key];
  }
  return out;
}

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

function byId<T extends HTMLElement = HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) throw new Error(`Élément #${id} absent de index.html`);
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
/** Blocking problem reaching the backend (shown in the health line). */
let fatalError: string | null = null;
/** Persistent warning about live updates. */
let liveWarning: string | null = null;
/** Short-lived message after a failed command. */
let notice: string | null = null;
/** "error" for a failed command, "info" for a hint (not a problem). */
let noticeTone: "error" | "info" = "error";
let noticeTimer = 0;
let renderQueued = false;
/** Something changed since the last render. */
let dirty = false;
/** The shell says the window is shown (false while hidden to the tray). */
let shown = true;
let ticker = 0;

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

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/** Runs a backend command; failures are shown as a notice, never thrown. */
async function call(cmd: string, args?: Record<string, unknown>): Promise<{ ok: boolean; value: unknown }> {
  if (!bridge) return { ok: false, value: null };
  try {
    return { ok: true, value: await bridge.invoke<unknown>(cmd, args) };
  } catch (err) {
    showNotice(`${COMMAND_FAILURE[cmd] ?? "Commande échouée"} : ${errorText(err)}`);
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
      showNotice("Fenêtre trop basse : tire son bord inférieur pour l'agrandir", "info");
    }
  }, 600);
}

async function setAlwaysOnTop(value: boolean): Promise<void> {
  const previous = settings;
  applySettings({ ...settings, alwaysOnTop: value });
  const res = await call(CMD.setAlwaysOnTop, { value });
  applySettings(res.ok ? sanitizeSettings(res.value, settings) : previous);
}

function showNotice(text: string, tone: "error" | "info" = "error"): void {
  notice = text;
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
  ui.btnPin.title = next.alwaysOnTop
    ? "Toujours au premier plan (activé)"
    : "Toujours au premier plan (désactivé)";
  if (modeChanged) {
    keepFocusVisible();
    // The list is not kept up to date while compact.
    scheduleRender();
  }
}

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
  if (fatalError) return "Moteur injoignable";
  return snapshot ? READING : "Connexion…";
}

interface SummaryPart {
  text: string;
  tone: SessionState | "task";
}

function summaryParts(c: Counts): SummaryPart[] {
  const parts: SummaryPart[] = [];
  if (c.waiting) parts.push({ text: `${c.waiting} ${plural(c.waiting, "t'attend", "t'attendent")}`, tone: "waiting" });
  if (c.error) parts.push({ text: `${c.error} en erreur`, tone: "error" });
  if (c.working) parts.push({ text: `${c.working} au travail`, tone: "working" });
  if (c.unseen) parts.push({ text: `${c.unseen} ${plural(c.unseen, "non vue", "non vues")}`, tone: "unseen" });
  return parts;
}

/** One-line summary when no part applies: empty list or idle sessions only. */
function quietSummary(c: Counts): string {
  if (c.total === 0) return "Aucune session active";
  return `${c.idle} ${plural(c.idle, "session inactive", "sessions inactives")}`;
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
  ui.compactDot.className = `dot dot--${mostUrgent(sessions) ?? "none"}`;

  // Health problems and failed commands: a warning button that opens the list.
  const problems = notice && noticeTone === "error" ? [...rows, { text: notice, tone: "error" as const }] : [...rows];
  ui.compactWarn.hidden = problems.length === 0;
  if (problems.length) {
    const tone = problems.some((r) => r.tone === "error") ? "error" : "warn";
    ui.compactWarn.className = `icon-btn compact-warn compact-warn--${tone}`;
    ui.compactWarn.title = [...problems.map((r) => r.text), "Cliquer pour afficher la liste"].join("\n");
    setAttr(
      ui.compactWarn,
      "aria-label",
      `${plural(problems.length, "Problème", "Problèmes")} : ${problems.map((r) => r.text).join(" ; ")}. Afficher la liste`,
    );
  }

  // A failed command replaces the line for a few seconds, so the click visibly did something.
  setText(ui.compactNotice, notice ?? "");
  ui.compactNotice.title = notice ?? "";
  ui.compactNotice.hidden = notice === null;
  ui.compactNotice.classList.toggle("is-info", noticeTone === "info");

  const waiting = sessions.filter((s) => s.state === "waiting");
  const first = notice === null ? waiting[0] : undefined;
  const openHadFocus = document.activeElement === ui.compactOpen;
  ui.compactOpen.hidden = !first;
  ui.compactSummary.hidden = Boolean(first) || notice !== null;
  if (openHadFocus && !first) (ui.compactWarn.hidden ? ui.btnExpand : ui.compactWarn).focus({ preventScroll: true });

  if (first) {
    renderCompactOpen(first, waiting.length - 1);
    return;
  }

  delete ui.compactOpen.dataset.key;
  let text = snapshot && isReady() ? summaryText(snapshot.counts) : pendingText();
  const prOpen = snapshot && isReady() ? snapshot.counts.prOpen : 0;
  if (prOpen > 0) text += ` · ${prOpen} PR`;
  const tasks = snapshot && isReady() ? snapshot.proposedTasks.length : 0;
  if (tasks > 0) text += ` · ${tasksText(tasks)}`;
  setText(ui.compactSummary, text);
  ui.compactSummary.title = text;
}

/** The compact line for the most urgent waiting session: "<title> · <question or reason>". */
function renderCompactOpen(first: SessionView, more: number): void {
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

  const others = more > 0 ? `${more} ${plural(more, "autre session t'attend", "autres sessions t'attendent")}` : null;
  const lines = [first.title, reason];
  if (question) lines.push(question);
  if (options.length) lines.push(`Réponses proposées : ${options.join(" · ")}`);
  if (others) lines.push(others);
  if (!first.isDesktop) lines.push(CLI_TOOLTIP);
  ui.compactOpen.title = lines.join("\n");

  const what = question ? `${reason} : ${question}` : reason;
  const label = first.isDesktop
    ? `Ouvrir « ${first.title} » : ${what}`
    : `« ${first.title} » : ${what}. Session de terminal, impossible de l'ouvrir dans Claude Desktop`;
  setAttr(ui.compactOpen, "aria-label", others ? `${label}. ${others}` : label);
}

function onCompactOpen(): void {
  const key = ui.compactOpen.dataset.key;
  if (!key) return;
  if (ui.compactOpen.getAttribute("aria-disabled") === "true") {
    showNotice(`« ${ui.compactOpenTitle.textContent ?? ""} » tourne dans un terminal : impossible de l'ouvrir dans Claude Desktop`, "info");
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
    const waiting = snapshot.sessions.filter((s) => s.state === "waiting");
    const first = waiting[0];
    if (first) {
      waitingLine = isTentative(first)
        ? `« ${first.title} » pose peut-être une question : ${waitingText(first)}`
        : `« ${first.title} » t'attend : ${waitingText(first)}`;
      const more = waiting.length - 1;
      if (more > 0) others = `${more} ${plural(more, "autre session t'attend", "autres sessions t'attendent")}`;
    }
    const c = snapshot.counts;
    if (c.error) errors = `${c.error} en erreur`;
    if (c.unseen) unseen = `${c.unseen} ${plural(c.unseen, "session terminée non vue", "sessions terminées non vues")}`;
  }
  return [waitingLine, others, errors, unseen, rows[0]?.text ?? "", notice ?? ""].map(sentence);
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

/** The latest sample is older than this: shown as out of date. */
const QUOTA_STALE_MS = 60 * 60_000;
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
  /** Reset shown after the percent: "05:25", "jeu."; empty when unknown. */
  reset: string;
  tooltip: string;
  /** Spoken value. */
  spoken: string;
}

const clock = (ts: number): string => new Date(ts).toLocaleTimeString("fr-FR", { hour: "2-digit", minute: "2-digit" });

const quotaTone = (percent: number): QuotaTone =>
  percent >= QUOTA_HIGH ? "high" : percent >= QUOTA_WARN ? "warn" : "ok";

/** Share of the window's time already gone, from its estimated reset, to the half percent. */
function elapsedOf(w: QuotaWindow, now: number): number | null {
  if (w.resetsAt === null || w.expired) return null;
  const share = ((now - (w.resetsAt - w.lengthMs)) / w.lengthMs) * 100;
  return Math.round(Math.min(Math.max(share, 0), 100) * 2) / 2;
}

function meterModel(week: boolean, w: QuotaWindow, sampledAt: number, now: number): MeterModel {
  const label = week ? "7 j" : "5 h";
  const name = week ? "Semaine" : "Fenêtre de 5 h";
  const shown = Math.round(w.percent);
  const sampled = `Relevé par Claude Desktop : ${formatClock(sampledAt)}`;
  if (w.expired) {
    return {
      label,
      percent: null,
      tone: "ok",
      elapsed: null,
      reset: "",
      tooltip: `${name} : réinitialisée depuis le dernier relevé (${shown} % alors)\n${sampled}`,
      spoken: "réinitialisée depuis le dernier relevé",
    };
  }
  const lines = [`${name} : ${shown} % utilisés`];
  let reset = "";
  let resetPhrase = "";
  if (w.resetsAt !== null) {
    const lo = w.resetsAt - w.resetMarginMs;
    const hi = w.resetsAt + w.resetMarginMs;
    const earliest = clock(lo);
    const latest = clock(hi);
    if (week) {
      const day = (ts: number, weekday: "long" | "short"): string =>
        new Date(ts).toLocaleDateString("fr-FR", { weekday, day: "numeric", month: "short" });
      reset = new Date(w.resetsAt).toLocaleDateString("fr-FR", { weekday: "short" });
      resetPhrase =
        new Date(lo).toDateString() === new Date(hi).toDateString()
          ? `réinitialisation ${day(lo, "long")} entre ${earliest} et ${latest}`
          : `réinitialisation entre ${day(lo, "short")} ${earliest} et ${day(hi, "short")} ${latest}`;
    } else {
      reset = clock(w.resetsAt);
      resetPhrase = `réinitialisation vers ${reset} (entre ${earliest} et ${latest})`;
    }
    lines.push(resetPhrase.charAt(0).toUpperCase() + resetPhrase.slice(1));
  } else if (w.percent > 0) {
    lines.push("Heure de réinitialisation inconnue");
  }
  const elapsed = elapsedOf(w, now);
  if (elapsed !== null) {
    lines.push(`Temps écoulé : ${Math.round(elapsed)} % ${week ? "de la semaine" : "de la fenêtre"} (zone teintée derrière la barre)`);
  }
  lines.push(sampled);
  return {
    label,
    percent: shown,
    tone: quotaTone(w.percent),
    elapsed,
    reset,
    tooltip: lines.join("\n"),
    spoken: [`${shown} % utilisés`, resetPhrase].filter(Boolean).join(", "),
  };
}

function quotaModels(now: number): MeterModel[] {
  const quota = snapshot && isReady() ? snapshot.quota : null;
  if (!quota) return [];
  const out: MeterModel[] = [];
  if (quota.fiveHour) out.push(meterModel(false, quota.fiveHour, quota.sampledAt, now));
  if (quota.sevenDay) out.push(meterModel(true, quota.sevenDay, quota.sampledAt, now));
  return out;
}

function buildMeter(m: MeterModel): HTMLElement {
  const meter = dragHandle(h("div", `meter meter--${m.tone}`));
  meter.title = m.tooltip;
  if (m.percent === null) {
    meter.classList.add("meter--expired");
    meter.setAttribute("role", "img");
    meter.setAttribute("aria-label", `Quota ${m.label} : ${m.spoken}`);
  } else {
    meter.setAttribute("role", "meter");
    meter.setAttribute("aria-label", `Quota ${m.label}`);
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
    dragHandle(h("span", "meter-value", m.percent === null ? "—" : `${m.percent} %`)),
  );
  if (m.reset) meter.append(dragHandle(h("span", "meter-reset", m.reset)));
  return meter;
}

/** Called on every render and every second: rebuilds only what changed. */
function renderQuota(): void {
  const now = Date.now();
  const models = quotaModels(now);
  const sampledAt = snapshot?.quota?.sampledAt ?? null;
  const age = models.length && sampledAt !== null && now - sampledAt > QUOTA_STALE_MS ? formatAgo(now - sampledAt) : "";

  ui.quota.hidden = models.length === 0;
  const sig = JSON.stringify([models, age]);
  if (ui.quota.dataset.sig !== sig) {
    ui.quota.dataset.sig = sig;
    ui.quota.classList.toggle("is-stale", age !== "");
    const nodes = models.map(buildMeter);
    if (age) {
      const stale = dragHandle(h("span", "quota-age", `il y a ${age}`));
      stale.title = "Claude Desktop n'a pas relevé le quota depuis : il le fait quand il est ouvert";
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
    setText(ui.compactQuota, `${top.label} ${top.percent} %`);
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
  sessions: SessionView[];
  /** "New task" cards proposed in the project, newest first, after its sessions. */
  tasks: ProposedTask[];
  /** Index of the group's first session in the backend order (tasks count after every session). */
  first: number;
}

/**
 * Sessions and proposed tasks by project. Groups holding a waiting session come
 * first (the one waiting longest first), then error, unseen, working, idle-only
 * groups, and last the groups with tasks only. Inside a group the backend order
 * (by urgency) is kept.
 */
function groupSessions(sessions: readonly SessionView[], tasks: readonly ProposedTask[]): Group[] {
  const byKey = new Map<string, Group>();
  const groupOf = (project: string, first: number): Group => {
    const key = project.trim();
    let group = byKey.get(key);
    if (!group) {
      group = { key, label: key || "Sans projet", sessions: [], tasks: [], first };
      byKey.set(key, group);
    }
    return group;
  };
  sessions.forEach((s, i) => groupOf(s.project, i).sessions.push(s));
  tasks.forEach((t, i) => groupOf(t.project, sessions.length + i).tasks.push(t));
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

/** What needs the user in a group, in a few words ("1 t'attend · 1 en erreur · 2 tâches proposées"). */
function groupNeeds(g: Group): SummaryPart[] {
  const waiting = g.sessions.filter((s) => s.state === "waiting").length;
  const error = g.sessions.filter((s) => s.state === "error").length;
  const tasks = g.tasks.length;
  const parts: SummaryPart[] = [];
  if (waiting) parts.push({ text: `${waiting} ${plural(waiting, "t'attend", "t'attendent")}`, tone: "waiting" });
  if (error) parts.push({ text: `${error} en erreur`, tone: "error" });
  if (tasks) parts.push({ text: tasksText(tasks), tone: "task" });
  return parts;
}

const tasksText = (n: number): string => `${n} ${plural(n, "tâche proposée", "tâches proposées")}`;

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
  const tasks = snapshot && isReady() ? snapshot.proposedTasks : [];
  // Removing or moving a node blurs it: remember the focus by session key.
  const active = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const activeCard = active && ui.cards.contains(active) ? active.closest<HTMLElement>(".card") : null;
  const focusKey = activeCard?.dataset.key ?? null;
  const focusPr = active?.dataset.focus === "pr";
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
    for (const s of g.sessions) {
      keepCards.add(s.key);
      let entry = cardEntries.get(s.key);
      if (entry && (entry.hit !== null) !== s.isDesktop) {
        entry.el.remove();
        entry = undefined;
      }
      if (!entry) {
        entry = createCard(s);
        cardEntries.set(s.key, entry);
      }
      fillCard(entry, s);
      place(group.list, entry.el, prev);
      prev = entry.el;
    }
    for (const t of g.tasks) {
      const key = taskKey(t);
      keepCards.add(key);
      let entry = cardEntries.get(key);
      if (!entry) {
        entry = createTaskCard(t);
        cardEntries.set(key, entry);
      }
      fillTaskCard(entry, t);
      place(group.list, entry.el, prev);
      prev = entry.el;
    }
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

  if (focusKey && !ui.cards.contains(document.activeElement)) restoreFocus(focusKey, focusPr, focusIndex);

  ui.empty.hidden = sessions.length > 0 || tasks.length > 0;
  if (!ui.empty.hidden) {
    const ready = isReady();
    setText(ui.emptyTitle, ready ? "Aucune session active" : pendingText());
    setText(ui.emptyHint, ready ? "Les sessions de l'onglet Code de Claude Desktop apparaîtront ici." : "");
  }
}

/** Puts the focus back on the same card, or on the card now at its place if it left. */
function restoreFocus(key: string, pr: boolean, index: number): void {
  const entry = cardEntries.get(key);
  let target: HTMLElement | null = null;
  if (entry) {
    target = (pr ? entry.el.querySelector<HTMLElement>('[data-focus="pr"]') : null) ?? entry.hit;
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

function fillCard(entry: CardEntry, s: SessionView): void {
  const actionable = entry.hit !== null;
  fillEntry(entry, {
    className: `card card--${s.state} ${actionable ? "card--action" : "card--static"}`,
    name: actionable ? cardName(s) : "",
    description: actionable ? cardDescription(s) : "",
    tooltip: cardTooltip(s),
    sections: cardSections(s),
  });
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

const TASK_HINT = "Clic : ouvrir la session dans Claude Desktop, pour lancer ou écarter la tâche";

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

function fillTaskCard(entry: CardEntry, t: ProposedTask): void {
  const lines = [`Tâche proposée : ${t.title}`];
  if (t.tldr) lines.push(clip(t.tldr, 300));
  lines.push(`Proposée par ${t.sessionTitle}, ${formatClock(t.createdAt)}`);
  if (t.expiresAt !== null && t.expiresAt <= t.listedUntil) {
    lines.push(`Desktop l'écarte ${formatWhen(t.expiresAt)} si elle n'est pas lancée`);
  } else {
    // Desktop keeps it; the monitor only lists current work.
    lines.push(`Masquée ici ${formatWhen(t.listedUntil)} ; Desktop la garde jusqu'à ce que tu la lances ou l'écartes`);
  }
  lines.push(TASK_HINT);
  fillEntry(entry, {
    className: "card card--task card--action",
    name: `Tâche proposée : ${t.title}, par ${t.sessionTitle}. Ouvrir la session dans Claude Desktop`,
    description: t.tldr ?? "",
    tooltip: lines.join("\n"),
    sections: [
      section("head", { title: t.title, createdAt: t.createdAt }, buildTaskHead),
      section("line", { by: t.sessionTitle }, (m) => h("div", "card-line", `Proposée par ${m.by}`)),
    ],
  });
}

function buildTaskHead(m: { title: string; createdAt: number }): HTMLElement {
  const head = h("div", "card-head");
  const mark = h("span", "task-mark");
  mark.append(icon("plus"));
  const status = h("span", "card-status");
  status.append(
    h("span", "status-label", "proposée"),
    " · ",
    sinceSpan(m.createdAt, "status-since", `Proposée ${formatClock(m.createdAt)}`),
  );
  head.append(mark, h("span", "card-title", m.title), status);
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
  return { id, sig: JSON.stringify(model), build: () => build(model) };
}

/** Line 1 (state, title, time) and, when it answers "does it need me / where is it", line 2. */
function cardSections(s: SessionView): Section[] {
  const out: Section[] = [section("head", headModel(s), buildHead)];
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
  s.parent ? str(s.parent.title).trim() || "une autre session" : null;
const peerName = (link: PeerLink): string => str(link.peerTitle).trim() || "session inconnue";
const clip = (text: string, max: number): string => (text.length > max ? `${text.slice(0, max - 1).trimEnd()}…` : text);
const ERROR_FALLBACK = "Le dernier tour s'est terminé sur une erreur";

/** Accessible name of a card's button: title, state and what it is about. */
function cardName(s: SessionView): string {
  const parts = [s.title, STATE_LABEL[s.state]];
  switch (s.state) {
    case "waiting":
      parts.push(reasonText(s));
      if (s.question) parts.push(s.question.text);
      else if (s.activity && REASONS_WITH_TOOL.has(reasonKey(s.waitingFor))) parts.push(activityText(s.activity));
      break;
    case "working":
      if (s.activity) parts.push(activityText(s.activity));
      break;
    case "error":
      parts.push(s.lastError ?? ERROR_FALLBACK);
      break;
    case "unseen":
    case "idle":
      break;
  }
  return parts.join(", ");
}

/** Accessible description: what the tooltip and the card tell beyond the name. */
function cardDescription(s: SessionView): string {
  const out: string[] = [];
  if ((s.state === "unseen" || s.state === "idle") && s.recap) out.push(s.recap);
  if (s.state === "waiting" && s.question?.options.length) {
    out.push(`Réponses proposées : ${s.question.options.join(", ")}`);
  }
  if (s.findings) out.push(findingsText(s.findings));
  if (s.onScreen) out.push("Affichée dans Claude Desktop");
  const where = [s.project ? `projet ${s.project}` : "", branchOf(s) ? `branche ${branchOf(s)}` : ""].filter(Boolean);
  if (where.length) out.push(where.join(", "));
  const dirty = dirtyOf(s);
  if (dirty > 0) out.push(`${dirty} ${plural(dirty, "fichier modifié non commité", "fichiers modifiés non commités")}`);
  const plan = planModel(s);
  if (plan) out.push(`Plan : ${plan.done} sur ${plan.total}${plan.current ? `, en cours : ${plan.current}` : ""}`);
  const pr = prModel(s);
  if (pr) out.push(`PR #${pr.number}, ${prStatus(pr, s.state)}`);
  out.push(...extrasLines(extrasModel(s)));
  const parent = parentTitle(s);
  if (parent) out.push(`Lancée depuis ${parent}`);
  return out.map(sentence).join(" ");
}

/** The card's tooltip: the details that no longer fit on it, one per line. */
function cardTooltip(s: SessionView): string {
  const lines: string[] = [];
  if (!s.isDesktop) lines.push(CLI_TOOLTIP);
  if (s.state === "waiting") {
    const reason = reasonFr(s.waitingFor);
    const plan = planTitle(s);
    if (s.question) {
      lines.push(`${reason} : ${clip(s.question.text, 300)}`);
      if (s.question.options.length) lines.push(`Réponses proposées : ${s.question.options.join(" · ")}`);
    }
    if (plan) {
      const at = s.activity?.at ?? null;
      lines.push(`${reason} : ${clip(plan, 300)}${at !== null ? ` (${formatClock(at)})` : ""}`);
    }
    if (s.activity && REASONS_WITH_TOOL.has(reasonKey(s.waitingFor))) {
      lines.push(`Demande : ${clip(activityText(s.activity), 160)}`);
    }
  }
  if (s.state === "error" && s.lastError) lines.push(`Erreur : ${clip(s.lastError, 300)}`);
  if (s.findings) lines.push(findingsText(s.findings));
  const branch = str(s.git?.branch).trim();
  const worktree = str(s.worktree).trim();
  if (branch) lines.push(`Branche : ${branch}`);
  if (worktree) lines.push(`Worktree : ${worktree}`);
  const dirty = dirtyOf(s);
  if (dirty > 0) lines.push(`${dirty} ${plural(dirty, "fichier modifié non commité", "fichiers modifiés non commités")}`);
  const pr = prModel(s);
  if (pr && !isOpen(pr)) lines.push(`PR #${pr.number} ${prStatus(pr, s.state)}`);
  if (s.recap) lines.push(`Résumé : ${clip(oneLine(s.recap), 200)}`);
  // For a plan to approve, the activity is the plan call itself, already shown above.
  const planShown = planTitle(s) !== null;
  if (s.activity && !planShown) {
    const at = s.activity.at !== null ? ` (${formatClock(s.activity.at)})` : "";
    lines.push(`Dernière activité : ${clip(activityText(s.activity), 120)}${at}`);
  } else if (!s.activity && s.lastActivityAt !== null && num(s.lastActivityAt) !== null) {
    lines.push(`Dernière activité : ${formatClock(s.lastActivityAt)}`);
  }
  const parent = parentTitle(s);
  if (parent) lines.push(`Lancée depuis ${parent}`);
  lines.push(...extrasLines(extrasModel(s)));
  if (s.stopped) lines.push(STOPPED_TOOLTIP);
  if (s.onScreen) lines.push(ON_SCREEN_TOOLTIP);
  return lines.join("\n");
}

// Line 1: dot, title, eye, CLI tag, open PR chip, "attend · 3 h".

const STATE_SHORT: Record<SessionState, string> = {
  waiting: "attend",
  error: "erreur",
  unseen: "non vue",
  working: "en cours",
  idle: "",
};

interface HeadModel {
  state: SessionState;
  title: string;
  since: number | null;
  onScreen: boolean;
  cli: boolean;
  pr: PrModel | null;
}

const headModel = (s: SessionView): HeadModel => {
  const pr = prModel(s);
  return {
    state: s.state,
    title: s.title,
    since: s.stateSince,
    onScreen: s.onScreen,
    cli: !s.isDesktop,
    pr: pr && isOpen(pr) ? pr : null,
  };
};

function buildHead(m: HeadModel): HTMLElement {
  const head = h("div", "card-head");
  const dot = h("span", `dot dot--${m.state}`);
  head.append(dot, h("span", "card-title", m.title));
  if (m.onScreen) {
    const eye = h("span", "card-eye");
    eye.title = ON_SCREEN_TOOLTIP;
    eye.append(icon("eye"));
    head.append(eye);
  }
  if (m.cli) {
    const tag = h("span", "card-tag", "CLI");
    tag.title = CLI_TOOLTIP;
    head.append(tag);
  }
  if (m.pr) head.append(prChip(m.pr, m.state));
  const label = STATE_SHORT[m.state];
  if (label || m.since !== null) {
    const status = h("span", `card-status status--${m.state}`);
    if (label) status.append(h("span", "status-label", label));
    if (label && m.since !== null) status.append(" · ");
    if (m.since !== null) {
      status.append(sinceSpan(m.since, "status-since", `${STATE_LABEL[m.state]} depuis ${formatClock(m.since)}`));
    }
    head.append(status);
  }
  return head;
}

// Line 2: the question or reason, the error, the plan, or a finished review's findings.

type LineModel =
  | { kind: "question" | "reason"; text: string; tentative: boolean }
  | { kind: "error" | "findings"; text: string }
  | ({ kind: "plan" } & PlanModel);

function lineModel(s: SessionView): LineModel | null {
  switch (s.state) {
    case "waiting":
      return s.question
        ? { kind: "question", text: s.question.text, tentative: isTentative(s) }
        : { kind: "reason", text: reasonText(s), tentative: isTentative(s) };
    case "error":
      return { kind: "error", text: oneLine(s.lastError ?? ERROR_FALLBACK) };
    case "working": {
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

const AUTO_FIX_TOOLTIP =
  "Auto-fix : Claude Desktop relance la session quand la CI échoue, en cas de conflit ou de commentaire de revue";

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

const AUTO_FIX_PHRASE = { on: "Auto-fix actif", handling: "Auto-fix s'en charge" } as const;

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
  const state = str(pr.state).toUpperCase();
  if (state === "MERGED") return "fusionnée";
  if (state === "CLOSED") return "fermée";
  const checks = CHECKS_PHRASE[pr.checks] ?? "checks inconnus";
  const autoFix = autoFixState(pr, session);
  const status = pr.isDraft ? `brouillon, ${checks}` : `ouverte, ${checks}`;
  return autoFix ? `${status}, ${AUTO_FIX_PHRASE[autoFix]}` : status;
}

function prTooltip(pr: PrModel, session: SessionState): string {
  const title = str(pr.title).trim();
  const lines = [`PR #${pr.number}${title ? ` · ${title}` : ""}`];
  lines.push(pr.isDraft ? "Ouverte · brouillon" : "Ouverte");
  lines.push(`Checks : ${CHECKS_LABEL[pr.checks] ?? "inconnus"}`);
  const review = str(pr.reviewDecision).trim();
  if (review) lines.push(`Revue : ${REVIEW_LABEL[review] ?? review}`);
  const merge = str(pr.mergeState).trim();
  if (merge) lines.push(`Fusion : ${MERGE_LABEL[merge] ?? merge.toLowerCase()}`);
  const repo = str(pr.repository).trim();
  if (repo) lines.push(repo);
  const autoFix = autoFixState(pr, session);
  if (autoFix) lines.push(AUTO_FIX_TOOLTIP);
  if (autoFix === "handling") lines.push("La session y travaille : Auto-fix s'en charge");
  return lines.join("\n");
}

function prChip(pr: PrModel, session: SessionState): HTMLElement {
  const variant = prVariant(pr);
  const autoFix = autoFixState(pr, session) !== null;
  const btn = h("button", `chip chip--pr chip--${variant}${pr.isDraft ? " chip--draft" : ""}${autoFix ? " chip--autofix" : ""}`);
  btn.type = "button";
  btn.dataset.focus = "pr";
  btn.title = prTooltip(pr, session);
  btn.setAttribute("aria-label", `Ouvrir la PR #${pr.number} (${prStatus(pr, session)})`);
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
  const out: string[] = [];
  for (const peer of m.awaiting) out.push(`Attend une réponse de ${peer}`);
  for (const u of m.unread) out.push(`${u.count} ${plural(u.count, "message non lu", "messages non lus")} de ${u.name}`);
  for (const peer of m.undelivered) out.push(`Message non livré à ${peer}`);
  if (m.exceeded > 0) out.push(m.exceeded > 1 ? `Contexte dépassé ${m.exceeded} fois` : "Contexte dépassé");
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
  if (fatalError) rows.push({ text: fatalError, tone: "error" });
  if (liveWarning) rows.push({ text: liveWarning, tone: "warn" });
  if (snapshot) {
    const health = snapshot.health;
    // Before the engine's first pass the source flags are defaults, not failures.
    if (snapshot.ready) {
      if (!health.registryOk) rows.push({ text: "Registre des sessions introuvable", tone: "warn" });
      if (!health.desktopOk) rows.push({ text: "Fichiers de Claude Desktop illisibles", tone: "warn" });
      if (health.ghOk === false) rows.push({ text: "gh indisponible : statut des PR en pause", tone: "warn" });
    }
    if (health.errors.length) rows.push({ text: health.errors[0], tone: "warn" });
  }
  return rows;
}

function renderHealth(base: readonly HealthRow[]): void {
  const tone = noticeTone === "info" ? ("warn" as const) : ("error" as const);
  const rows = notice ? [...base, { text: notice, tone }] : base;
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
  const next = sanitizeSnapshot(raw);
  if (!next) return;
  if (snapshot) {
    if (next.seq < snapshot.seq) return;
    if (origin === "fetch" && gotEvent && next.seq === snapshot.seq) return;
  }
  if (origin === "event") gotEvent = true;
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
  const caption = h("p", "mock-caption", "Mode démo hors Tauri, données fictives. Scénarios : ");
  const scenarios: Array<[string, string]> = [
    ["complet", "?"],
    ["fenêtre large", "?wide"],
    ["vide", "?scenario=empty"],
    ["sources en panne", "?scenario=health"],
  ];
  scenarios.forEach(([label, href], i) => {
    if (i > 0) caption.append(" · ");
    const a = h("a", "", label);
    a.href = href;
    caption.append(a);
  });
  ui.app.after(caption);
}

async function loadSnapshot(): Promise<void> {
  if (!bridge) return;
  try {
    acceptSnapshot(await bridge.invoke<unknown>(CMD.getSnapshot), "fetch");
  } catch (err) {
    fatalError = `Impossible de lire l'état des sessions : ${errorText(err)}`;
    scheduleRender();
  }
}

async function loadSettings(): Promise<void> {
  if (!bridge) return;
  try {
    applySettings(sanitizeSettings(await bridge.invoke<unknown>(CMD.getSettings), settings));
  } catch (err) {
    showNotice(`Réglages illisibles : ${errorText(err)}`);
  }
}

async function start(): Promise<void> {
  bindUi();
  applySettings(settings);
  scheduleRender();

  try {
    bridge = await createBridge();
  } catch (err) {
    fatalError = `Initialisation impossible : ${errorText(err)}`;
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
    liveWarning = `Mises à jour en direct indisponibles (${errorText(err)}), actualisation toutes les 2 s`;
  }

  await Promise.all([loadSnapshot(), loadSettings()]);
  if (!live) window.setInterval(() => void loadSnapshot(), 2000);
  syncTicker();
}

void start();
