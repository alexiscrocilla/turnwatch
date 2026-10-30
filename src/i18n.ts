// UI text in English and French. The engine resolves the language and sends it
// with every snapshot; until the first one, the browser's language stands in.
// Both tables have the same shape (`fr: typeof en`): a key missing from one does
// not compile. Sentences stay whole here rather than assembled by the callers:
// word order differs, and so does spacing ("82 %", "Quota : x", « x » in French).

import { CMD } from "./types";

export type Lang = "en" | "fr";

export const isLang = (v: unknown): v is Lang => v === "en" || v === "fr";

/** The language for a BCP 47 tag: "fr*" is French, everything else English. */
export const langOf = (tag: string): Lang => (tag.toLowerCase().startsWith("fr") ? "fr" : "en");

let lang: Lang = langOf(navigator.language);

export const getLang = (): Lang => lang;

export function setLang(next: Lang): void {
  lang = next;
}

/** Locale of the dates and times shown; en-GB for its 24-hour clock. */
export const dateLocale = (): string => (lang === "fr" ? "fr-FR" : "en-GB");

/** A time of day, with its date unless that is today. */
export interface Moment {
  time: string;
  date: string | null;
}

/** English plural: only 1 takes the singular. */
const pluralEn = (n: number, one: string, many: string): string => (n === 1 ? one : many);

/** French plural: 0 and 1 take the singular. */
const pluralFr = (n: number, one: string, many: string): string => (n > 1 ? many : one);

const whenEn = (m: Moment): string => (m.date ? `on ${m.date} at ${m.time}` : `at ${m.time}`);
const clockFr = (m: Moment): string => (m.date ? `${m.date} à ${m.time}` : m.time);
const resultsFr = (n: number): string => `${n} ${pluralFr(n, "résultat", "résultats")}`;

const en = {
  // Window
  windowTitle: "Claude Sessions",
  sessionsRegion: "Sessions",
  compactTitle: "Compact mode",
  compactLabel: "Switch to compact mode",
  pinLabel: "Always on top",
  pinOn: "Always on top (on)",
  pinOff: "Always on top (off)",
  hideTitle: "Hide (stays in the notification area)",
  hideLabel: "Hide the window",
  expandTitle: "Expand",
  expandLabel: "Show the session list",
  windowTooShort: "Window too short: drag its bottom edge to enlarge it",

  // Loading and failures
  connecting: "Connecting…",
  reading: "Reading sessions…",
  engineUnreachable: "Engine unreachable",
  unknownError: "unknown error",
  commandFailed: "Command failed",
  commandFailures: {
    [CMD.openSession]: "Couldn't open the session",
    [CMD.openUrl]: "Couldn't open the PR",
    [CMD.setCompact]: "Couldn't switch mode",
    [CMD.setAlwaysOnTop]: "Couldn't change the pin",
    [CMD.hideWindow]: "Couldn't hide the window",
  },
  stateUnreadable: (why: string) => `Couldn't read the session state: ${why}`,
  settingsUnreadable: (why: string) => `Settings unreadable: ${why}`,
  startFailed: (why: string) => `Couldn't start: ${why}`,
  liveUnavailable: (why: string) => `Live updates unavailable (${why}), refreshing every 2 s`,

  // Health line
  registryMissing: "Session registry not found",
  desktopUnreadable: "Claude Desktop files unreadable",
  ghDown: "gh unavailable: PR status paused",

  // Fallbacks for what the backend leaves empty
  untitledSession: "Untitled session",
  aSession: "a session",
  anotherSession: "another session",
  unknownSession: "an unknown session",
  unnamedTask: "unnamed task",
  noProject: "No project",

  // Times
  seconds: (n: number) => `${n} s`,
  minutes: (n: number) => `${n} min`,
  hours: (n: number) => `${n} h`,
  days: (n: number) => `${n} d`,
  /** "14:05", or "29 Oct at 14:05" on another day. */
  clock: (m: Moment) => (m.date ? `${m.date} at ${m.time}` : m.time),
  /** The same after a verb: "at 14:05", "on 29 Oct at 14:05". */
  when: whenEn,

  // Text put together from pieces that are already worded
  /** "Label: text". */
  labeled: (label: string, text: string) => `${label}: ${text}`,
  percent: (n: number) => `${n}%`,

  // States
  states: {
    waiting: "Waiting",
    error: "Error",
    unseen: "Finished, unseen",
    working: "Working",
    idle: "Idle",
  },
  statesShort: {
    waiting: "waiting",
    error: "error",
    unseen: "unseen",
    working: "working",
    idle: "",
  },
  stateProposed: "proposed",
  stateSince: (state: string, clock: string) => `${state} since ${clock}`,
  reasons: {
    "permission prompt": "Permission requested",
    "input needed": "Question pending",
    "sandbox request": "Sandbox access requested",
    "worker request": "Worker request",
    "dialog open": "Dialog open",
    decision: "Decision needed",
    "probable question": "Question in the last message",
    offer: "Choice requested",
    "plan approval": "Plan to approve",
  },

  // Summary
  waitingCount: (n: number) => `${n} waiting`,
  errorCount: (n: number) => `${n} ${pluralEn(n, "error", "errors")}`,
  workingCount: (n: number) => `${n} working`,
  unseenCount: (n: number) => `${n} unseen`,
  noSessions: "No sessions",
  idleSessions: (n: number) => `${n} idle ${pluralEn(n, "session", "sessions")}`,
  prCount: (n: number) => `${n} ${pluralEn(n, "PR", "PRs")}`,
  proposedTasks: (n: number) => `${n} proposed ${pluralEn(n, "task", "tasks")}`,
  emptyHint: "Sessions from Claude Desktop's Code tab will show up here.",

  // Compact bar and screen readers
  clickToShowList: "Click to show the list",
  problems: (texts: readonly string[]) =>
    `${pluralEn(texts.length, "Problem", "Problems")}: ${texts.join("; ")}. Show the list`,
  othersWaiting: (n: number) => (n === 1 ? "1 other session is waiting" : `${n} other sessions are waiting`),
  suggestedAnswers: (answers: string) => `Suggested answers: ${answers}`,
  openTitled: (title: string, what: string) => `Open "${title}": ${what}`,
  terminalTitled: (title: string, what: string) =>
    `"${title}": ${what}. Terminal session, can't be opened in Claude Desktop`,
  terminalNotice: (title: string) => `"${title}" runs in a terminal: can't open it in Claude Desktop`,
  liveWaiting: (title: string, what: string) => `"${title}" is waiting for you: ${what}`,
  liveTentative: (title: string, what: string) => `"${title}" may be asking a question: ${what}`,
  liveUnseen: (n: number) => `${n} finished ${pluralEn(n, "session", "sessions")} unseen`,

  // Quota
  gaugeFiveHours: "5 h",
  gaugeWeek: "7 d",
  windowFiveHours: "5-hour window",
  windowWeek: "Week",
  quotaGauge: (gauge: string) => `Quota ${gauge}`,
  percentUsed: (n: number) => `${n}% used`,
  lastSample: "Last sample",
  sampledByDesktop: "Sampled by Claude Desktop",
  quotaExpired: (n: number) => `reset since the last sample (${n}% then)`,
  quotaExpiredSpoken: "reset since the last sample",
  resetsAt: (time: string) => `resets at ${time}`,
  resetsAround: (time: string, earliest: string, latest: string) =>
    `resets around ${time} (between ${earliest} and ${latest})`,
  resetsDayAt: (day: string, time: string) => `resets ${day} at ${time}`,
  resetsDayBetween: (day: string, earliest: string, latest: string) =>
    `resets ${day} between ${earliest} and ${latest}`,
  resetsBetween: (firstDay: string, earliest: string, lastDay: string, latest: string) =>
    `resets between ${firstDay} ${earliest} and ${lastDay} ${latest}`,
  resetUnknown: "Reset time unknown",
  timeElapsed: (n: number, week: boolean) =>
    `Time elapsed: ${n}% of the ${week ? "week" : "window"} (tinted band behind the bar)`,
  quotaStale: (since: string) => `No sample since ${since}: right-click Claude Desktop's icon to restart it`,
  quotaResumeHint:
    "Claude Desktop only samples the quota when it opens and for 5 h after a right-click on its icon in the notification area. A right-click on that icon restarts sampling.",

  // Nested sessions and folded rows
  nestedSessions: (n: number) => `${n} nested ${pluralEn(n, "session", "sessions")}`,
  nestedWorking: (n: number) => `${n} working`,
  foldAction: (open: boolean, what: string) => `${open ? "Collapse" : "Show"} ${what}`,
  foldHint: (open: boolean, nested: boolean) =>
    `Click: ${open ? "collapse" : "show"} ${nested ? "the nested sessions" : "these tasks"}`,
  collapseNested: "Collapse nested sessions",
  expandNested: "Expand nested sessions",

  // Proposed tasks
  proposedTask: (title: string) => `Proposed task: ${title}`,
  proposedBy: (session: string) => `Proposed by ${session}`,
  proposedByAt: (session: string, clock: string) => `Proposed by ${session}, ${clock}`,
  proposedAt: (clock: string) => `Proposed: ${clock}`,
  taskPassedOver: "Collapsed: you've written to this session twice since",
  taskExpires: (when: string) => `Desktop drops it ${when} if it isn't started`,
  taskListedUntil: (when: string) => `Hidden here ${when}; Desktop keeps it until you start or dismiss it`,
  taskHint: "Click: open the session in Claude Desktop to start or dismiss the task",
  taskCardName: (title: string, session: string) =>
    `Proposed task: ${title}, by ${session}. Open the session in Claude Desktop`,
  hideTaskHint: "Not for me: hide it here. It stays in its session in Claude Desktop",
  hideTaskLabel: (title: string) => `Hide the task "${title}" here`,
  taskHidden: "Task hidden here: it stays in its session in Claude Desktop",

  // Cards
  cliTooltip: "Session started in a terminal: Claude Desktop can't open it",
  onScreenTooltip: "Shown in Claude Desktop, in the foreground",
  stoppedTooltip: "Paused: Claude Desktop stopped its process. It resumes as soon as you write to it.",
  errorFallback: "The last turn ended with an error",
  shownInDesktop: "Shown in Claude Desktop",
  inProject: (project: string) => `project ${project}`,
  onBranch: (branch: string) => `branch ${branch}`,
  dirtyFiles: (n: number) => `${n} uncommitted ${pluralEn(n, "file", "files")}`,
  planProgress: (done: number, total: number, current: string) =>
    `Plan: ${done} of ${total}${current ? `, in progress: ${current}` : ""}`,
  startedFrom: (parent: string) => `Started from ${parent}`,
  requestLine: (text: string) => `Request: ${text}`,
  errorLine: (text: string) => `Error: ${text}`,
  branchLine: (branch: string) => `Branch: ${branch}`,
  worktreeLine: (worktree: string) => `Worktree: ${worktree}`,
  recapLine: (text: string) => `Summary: ${text}`,
  activityLine: (text: string) => `Last activity: ${text}`,
  subAgents: (n: number, names: string) => `${n} ${pluralEn(n, "sub-agent", "sub-agents")}: ${names}`,

  // Code review
  findingsOpen: (open: number, count: number) => `Review: ${open} of ${count} findings still open`,
  findingsAllConfirmed: (count: number) =>
    count > 1 ? `Review: ${count} findings, all confirmed` : "Review: 1 confirmed finding",
  findings: (count: number, confirmed: number) =>
    `Review: ${count} ${pluralEn(count, "finding", "findings")}${confirmed > 0 ? `, ${confirmed} confirmed` : ""}`,

  // What a session waits for after its turn
  background: {
    command: "Waiting for a command",
    agent: "Waiting for an agent",
    workflow: "Waiting for a workflow",
    monitor: "Monitoring",
  },
  waitingForSession: "Waiting for a session",
  waitingForSessions: (n: number) => `Waiting for ${n} sessions`,
  waitingForTask: "Waiting for a task",
  waitingForTasks: (n: number) => `Waiting for ${n} tasks`,
  resumesAt: (at: Moment) => `Resumes ${whenEn(at)}`,
  willResume: "Will resume",

  // Pull requests
  checks: {
    pass: "passed",
    fail: "failed",
    pending: "running",
    none: "none",
  },
  checksUnknown: "unknown",
  checksPhrases: {
    pass: "checks passed",
    fail: "checks failed",
    pending: "checks running",
    none: "no checks",
  },
  checksPhraseUnknown: "checks unknown",
  reviews: {
    APPROVED: "approved",
    CHANGES_REQUESTED: "changes requested",
    REVIEW_REQUIRED: "required",
  },
  merges: {
    CLEAN: "ready to merge",
    BLOCKED: "blocked",
    DIRTY: "conflicts to resolve",
    BEHIND: "behind the base branch",
    UNSTABLE: "optional checks failing",
    HAS_HOOKS: "ready (hooks)",
    DRAFT: "draft",
    UNKNOWN: "unknown state",
  },
  prMerged: "merged",
  prClosed: "closed",
  prDraftStatus: (checks: string) => `draft, ${checks}`,
  prOpenStatus: (checks: string) => `open, ${checks}`,
  prOpen: "Open",
  prOpenDraft: "Open · draft",
  prChecks: (status: string) => `Checks: ${status}`,
  prReview: (status: string) => `Review: ${status}`,
  prMerge: (status: string) => `Merge: ${status}`,
  prOpenLabel: (number: number, status: string) => `Open PR #${number} (${status})`,
  autoFixOn: "Auto-fix on",
  autoFixHandling: "Auto-fix is on it",
  autoFixTooltip: "Auto-fix: Claude Desktop restarts the session when CI fails, on a conflict or on a review comment",
  autoFixWorking: "The session is working on it: Auto-fix is on it",

  // Messages between sessions and warnings
  awaitingReply: (peer: string) => `Waiting for a reply from ${peer}`,
  unreadMessages: (n: number, peer: string) => `${n} unread ${pluralEn(n, "message", "messages")} from ${peer}`,
  undelivered: (peer: string) => `Message not delivered to ${peer}`,
  contextExceeded: (n: number) => (n > 1 ? `Context exceeded ${n} times` : "Context exceeded"),

  // Demo caption, outside Tauri
  mockCaption: "Demo mode outside Tauri, fictional data. Scenarios: ",
  mockFull: "full",
  mockWide: "wide window",
  mockEmpty: "empty",
  mockHealth: "sources down",
};

const fr: typeof en = {
  // Window
  windowTitle: "Sessions Claude",
  sessionsRegion: "Sessions",
  compactTitle: "Barre compacte",
  compactLabel: "Passer en barre compacte",
  pinLabel: "Toujours au premier plan",
  pinOn: "Toujours au premier plan (activé)",
  pinOff: "Toujours au premier plan (désactivé)",
  hideTitle: "Masquer (reste dans la zone de notification)",
  hideLabel: "Masquer la fenêtre",
  expandTitle: "Agrandir",
  expandLabel: "Afficher la liste des sessions",
  windowTooShort: "Fenêtre trop basse : tire son bord inférieur pour l'agrandir",

  // Loading and failures
  connecting: "Connexion…",
  reading: "Lecture des sessions…",
  engineUnreachable: "Moteur injoignable",
  unknownError: "erreur inconnue",
  commandFailed: "Commande échouée",
  commandFailures: {
    [CMD.openSession]: "Impossible d'ouvrir la session",
    [CMD.openUrl]: "Impossible d'ouvrir la PR",
    [CMD.setCompact]: "Impossible de changer de mode",
    [CMD.setAlwaysOnTop]: "Impossible de changer l'épinglage",
    [CMD.hideWindow]: "Impossible de masquer la fenêtre",
  },
  stateUnreadable: (why) => `Impossible de lire l'état des sessions : ${why}`,
  settingsUnreadable: (why) => `Réglages illisibles : ${why}`,
  startFailed: (why) => `Initialisation impossible : ${why}`,
  liveUnavailable: (why) => `Mises à jour en direct indisponibles (${why}), actualisation toutes les 2 s`,

  // Health line
  registryMissing: "Registre des sessions introuvable",
  desktopUnreadable: "Fichiers de Claude Desktop illisibles",
  ghDown: "gh indisponible : statut des PR en pause",

  // Fallbacks for what the backend leaves empty
  untitledSession: "Session sans titre",
  aSession: "une session",
  anotherSession: "une autre session",
  unknownSession: "session inconnue",
  unnamedTask: "tâche sans nom",
  noProject: "Sans projet",

  // Times
  seconds: (n) => `${n} s`,
  minutes: (n) => `${n} min`,
  hours: (n) => `${n} h`,
  days: (n) => `${n} j`,
  clock: clockFr,
  when: (m) => (m.date ? `le ${m.date} à ${m.time}` : `à ${m.time}`),

  // Text put together from pieces that are already worded
  labeled: (label, text) => `${label} : ${text}`,
  percent: (n) => `${n} %`,

  // States
  states: {
    waiting: "En attente",
    error: "En erreur",
    unseen: "Terminée, non vue",
    working: "Au travail",
    idle: "Inactive",
  },
  statesShort: {
    waiting: "attend",
    error: "erreur",
    unseen: "non vue",
    working: "en cours",
    idle: "",
  },
  stateProposed: "proposée",
  stateSince: (state, clock) => `${state} depuis ${clock}`,
  reasons: {
    "permission prompt": "Permission demandée",
    "input needed": "Question en attente",
    "sandbox request": "Accès sandbox demandé",
    "worker request": "Requête d'un worker",
    "dialog open": "Boîte de dialogue ouverte",
    decision: "Décision attendue",
    "probable question": "Question dans le dernier message",
    offer: "Choix demandé",
    "plan approval": "Plan à approuver",
  },

  // Summary
  waitingCount: (n) => `${n} ${pluralFr(n, "t'attend", "t'attendent")}`,
  errorCount: (n) => `${n} en erreur`,
  workingCount: (n) => `${n} au travail`,
  unseenCount: (n) => `${n} ${pluralFr(n, "non vue", "non vues")}`,
  noSessions: "Aucune session active",
  idleSessions: (n) => `${n} ${pluralFr(n, "session inactive", "sessions inactives")}`,
  prCount: (n) => `${n} PR`,
  proposedTasks: (n) => `${n} ${pluralFr(n, "tâche proposée", "tâches proposées")}`,
  emptyHint: "Les sessions de l'onglet Code de Claude Desktop apparaîtront ici.",

  // Compact bar and screen readers
  clickToShowList: "Cliquer pour afficher la liste",
  problems: (texts) =>
    `${pluralFr(texts.length, "Problème", "Problèmes")} : ${texts.join(" ; ")}. Afficher la liste`,
  othersWaiting: (n) => `${n} ${pluralFr(n, "autre session t'attend", "autres sessions t'attendent")}`,
  suggestedAnswers: (answers) => `Réponses proposées : ${answers}`,
  openTitled: (title, what) => `Ouvrir « ${title} » : ${what}`,
  terminalTitled: (title, what) =>
    `« ${title} » : ${what}. Session de terminal, impossible de l'ouvrir dans Claude Desktop`,
  terminalNotice: (title) => `« ${title} » tourne dans un terminal : impossible de l'ouvrir dans Claude Desktop`,
  liveWaiting: (title, what) => `« ${title} » t'attend : ${what}`,
  liveTentative: (title, what) => `« ${title} » pose peut-être une question : ${what}`,
  liveUnseen: (n) => `${n} ${pluralFr(n, "session terminée non vue", "sessions terminées non vues")}`,

  // Quota
  gaugeFiveHours: "5 h",
  gaugeWeek: "7 j",
  windowFiveHours: "Fenêtre de 5 h",
  windowWeek: "Semaine",
  quotaGauge: (gauge) => `Quota ${gauge}`,
  percentUsed: (n) => `${n} % utilisés`,
  lastSample: "Dernier relevé",
  sampledByDesktop: "Relevé par Claude Desktop",
  quotaExpired: (n) => `réinitialisée depuis le dernier relevé (${n} % alors)`,
  quotaExpiredSpoken: "réinitialisée depuis le dernier relevé",
  resetsAt: (time) => `réinitialisation à ${time}`,
  resetsAround: (time, earliest, latest) => `réinitialisation vers ${time} (entre ${earliest} et ${latest})`,
  resetsDayAt: (day, time) => `réinitialisation ${day} à ${time}`,
  resetsDayBetween: (day, earliest, latest) => `réinitialisation ${day} entre ${earliest} et ${latest}`,
  resetsBetween: (firstDay, earliest, lastDay, latest) =>
    `réinitialisation entre ${firstDay} ${earliest} et ${lastDay} ${latest}`,
  resetUnknown: "Heure de réinitialisation inconnue",
  timeElapsed: (n, week) =>
    `Temps écoulé : ${n} % ${week ? "de la semaine" : "de la fenêtre"} (zone teintée derrière la barre)`,
  quotaStale: (since) => `Pas de relevé depuis ${since} : clic droit sur l'icône de Claude Desktop pour le relancer`,
  quotaResumeHint:
    "Claude Desktop ne relève le quota qu'à son ouverture et pendant les 5 h qui suivent un clic droit sur son icône, dans la zone de notification. Un clic droit sur cette icône relance le relevé.",

  // Nested sessions and folded rows
  nestedSessions: (n) => `${n} ${pluralFr(n, "session imbriquée", "sessions imbriquées")}`,
  nestedWorking: (n) => `${n} en cours`,
  foldAction: (open, what) => `${open ? "Replier" : "Afficher"} ${what}`,
  foldHint: (open, nested) =>
    `Clic : ${open ? "replier" : "afficher"} ${nested ? "les sessions imbriquées" : "ces tâches"}`,
  collapseNested: "Replier les sessions imbriquées",
  expandNested: "Déplier les sessions imbriquées",

  // Proposed tasks
  proposedTask: (title) => `Tâche proposée : ${title}`,
  proposedBy: (session) => `Proposée par ${session}`,
  proposedByAt: (session, clock) => `Proposée par ${session}, ${clock}`,
  proposedAt: (clock) => `Proposée ${clock}`,
  taskPassedOver: "Repliée : tu as écrit deux fois à cette session depuis",
  taskExpires: (when) => `Desktop l'écarte ${when} si elle n'est pas lancée`,
  taskListedUntil: (when) => `Masquée ici ${when} ; Desktop la garde jusqu'à ce que tu la lances ou l'écartes`,
  taskHint: "Clic : ouvrir la session dans Claude Desktop, pour lancer ou écarter la tâche",
  taskCardName: (title, session) =>
    `Tâche proposée : ${title}, par ${session}. Ouvrir la session dans Claude Desktop`,
  hideTaskHint: "Pas pour moi : la masquer ici. Elle reste dans sa session, dans Claude Desktop",
  hideTaskLabel: (title) => `Masquer ici la tâche « ${title} »`,
  taskHidden: "Tâche masquée ici : elle reste dans sa session, dans Claude Desktop",

  // Cards
  cliTooltip: "Session lancée dans un terminal : elle ne peut pas être ouverte dans Claude Desktop",
  onScreenTooltip: "Affichée dans Claude Desktop, au premier plan",
  stoppedTooltip: "En pause : Claude Desktop a arrêté son processus. Elle reprend dès que tu lui écris.",
  errorFallback: "Le dernier tour s'est terminé sur une erreur",
  shownInDesktop: "Affichée dans Claude Desktop",
  inProject: (project) => `projet ${project}`,
  onBranch: (branch) => `branche ${branch}`,
  dirtyFiles: (n) => `${n} ${pluralFr(n, "fichier modifié non commité", "fichiers modifiés non commités")}`,
  planProgress: (done, total, current) =>
    `Plan : ${done} sur ${total}${current ? `, en cours : ${current}` : ""}`,
  startedFrom: (parent) => `Lancée depuis ${parent}`,
  requestLine: (text) => `Demande : ${text}`,
  errorLine: (text) => `Erreur : ${text}`,
  branchLine: (branch) => `Branche : ${branch}`,
  worktreeLine: (worktree) => `Worktree : ${worktree}`,
  recapLine: (text) => `Résumé : ${text}`,
  activityLine: (text) => `Dernière activité : ${text}`,
  subAgents: (n, names) => `${n} ${pluralFr(n, "sous-agent", "sous-agents")} : ${names}`,

  // Code review
  findingsOpen: (open, count) => `Revue : ${resultsFr(open)} sur ${count} encore à traiter`,
  findingsAllConfirmed: (count) =>
    count > 1 ? `Revue : ${resultsFr(count)}, tous confirmés` : "Revue : 1 résultat confirmé",
  findings: (count, confirmed) => {
    const among = confirmed > 0 ? ` dont ${confirmed} ${pluralFr(confirmed, "confirmé", "confirmés")}` : "";
    return `Revue : ${resultsFr(count)}${among}`;
  },

  // What a session waits for after its turn
  background: {
    command: "Attend une commande",
    agent: "Attend un agent",
    workflow: "Attend un workflow",
    monitor: "Surveille",
  },
  waitingForSession: "Attend une session",
  waitingForSessions: (n) => `Attend ${n} sessions`,
  waitingForTask: "Attend une tâche",
  waitingForTasks: (n) => `Attend ${n} tâches`,
  resumesAt: (at) => `Reprend à ${clockFr(at)}`,
  willResume: "Reprendra",

  // Pull requests
  checks: {
    pass: "réussis",
    fail: "en échec",
    pending: "en cours",
    none: "aucun",
  },
  checksUnknown: "inconnus",
  checksPhrases: {
    pass: "checks réussis",
    fail: "checks en échec",
    pending: "checks en cours",
    none: "aucun check",
  },
  checksPhraseUnknown: "checks inconnus",
  reviews: {
    APPROVED: "approuvée",
    CHANGES_REQUESTED: "modifications demandées",
    REVIEW_REQUIRED: "en attente de revue",
  },
  merges: {
    CLEAN: "prête à fusionner",
    BLOCKED: "bloquée",
    DIRTY: "conflits à résoudre",
    BEHIND: "en retard sur la branche de base",
    UNSTABLE: "checks facultatifs en échec",
    HAS_HOOKS: "prête (hooks)",
    DRAFT: "brouillon",
    UNKNOWN: "état inconnu",
  },
  prMerged: "fusionnée",
  prClosed: "fermée",
  prDraftStatus: (checks) => `brouillon, ${checks}`,
  prOpenStatus: (checks) => `ouverte, ${checks}`,
  prOpen: "Ouverte",
  prOpenDraft: "Ouverte · brouillon",
  prChecks: (status) => `Checks : ${status}`,
  prReview: (status) => `Revue : ${status}`,
  prMerge: (status) => `Fusion : ${status}`,
  prOpenLabel: (number, status) => `Ouvrir la PR #${number} (${status})`,
  autoFixOn: "Auto-fix actif",
  autoFixHandling: "Auto-fix s'en charge",
  autoFixTooltip:
    "Auto-fix : Claude Desktop relance la session quand la CI échoue, en cas de conflit ou de commentaire de revue",
  autoFixWorking: "La session y travaille : Auto-fix s'en charge",

  // Messages between sessions and warnings
  awaitingReply: (peer) => `Attend une réponse de ${peer}`,
  unreadMessages: (n, peer) => `${n} ${pluralFr(n, "message non lu", "messages non lus")} de ${peer}`,
  undelivered: (peer) => `Message non livré à ${peer}`,
  contextExceeded: (n) => (n > 1 ? `Contexte dépassé ${n} fois` : "Contexte dépassé"),

  // Demo caption, outside Tauri
  mockCaption: "Mode démo hors Tauri, données fictives. Scénarios : ",
  mockFull: "complet",
  mockWide: "fenêtre large",
  mockEmpty: "vide",
  mockHealth: "sources en panne",
};

/** The messages in the current language. Ask each time: a switch changes them. */
export const t = (): typeof en => (lang === "fr" ? fr : en);
