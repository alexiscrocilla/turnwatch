// In-browser stand-in for the Tauri backend, used when the page runs outside
// Tauri (`pnpm dev` in a plain browser). All data is fictional.

import { CMD, EVENT_SETTINGS, EVENT_SNAPSHOT } from "./types";
import type {
  Counts,
  GitInfo,
  MessageSummary,
  PeerLink,
  PendingQuestion,
  PlanProgress,
  PrInfo,
  ProposedTask,
  Quota,
  SessionState,
  SessionView,
  Settings,
  Snapshot,
  SourceHealth,
  ToolActivity,
} from "./types";

/** Fake backend with the same command and event contract as the Tauri shell. */
export interface MockBackend {
  /** Runs a command; resolves with its result like Tauri's `invoke`. */
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
  /** Subscribes to a backend event; returns the unsubscribe function. */
  listen(event: string, handler: (payload: unknown) => void): () => void;
}

/** Delay between two scripted changes of the demo. */
const STEP_MS = 5_000;
/** One tool per kind of thing a character holds, the last one holding nothing new. */
const TOOLS: ReadonlyArray<readonly [name: string, summary: string]> = [
  ["Edit", "src/settings/SettingsForm.ts"],
  ["Bash", "pnpm test --filter web"],
  ["Grep", '"useSettings" src/ --type ts'],
  ["WebFetch", "https://exemple.org/docs/formulaires"],
  ["Agent", "Audit du formulaire"],
  ["update_plan", ""],
];
/** Simulated IPC latency. */
const LATENCY_MS = 30;
/** The engine's first pass: until then snapshots are not ready. */
const READY_MS = 1_000;

const K = {
  auth: "local_mock-0001-auth",
  cache: "local_mock-0002-cache",
  pdf: "local_mock-0003-pdf",
  dates: "local_mock-0004-dates",
  settings: "local_mock-0005-settings",
  search: "local_mock-0006-search",
  deps: "local_mock-0007-deps",
  backup: "cli:mock-0008-backup",
  audit: "local_mock-0009-audit",
  lot: "local_mock-0010-lot",
  grill: "local_mock-0011-grill",
  alias: "local_mock-0012-alias",
  offer: "local_mock-0013-offer",
  planned: "local_mock-0014-planned",
  contrast: "local_mock-0015-contrast",
} as const;

const TITLE_SETTINGS = "Refonte de la page Paramètres";
const TITLE_SEARCH = "Pagination des résultats de recherche";
const TITLE_DATES = "Dates relatives figées dans la liste";

const AUTH_PLAN = [
  "Ajouter le fournisseur OAuth",
  "Écrire la migration de la table users",
  "Adapter le middleware de session",
  "Mettre à jour les tests",
  "Documenter la configuration",
];
const DATES_PLAN = [
  "Reproduire le gel des dates",
  "Partager un minuteur unique",
  "Couvrir le passage à minuit",
  "Relancer la suite de tests",
];
const SETTINGS_PLAN = [
  "Lister les réglages existants",
  "Créer le schéma du formulaire",
  "Découper la page en sections",
  "Brancher le formulaire sur l'API",
  "Gérer les erreurs de validation",
  "Écrire les tests",
  "Vérifier le thème sombre",
];
const DEPS_PLAN = [
  "Mettre à jour les dépendances",
  "Corriger les avertissements",
  "Ouvrir la PR",
];
const LOT_PLAN = [
  "Lire le ticket #6",
  "Regrouper les corrections par module",
  "Écrire les tests de non-régression",
  "Préparer le lot sur feat/6-lot",
  "Relecture adverse en 5 axes",
  "Corriger les points relevés",
  "Ouvrir la PR",
];

const MOCK_SETTINGS: Settings = {
  notifyOnWaiting: true,
  notifyOnFinished: false,
  notifyOnError: true,
  notifyOnQuota: true,
  includeCliSessions: true,
  prPollSecs: 60,
  messageWindowHours: 6,
  compact: false,
  alwaysOnTop: true,
};

const STATE_RANK: Record<SessionState, number> = {
  waiting: 0,
  error: 1,
  unseen: 2,
  working: 3,
  idle: 4,
};

const ago = (seconds: number): number => Date.now() - seconds * 1000;

const act = (name: string, summary: string, secondsAgo = 2): ToolActivity => ({
  name,
  summary,
  at: ago(secondsAgo),
});

/** A question asked at the end of a turn. */
const ask = (text: string, options: string[], source: PendingQuestion["source"]): PendingQuestion => ({
  text,
  options,
  source,
  at: Date.now(),
});

const noMessages = (): MessageSummary => ({
  awaitingReply: [],
  undelivered: [],
  unreadInbound: [],
});

const okHealth = (): SourceHealth => ({
  registryOk: true,
  desktopOk: true,
  ghOk: true,
  errors: [],
});

const brokenHealth = (): SourceHealth => ({
  registryOk: false,
  desktopOk: false,
  ghOk: false,
  errors: ["Lecture de local_mock-0003-pdf.json interrompue : fichier en cours d'écriture"],
});

/** What the engine returns before its first pass (Snapshot::default()). */
const notReadySnapshot = (): Snapshot => ({
  generatedAt: 0,
  ready: false,
  seq: 0,
  sessions: [],
  counts: countSessions([]),
  health: { registryOk: false, desktopOk: false, ghOk: null, errors: [] },
  quota: null,
  proposedTasks: [],
});

/** "New task" cards: two under the sessions that proposed them, one alone in its project. */
const mockTasks = (): ProposedTask[] => [
  {
    id: "task_0a1b2c3d",
    title: "Tester le passage à l'heure d'hiver",
    tldr: "Le minuteur partagé n'a pas de test pour le changement d'heure : un cas suffirait.",
    createdAt: ago(25 * 60),
    expiresAt: Date.now() + 6 * 24 * HOUR_MS,
    listedUntil: Date.now() + 6 * 24 * HOUR_MS,
    sessionKey: K.dates,
    sessionTitle: TITLE_DATES,
    project: "carnet",
    passedOver: false,
  },
  {
    id: "task_8c9d0e1f",
    title: "Parcourir la page Paramètres au clavier",
    tldr: "Les nouvelles sections n'ont pas été vérifiées à la tabulation.",
    createdAt: ago(8 * 60),
    expiresAt: Date.now() + 7 * 24 * HOUR_MS,
    listedUntil: Date.now() + 7 * 24 * HOUR_MS,
    sessionKey: K.settings,
    sessionTitle: TITLE_SETTINGS,
    project: "atlas-web",
    passedOver: false,
  },
  // A second card of the family: it waits behind a row.
  {
    id: "task_2d3e4f5a",
    title: "Aligner les libellés du formulaire",
    tldr: "Les sections n'utilisent pas toutes la même largeur de libellé.",
    createdAt: ago(40 * 60),
    expiresAt: Date.now() + 7 * 24 * HOUR_MS,
    listedUntil: Date.now() + 7 * 24 * HOUR_MS,
    sessionKey: K.settings,
    sessionTitle: TITLE_SETTINGS,
    project: "atlas-web",
    passedOver: false,
  },
  // The person wrote to the session twice since: folded away, without asking for attention.
  {
    id: "task_6b7c8d9e",
    title: "Supprimer l'ancien écran de préférences",
    tldr: null,
    createdAt: ago(2 * 3600),
    expiresAt: null,
    listedUntil: ago(2 * 3600) + 7 * 24 * HOUR_MS,
    sessionKey: K.settings,
    sessionTitle: TITLE_SETTINGS,
    project: "atlas-web",
    passedOver: true,
  },
  {
    id: "task_4e5f6a7b",
    title: "Nettoyer les captures d'écran de la doc",
    tldr: "Douze captures ne correspondent plus à l'interface actuelle.",
    createdAt: ago(3 * 3600),
    expiresAt: null,
    // An older card: Desktop never expires it, the monitor lists it 7 days.
    listedUntil: ago(3 * 3600) + 7 * 24 * HOUR_MS,
    sessionKey: "local_mock-0099-docs",
    sessionTitle: "Mettre à jour la documentation",
    project: "guide-utilisateur",
    passedOver: false,
  },
];

const HOUR_MS = 3_600_000;

/** A 5-hour window well used (amber), a week barely started. */
const mockQuota = (): Quota => ({
  sampledAt: ago(4 * 60),
  fiveHour: {
    percent: 82,
    lengthMs: 5 * HOUR_MS,
    resetsAt: Date.now() + 72 * 60_000,
    resetMarginMs: 7 * 60_000,
    endsBy: Date.now() + 79 * 60_000,
    expired: false,
  },
  sevenDay: {
    percent: 28,
    lengthMs: 7 * 24 * HOUR_MS,
    resetsAt: Date.now() + 6 * 24 * HOUR_MS,
    resetMarginMs: 150 * 60_000,
    endsBy: Date.now() + 6 * 24 * HOUR_MS + 150 * 60_000,
    expired: false,
  },
});

/** Desktop stopped sampling 8 hours ago: the 5-hour window is over, the week goes on. */
const pausedQuota = (): Quota => ({
  sampledAt: ago(8 * 3600),
  fiveHour: {
    percent: 0,
    lengthMs: 5 * HOUR_MS,
    resetsAt: null,
    resetMarginMs: 0,
    endsBy: Date.now() - 3 * HOUR_MS,
    expired: true,
  },
  sevenDay: {
    percent: 53,
    lengthMs: 7 * 24 * HOUR_MS,
    resetsAt: Date.now() + 2 * 24 * HOUR_MS,
    resetMarginMs: 150 * 60_000,
    endsBy: Date.now() + 2 * 24 * HOUR_MS + 150 * 60_000,
    expired: false,
  },
});

function link(peerKey: string, peerTitle: string, sentSecondsAgo: number, delivered: boolean): PeerLink {
  const sentAt = ago(sentSecondsAgo);
  return {
    peerHostSessionId: peerKey,
    peerTitle,
    sentAt,
    deliveredAt: delivered ? sentAt + 400 : null,
    readAt: null,
  };
}

/**
 * A plan with `done` steps completed and the next one in progress. As in
 * Desktop, a wrap_up ticks nothing off: `wrappedUp` is only information.
 */
function plan(steps: readonly string[], done: number, wrappedUp = false): PlanProgress {
  const items = steps.map((content, i) => ({
    content,
    status: i < done ? "completed" : i === done ? "in_progress" : "pending",
  }));
  return {
    items,
    done,
    total: steps.length,
    current: steps[done] ?? null,
    updatedAt: Date.now(),
    wrappedUp,
  };
}

function git(project: string, branch: string, dirtyFiles: number, isDefaultBranch = false): GitInfo {
  return {
    branch,
    repoRoot: `D:\\Développement\\${project}`,
    dirtyFiles,
    isDefaultBranch,
    checkedAt: Date.now(),
  };
}

function pr(repository: string, number: number, title: string, patch: Partial<PrInfo>): PrInfo {
  return {
    number,
    url: `https://github.com/${repository}/pull/${number}`,
    title,
    repository,
    state: "OPEN",
    isDraft: false,
    reviewDecision: null,
    checks: "none",
    mergeState: null,
    checkedAt: Date.now(),
    source: "desktop",
    autoFix: false,
    ...patch,
  };
}

type Seed = Pick<SessionView, "key" | "title" | "project" | "state"> & Partial<SessionView>;

let nextPid = 41_200;

function session(seed: Seed): SessionView {
  const desktop = seed.key.startsWith("local_");
  nextPid += 12;
  return {
    hostSessionId: desktop ? seed.key : null,
    cliSessionId: `mock-cli-${seed.key.slice(-6)}`,
    pid: nextPid,
    isDesktop: desktop,
    stopped: false,
    cwd: `D:\\Développement\\${seed.project}`,
    worktree: null,
    waitingFor: null,
    question: null,
    stateSince: Date.now(),
    onScreen: false,
    model: "claude-opus-5-5",
    effort: "high",
    permissionMode: "default",
    completedTurns: 4,
    lastActivityAt: Date.now(),
    recap: null,
    activity: null,
    plan: null,
    git: null,
    pr: null,
    messages: noMessages(),
    parent: null,
    nestUnder: null,
    contextExceededCount: null,
    lastError: null,
    version: "2.1.308",
    findings: null,
    ...seed,
  };
}

/**
 * The demo's starting point: several projects, one session per state and per
 * UI feature (question, probable question, plan, open PR with failing checks,
 * an idle-only project).
 */
function initialSessions(): SessionView[] {
  return [
    session({
      key: K.auth,
      title: "Migrer l'authentification vers OAuth",
      project: "atlas-api",
      state: "waiting",
      waitingFor: "permission prompt",
      stateSince: ago(95),
      activity: act("Bash", "pnpm prisma migrate dev --name oauth", 95),
      plan: plan(AUTH_PLAN, 1),
      git: git("atlas-api", "feat/oauth", 4),
    }),
    // Turn ended with wrap_up { ask, actions }: the registry says idle, Desktop says waiting.
    session({
      key: K.lot,
      title: "Implémenter #6 : préparer un lot de corrections",
      project: "facturo",
      state: "waiting",
      waitingFor: "decision",
      question: ask("Pousser `feat/6-lot` et ouvrir la PR ?", ["Pousser et ouvrir", "Pas encore"], "wrap_up"),
      stateSince: ago(70),
      onScreen: true,
      recap: "Lot prêt sur feat/6-lot : 9 corrections, tests au vert.",
      activity: act("wrap_up", "Lot prêt, **9 corrections**", 70),
      plan: plan(LOT_PLAN, 4, true),
      git: git("facturo", "feat/6-lot", 0),
    }),
    session({
      key: K.cache,
      title: "Choisir la stratégie de cache",
      project: "atlas-api",
      state: "waiting",
      waitingFor: "input needed",
      stateSince: ago(40),
      activity: act("AskUserQuestion", "Redis partagé ou cache mémoire par instance ?", 40),
      git: git("atlas-api", "main", 0, true),
    }),
    // Only the wording of the last message says it is a question.
    session({
      key: K.grill,
      title: "Grill with docs",
      project: "carnet",
      state: "waiting",
      waitingFor: "probable question",
      question: ask("Je garde l'ancienne API en parallèle pendant la migration ?", [], "text"),
      stateSince: ago(25),
      git: git("carnet", "docs/grill", 1),
    }),
    // The person asked for a side task: the session waits for "New session" or "Do it here".
    session({
      key: K.offer,
      title: "Revoir les relances de paiement",
      project: "facturo",
      state: "waiting",
      waitingFor: "offer",
      question: ask(
        "Nouvelle session pour « Relances : gérer les cartes refusées », ou ici ?",
        ["Nouvelle session", "Ici"],
        "offer",
      ),
      stateSince: ago(90),
    }),
    session({
      key: K.pdf,
      title: "Corriger l'export PDF des factures",
      project: "facturo",
      state: "error",
      stateSince: ago(6 * 60),
      lastError: "API Error: 529 overloaded_error — le tour s'est arrêté avant la fin",
      contextExceededCount: 2,
      activity: act("Bash", "cargo test -p export", 6 * 60),
      git: git("facturo", "fix/pdf-export", 2),
      // Desktop's Auto-fix watches the PR, but this session failed: nothing handles the red CI now.
      pr: pr("exemple/facturo", 16, "Export PDF : polices embarquées", {
        checks: "fail",
        reviewDecision: "CHANGES_REQUESTED",
        mergeState: "BLOCKED",
        autoFix: true,
      }),
    }),
    session({
      key: K.dates,
      title: TITLE_DATES,
      project: "carnet",
      state: "unseen",
      stateSince: ago(4 * 60),
      // Desktop stopped its process since.
      stopped: true,
      pid: 0,
      // Raw Markdown, as the transcript has it: the UI shows it as plain text.
      recap:
        "Les dates relatives se recalculent chaque minute grâce à un minuteur partagé, comme dans " +
        "[Implémenter #6](#local_6093b050-3490-4d38-b251-c87d2aa1d597). Tests ajoutés dans " +
        "`list.test.ts` ; **rien n'est commité**.",
      plan: plan(DATES_PLAN, 4, true),
      git: git("carnet", "fix/relative-dates", 3),
      // The turn ended on a code review.
      findings: { count: 13, confirmed: 5, open: 13, at: ago(4 * 60) },
    }),
    // Plan mode: the plan waits for approval.
    session({
      key: K.planned,
      title: "Miniatures servies par le CDN",
      project: "atlas-web",
      state: "waiting",
      waitingFor: "plan approval",
      stateSince: ago(40),
      activity: act("ExitPlanMode", "Servir les miniatures depuis le CDN", 40),
    }),
    session({
      key: K.settings,
      title: TITLE_SETTINGS,
      project: "atlas-web",
      state: "working",
      stateSince: ago(12 * 60),
      onScreen: true,
      activity: act("Bash", "pnpm test --filter web", 3),
      plan: plan(SETTINGS_PLAN, 3),
      git: git("atlas-web", "feat/settings-page", 6),
      messages: { ...noMessages(), awaitingReply: [link(K.search, TITLE_SEARCH, 5 * 60, true)] },
    }),
    // Side sessions started by the Paramètres session: nested under it.
    session({
      key: K.search,
      title: TITLE_SEARCH,
      project: "atlas-web",
      cwd: "D:\\Développement\\atlas-web\\.claude\\worktrees\\eager-lovelace-7f3a",
      worktree: "eager-lovelace-7f3a",
      state: "working",
      stateSince: ago(3 * 60),
      activity: act("Grep", '"<Pagination" src/ --type ts', 1),
      git: git("atlas-web", "claude/search-pagination", 0),
      pr: pr("exemple/atlas-web", 128, "Pagination des résultats de recherche", {
        checks: "pending",
        reviewDecision: "REVIEW_REQUIRED",
        mergeState: "BLOCKED",
      }),
      parent: { hostSessionId: K.settings, title: TITLE_SETTINGS },
      nestUnder: K.settings,
      messages: { ...noMessages(), unreadInbound: [link(K.settings, TITLE_SETTINGS, 5 * 60, true)] },
    }),
    session({
      key: K.contrast,
      title: "Contraste du thème sombre",
      project: "atlas-web",
      state: "idle",
      stateSince: ago(50 * 60),
      recap: "Contrastes relevés à 4,5:1 sur les boutons secondaires ; rien d'autre à corriger.",
      git: git("atlas-web", "feat/settings-page", 0),
      parent: { hostSessionId: K.settings, title: TITLE_SETTINGS },
      nestUnder: K.settings,
    }),
    session({
      key: K.deps,
      title: "Mise à jour des dépendances",
      project: "facturo",
      state: "idle",
      stateSince: ago(25 * 60),
      recap: "Toutes les dépendances sont à jour et la PR #57 est fusionnée. Aucun changement cassant.",
      plan: plan(DEPS_PLAN, 3, true),
      git: git("facturo", "main", 0, true),
      pr: pr("exemple/facturo", 57, "Mise à jour des dépendances d'octobre", {
        state: "MERGED",
        checks: "pass",
        reviewDecision: "APPROVED",
        mergeState: "CLEAN",
      }),
      messages: { ...noMessages(), undelivered: [link(K.audit, "Audit d'accessibilité", 22 * 60, false)] },
    }),
    // Started in a terminal: listed, but Desktop cannot open it.
    session({
      key: K.backup,
      title: "Script de sauvegarde nocturne",
      project: "dotfiles",
      state: "idle",
      stateSince: ago(2 * 3600 + 5 * 60),
      recap: "Le script tourne en local, mais la CI échoue sur shellcheck (SC2086 : variables non citées).",
      git: git("dotfiles", "feat/backup", 1),
      pr: pr("exemple/dotfiles", 9, "Sauvegarde nocturne avec restic", {
        isDraft: true,
        checks: "fail",
        mergeState: "DRAFT",
        source: "gh-branch",
      }),
    }),
    // Second idle session of an idle-only project.
    session({
      key: K.alias,
      title: "Alias git pour les worktrees",
      project: "dotfiles",
      state: "idle",
      stateSince: ago(26 * 3600),
      recap: "Alias `gwt` et `gwtr` ajoutés au .gitconfig, documentés dans le README.",
      git: git("dotfiles", "main", 0, true),
    }),
  ];
}

/** Backend order: waiting (longest first), error, unseen, working, idle (most recent first). */
function sortSessions(list: readonly SessionView[]): SessionView[] {
  return [...list].sort((a, b) => {
    const rank = STATE_RANK[a.state] - STATE_RANK[b.state];
    if (rank !== 0) return rank;
    const sa = a.stateSince ?? 0;
    const sb = b.stateSince ?? 0;
    return a.state === "waiting" ? sa - sb : sb - sa;
  });
}

function countSessions(list: readonly SessionView[]): Counts {
  const counts: Counts = {
    total: list.length,
    waiting: 0,
    error: 0,
    unseen: 0,
    working: 0,
    idle: 0,
    prOpen: 0,
    prFailing: 0,
    messagesPending: 0,
  };
  for (const s of list) {
    counts[s.state] += 1;
    if (s.pr?.state === "OPEN") {
      counts.prOpen += 1;
      if (s.pr.checks === "fail") counts.prFailing += 1;
    }
    counts.messagesPending += s.messages.awaitingReply.length;
  }
  return counts;
}

/**
 * Creates the fake backend. Every scenario starts not ready for READY_MS,
 * like the engine before its first pass. `?scenario=empty` then has no
 * session, `?scenario=health` every source failing, `?scenario=paused` a
 * quota Desktop stopped sampling.
 */
export function createMockBackend(): MockBackend {
  const scenario = new URLSearchParams(window.location.search).get("scenario");
  const listeners = new Map<string, Set<(payload: unknown) => void>>();
  let settings: Settings = { ...MOCK_SETTINGS };
  let sessions: SessionView[] = scenario === "empty" ? [] : initialSessions();
  let health: SourceHealth = scenario === "health" ? brokenHealth() : okHealth();
  const quota: Quota | null = scenario === "empty" ? null : scenario === "paused" ? pausedQuota() : mockQuota();
  const tasks: ProposedTask[] = scenario === "empty" ? [] : mockTasks();
  let ready = false;
  /** Increases with every published snapshot, like the engine's. */
  let seq = 0;
  /** Simulated wall-clock error: the UI must order by seq, not generatedAt. */
  let clockSkewMs = 0;
  let step = 0;

  const emit = (event: string, payload: unknown): void => {
    for (const handler of listeners.get(event) ?? []) {
      const copy = structuredClone(payload);
      window.setTimeout(() => handler(copy), LATENCY_MS);
    }
  };

  const snapshot = (): Snapshot =>
    ready
      ? {
          generatedAt: Date.now() + clockSkewMs,
          ready: true,
          seq,
          sessions: sortSessions(sessions),
          counts: countSessions(sessions),
          health,
          quota,
          proposedTasks: tasks,
        }
      : notReadySnapshot();

  const publish = (): void => {
    if (!ready) return;
    seq += 1;
    emit(EVENT_SNAPSHOT, snapshot());
  };

  const find = (key: string): SessionView | undefined => sessions.find((s) => s.key === key);

  /** Moves a session to a state; `stateSince` restarts only on a real change. */
  const move = (key: string, state: SessionState, patch: Partial<SessionView> = {}): void => {
    const s = find(key);
    if (!s) return;
    const now = Date.now();
    if (s.state !== state) s.stateSince = now;
    Object.assign(s, { state, waitingFor: null, question: null, lastActivityAt: now }, patch);
  };

  /** One scripted change every STEP_MS, then the demo starts over. */
  const steps: ReadonlyArray<() => void> = [
    () => {
      const s = find(K.settings);
      if (!s) return;
      s.plan = plan(SETTINGS_PLAN, 4);
      s.activity = act("Edit", "src/settings/SettingsForm.ts", 1);
    },
    () => move(K.auth, "working", { activity: act("Bash", "pnpm prisma migrate dev --name oauth", 1) }),
    // The user answered "Pousser et ouvrir" in Desktop.
    () =>
      move(K.lot, "working", {
        activity: act("Bash", "git push -u origin feat/6-lot", 1),
        plan: plan(LOT_PLAN, 5),
      }),
    () => {
      const s = find(K.search);
      if (!s?.pr) return;
      Object.assign(s.pr, { checks: "pass", reviewDecision: "APPROVED", mergeState: "CLEAN", checkedAt: Date.now() });
    },
    () => move(K.cache, "working", { activity: act("Read", "src/cache/mod.rs", 1) }),
    () => {
      move(K.settings, "unseen", {
        plan: plan(SETTINGS_PLAN, SETTINGS_PLAN.length, true),
        activity: act("wrap_up", "Page Paramètres terminée", 1),
        recap: "Page Paramètres refaite : formulaire relié à l'API et **12 tests au vert**. Rien n'est commité.",
        messages: noMessages(),
      });
      const search = find(K.search);
      if (search) search.messages = noMessages();
    },
    // Ends its turn on a decision, while shown in Desktop.
    () =>
      move(K.search, "waiting", {
        waitingFor: "decision",
        question: ask("Je fusionne la PR #128 dans main ?", ["Fusionner", "Attendre la revue"], "wrap_up"),
        activity: act("wrap_up", "Checks au vert, revue approuvée", 1),
        onScreen: true,
      }),
    () => move(K.auth, "waiting", { waitingFor: "sandbox request" }),
    () => {
      if (scenario === "health") return;
      health = { registryOk: true, desktopOk: true, ghOk: false, errors: ["gh pr list : délai dépassé (10 s)"] };
    },
    () => {
      if (scenario !== "health") health = okHealth();
      move(K.dates, "idle");
      // The wall clock jumps back 90 s: seq still increases, the UI keeps up.
      clockSkewMs = -90_000;
    },
    () => move(K.pdf, "working", { lastError: null, activity: act("Bash", "cargo test -p export", 1) }),
    () => move(K.grill, "working", { activity: act("Edit", "docs/api.md", 1) }),
    () => {
      sessions = initialSessions();
      clockSkewMs = 0;
    },
  ];

  window.setTimeout(() => {
    ready = true;
    publish();
  }, READY_MS);

  /** Working sessions go on calling tools: their characters step and change hands. */
  const walk = (): void => {
    const [name, summary] = TOOLS[step % TOOLS.length];
    for (const s of sessions) {
      if (s.state === "working") s.activity = act(name, summary, 1);
    }
  };

  if (scenario !== "empty") {
    window.setInterval(() => {
      steps[step % steps.length]();
      walk();
      step += 1;
      publish();
    }, STEP_MS);
  }

  const handle = (cmd: string, args: Record<string, unknown>): unknown => {
    console.info(`[mock] ${cmd}`, args);
    switch (cmd) {
      case CMD.getSnapshot:
        return snapshot();
      case CMD.getSettings:
        return settings;
      case CMD.setSettings:
        settings = { ...settings, ...(args.settings as Partial<Settings> | undefined) };
        emit(EVENT_SETTINGS, settings);
        return settings;
      case CMD.setCompact:
        settings = { ...settings, compact: args.compact === true };
        emit(EVENT_SETTINGS, settings);
        return settings;
      case CMD.setAlwaysOnTop:
        settings = { ...settings, alwaysOnTop: args.value === true };
        emit(EVENT_SETTINGS, settings);
        return settings;
      case CMD.openSession:
      case CMD.ackSession: {
        const key = String(args.key);
        if (!key.startsWith("local_")) throw new Error("Session hors de Claude Desktop");
        const s = find(key);
        if (s?.state === "unseen") {
          move(s.key, "idle");
          publish();
        }
        return null;
      }
      case CMD.openUrl:
      case CMD.hideWindow:
      case CMD.quitApp:
        return null;
      default:
        throw new Error(`Commande inconnue : ${cmd}`);
    }
  };

  return {
    invoke<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
      return new Promise<T>((resolve, reject) => {
        try {
          const result = structuredClone(handle(cmd, args)) as T;
          window.setTimeout(() => resolve(result), LATENCY_MS);
        } catch (err) {
          reject(err instanceof Error ? err : new Error(String(err)));
        }
      });
    },
    listen(event, handler) {
      const set = listeners.get(event) ?? new Set<(payload: unknown) => void>();
      listeners.set(event, set);
      set.add(handler);
      return () => {
        set.delete(handler);
      };
    },
  };
}
