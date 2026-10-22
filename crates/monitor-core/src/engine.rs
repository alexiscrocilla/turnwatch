//! Watches the sources, rebuilds the snapshot when something changes and
//! hands it to the window.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{Map, Value};

use crate::aggregate::{self, AggregateInput};
use crate::desktop::DesktopIndex;
use crate::git_pr::{GitPrRequest, GitPrService};
use crate::model::{
    DesktopSession, GitInfo, PrInfo, RegistryEntry, SessionState, Settings, Snapshot,
    SourceHealth, TranscriptInfo, Transition,
};
use crate::paths;
use crate::registry;
use crate::transcript::TranscriptTracker;

pub type SnapshotSink = Box<dyn Fn(&Snapshot, &[Transition]) + Send + 'static>;

/// Recompute at least this often, even without file events (liveness, timers).
const TICK: Duration = Duration::from_secs(2);
/// Quiet time after a file event before recomputing, so bursts coalesce.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Delay between two attempts to watch a directory that could not be watched.
const WATCH_RETRY: Duration = Duration::from_secs(10);
/// Claude Desktop must stay in front this long before what it shows counts
/// as seen: a window that only flashes by (alt-tab, a deep link landing)
/// credits nothing.
const FOREGROUND_SETTLE: Duration = Duration::from_millis(1000);
/// After the user asks Desktop to show a session (card or toast click), no
/// other session counts as seen until Desktop shows that one, or this long.
const OPEN_GRACE: Duration = Duration::from_secs(5);
/// Shortest wait between two passes scheduled by the worker itself.
const MIN_SELF_WAKE: Duration = Duration::from_millis(50);
/// Times written by the CLI may be this far ahead of ours.
const CLOCK_SLACK_MS: i64 = 5_000;
/// File holding the acks in the state folder.
pub const ACKS_FILE: &str = "acks.json";
/// Acks are written to disk at most this often.
const ACKS_WRITE_INTERVAL: Duration = Duration::from_secs(5);
/// Most acks kept on disk (the most recent win).
const MAX_STORED_ACKS: usize = 2000;
/// Key of the pane layout in Desktop's `epitaxyPrefs`.
const PANE_STORE_KEY: &str = "desktop-frame.paneStore.v1";

/// Cheap to clone; every clone drives the same engine.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

/// Owned by every `Engine` clone; the worker only holds `shared` and its own
/// senders, so dropping the last clone drops this and stops the worker.
struct Inner {
    shared: Arc<Mutex<Shared>>,
    tx: Sender<Msg>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

/// State read and written from outside the worker.
#[derive(Default)]
struct Shared {
    settings: Settings,
    /// Session key -> time the user acknowledged it.
    acks: HashMap<String, i64>,
    last: Option<Snapshot>,
    /// Session the user just asked Desktop to show, and until when the
    /// worker waits for it. Taken by the worker.
    open_request: Option<(String, Instant)>,
    /// Where the acks are saved; None without a state folder.
    ack_store: Option<AckStore>,
}

impl Shared {
    /// Writes the acks if they changed: at once when `now`, else at most
    /// every [`ACKS_WRITE_INTERVAL`].
    fn save_acks(&mut self, now: bool) {
        let Shared { acks, ack_store, .. } = self;
        if let Some(store) = ack_store {
            store.save(acks, now);
        }
    }
}

/// What wakes the worker up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Msg {
    /// A watched file changed, or the PR service has new results.
    Dirty,
    /// Recompute now (ack, settings change).
    Refresh,
    /// Recompute now and ask the PR service to refresh every session.
    RefreshAll,
    Stop,
}

impl Engine {
    /// Starts the watchers and the worker thread. `sink` is called with each
    /// new snapshot (only when it differs from the previous one).
    pub fn start(settings: Settings, sink: SnapshotSink) -> Engine {
        Self::start_with_state(settings, None, sink)
    }

    /// Like [`Engine::start`], keeping the engine's own state (acks) in
    /// `state_dir` so it survives a restart of the app.
    pub fn start_with_state(
        settings: Settings,
        state_dir: Option<PathBuf>,
        sink: SnapshotSink,
    ) -> Engine {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared {
            settings,
            ack_store: state_dir.map(AckStore::new),
            ..Shared::default()
        }));
        let worker_shared = Arc::clone(&shared);
        let worker_tx = tx.clone();
        let spawned = thread::Builder::new()
            .name("monitor-engine".into())
            .spawn(move || run_worker(worker_shared, worker_tx, rx, sink));
        if let Err(e) = spawned {
            // Nothing more will come: say so instead of "not ready yet".
            let mut snapshot = Snapshot {
                generated_at: paths::now_ms(),
                ready: true,
                seq: 1,
                ..Snapshot::default()
            };
            snapshot
                .health
                .errors
                .push(format!("Impossible de démarrer le moteur : {e}"));
            lock(&shared).last = Some(snapshot);
        }
        Engine {
            inner: Arc::new(Inner { shared, tx }),
        }
    }

    /// Latest snapshot. Before the first pass: an empty one with
    /// `ready == false`.
    pub fn snapshot(&self) -> Snapshot {
        lock(&self.inner.shared).last.clone().unwrap_or_default()
    }

    /// Marks a session as seen now. The ack is saved at once: the user may
    /// quit right after.
    pub fn ack(&self, key: &str) {
        {
            let mut shared = lock(&self.inner.shared);
            shared.acks.insert(key.to_owned(), paths::now_ms());
            shared.save_acks(true);
        }
        self.send(Msg::Refresh);
    }

    /// Saves the acks now if they changed since the last write. Call it
    /// before the app exits: the process may end before the worker stops.
    pub fn flush(&self) {
        lock(&self.inner.shared).save_acks(true);
    }

    pub fn settings(&self) -> Settings {
        lock(&self.inner.shared).settings.clone()
    }

    pub fn set_settings(&self, settings: Settings) {
        lock(&self.inner.shared).settings = settings;
        self.send(Msg::Refresh);
    }

    /// Tells the engine the user just asked Desktop to show `key`, so auto-ack
    /// does not credit the session Desktop was showing before. Call it before
    /// asking Desktop.
    pub fn note_open(&self, key: &str) {
        lock(&self.inner.shared).open_request = Some((key.to_owned(), Instant::now() + OPEN_GRACE));
    }

    pub fn refresh_now(&self) {
        self.send(Msg::RefreshAll);
    }

    /// Saves the acks, then stops the worker and the watchers without
    /// waiting for them. Later calls on any clone keep returning the last
    /// snapshot.
    pub fn shutdown(&self) {
        self.flush();
        self.send(Msg::Stop);
    }

    /// Builds one snapshot synchronously, without watchers, PR lookups or
    /// saved state.
    pub fn snapshot_once(settings: &Settings) -> Snapshot {
        let paths = Paths::standard();
        let mut readers = Readers::default();
        let mut errors = Vec::new();
        let now = paths::now_ms();
        let pass = readers.read(&paths, settings, &mut errors);
        let empty = BuildState {
            git_pr: &HashMap::new(),
            acks: &HashMap::new(),
            on_screen: &HashSet::new(),
            turn_ended_at: &HashMap::new(),
        };
        let (mut snapshot, _) = build(&pass, &empty, settings, None, errors, now, None);
        snapshot.ready = true;
        snapshot.seq = 1;
        snapshot
    }

    fn send(&self, msg: Msg) {
        let _ = self.inner.tx.send(msg);
    }
}

/// Locks a mutex, recovering the data if a previous holder panicked.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// Files and directories the engine reads.
struct Paths {
    sessions: PathBuf,
    desktop_root: PathBuf,
    projects: PathBuf,
    desktop_config: PathBuf,
}

impl Paths {
    fn standard() -> Self {
        Self {
            sessions: paths::sessions_dir(),
            desktop_root: paths::desktop_sessions_root(),
            projects: paths::projects_dir(),
            desktop_config: paths::desktop_config_file(),
        }
    }
}

/// Tells whether a changed path can change the snapshot.
type EventFilter = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// One watched directory.
struct WatchSlot {
    path: PathBuf,
    mode: RecursiveMode,
    filter: EventFilter,
    watcher: Option<RecommendedWatcher>,
    last_attempt: Option<Instant>,
    error: Option<String>,
}

impl WatchSlot {
    fn new(path: PathBuf, mode: RecursiveMode, filter: EventFilter) -> Self {
        Self {
            path,
            mode,
            filter,
            watcher: None,
            last_attempt: None,
            error: None,
        }
    }

    /// Starts watching when not watching yet (at most every `WATCH_RETRY`),
    /// and drops the watcher of a directory that disappeared so it is watched
    /// again once recreated.
    fn maintain(&mut self, tx: &Sender<Msg>) {
        if self.watcher.is_some() {
            if !self.path.is_dir() {
                self.watcher = None;
                self.last_attempt = None;
                self.error = Some(missing_dir(&self.path));
            }
            return;
        }
        if self
            .last_attempt
            .is_some_and(|at| at.elapsed() < WATCH_RETRY)
        {
            return;
        }
        self.last_attempt = Some(Instant::now());
        if !self.path.is_dir() {
            self.error = Some(missing_dir(&self.path));
            return;
        }
        let sender = tx.clone();
        let filter = Arc::clone(&self.filter);
        let handler = move |event: notify::Result<notify::Event>| {
            if let Ok(event) = &event
                && !wakes_up(event, filter.as_ref())
            {
                return;
            }
            let _ = sender.send(Msg::Dirty);
        };
        let watched = notify::recommended_watcher(handler).and_then(|mut watcher| {
            watcher.watch(&self.path, self.mode)?;
            Ok(watcher)
        });
        match watched {
            Ok(watcher) => {
                self.watcher = Some(watcher);
                self.error = None;
            }
            Err(e) => {
                self.error = Some(format!(
                    "Surveillance impossible de {} : {e}",
                    self.path.display()
                ));
            }
        }
    }
}

/// True when a file event can change the snapshot. Reads never do (ignoring
/// them also keeps the monitor's own reads from waking it up); a rescan
/// request always does; otherwise one of its paths must pass `filter`.
fn wakes_up(event: &notify::Event, filter: &(dyn Fn(&Path) -> bool + Send + Sync)) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.need_rescan() || event.paths.iter().any(|path| filter(path))
}

/// Registry files: `<pid>.json`.
fn registry_event(path: &Path) -> bool {
    path.file_name().is_some_and(registry::is_registry_file_name)
}

/// Desktop session files: `local_<id>.json`.
fn desktop_event(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.len() > "local_.json".len() && name.starts_with("local_") && name.ends_with(".json")
        })
}

/// Transcript of a listed live session: `<root>\<project>\<session id>.jsonl`.
/// Subagent transcripts, tool results and other sessions are ignored.
fn transcript_event(root: &Path, path: &Path, live: &HashSet<String>) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    if relative.components().count() != 2 {
        return false;
    }
    let is_jsonl = relative
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));
    is_jsonl
        && relative
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|id| live.contains(id))
}

fn missing_dir(path: &Path) -> String {
    format!("Dossier introuvable : {}", path.display())
}

/// Everything the worker thread owns.
struct Worker {
    shared: Arc<Mutex<Shared>>,
    tx: Sender<Msg>,
    sink: SnapshotSink,
    paths: Paths,
    watches: Vec<WatchSlot>,
    readers: Readers,
    git_pr: Option<GitPrService>,
    git_pr_error: Option<String>,
    applied_poll_secs: u64,
    last_requests: Option<Vec<GitPrRequest>>,
    /// True once a snapshot built from a readable registry has been emitted.
    /// Until then transitions are not reported, so a startup or a recovery
    /// from a broken source does not raise a toast for every session.
    primed: bool,
    /// `seq` of the latest snapshot published.
    seq: u64,
    /// CLI ids of the listed live sessions: only their transcripts wake the
    /// worker up.
    live_ids: Arc<RwLock<HashSet<String>>>,
    turns: TurnTracker,
    screen: ScreenTracker,
    panes: PaneReader,
}

fn run_worker(
    shared: Arc<Mutex<Shared>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    sink: SnapshotSink,
) {
    let paths = Paths::standard();
    let live_ids: Arc<RwLock<HashSet<String>>> = Arc::default();
    let projects_root = paths.projects.clone();
    let listed = Arc::clone(&live_ids);
    let transcript_filter: EventFilter = Arc::new(move |path: &Path| {
        let live = listed.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        transcript_event(&projects_root, path, &live)
    });
    let watches = vec![
        WatchSlot::new(
            paths.sessions.clone(),
            RecursiveMode::NonRecursive,
            Arc::new(registry_event),
        ),
        WatchSlot::new(
            paths.desktop_root.clone(),
            RecursiveMode::Recursive,
            Arc::new(desktop_event),
        ),
        WatchSlot::new(
            paths.projects.clone(),
            RecursiveMode::Recursive,
            transcript_filter,
        ),
    ];
    let poll_secs = lock(&shared).settings.pr_poll_secs;
    let pr_tx = tx.clone();
    let on_change: crate::git_pr::ChangeCallback = Arc::new(move || {
        let _ = pr_tx.send(Msg::Dirty);
    });
    let (git_pr, git_pr_error) = match guarded(|| GitPrService::start(poll_secs, on_change)) {
        Ok(service) => (Some(service), None),
        Err(msg) => (None, Some(internal_error("Service Git/PR", &msg))),
    };
    let panes = PaneReader::new(paths.desktop_config.clone());
    let mut worker = Worker {
        shared,
        tx,
        sink,
        paths,
        watches,
        readers: Readers::default(),
        git_pr,
        git_pr_error,
        applied_poll_secs: poll_secs,
        last_requests: None,
        primed: false,
        seq: 0,
        live_ids,
        turns: TurnTracker::default(),
        screen: ScreenTracker::default(),
        panes,
    };

    worker.load_acks();
    worker.maintain_watches();
    worker.recompute(false);
    let mut next_run = worker.next_run();
    loop {
        let wait = next_run.saturating_duration_since(Instant::now());
        let mut wake = Wake::default();
        match rx.recv_timeout(wait) {
            Ok(msg) => {
                wake.add(msg);
                if msg == Msg::Dirty {
                    thread::sleep(DEBOUNCE);
                }
                wake.merge(drain(&rx));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => wake.stop = true,
        }
        if wake.stop {
            worker.flush_acks();
            return;
        }
        worker.maintain_watches();
        worker.recompute(wake.refresh_all);
        next_run = worker.next_run();
    }
}

/// What a batch of messages asks for.
#[derive(Debug, Default, PartialEq, Eq)]
struct Wake {
    stop: bool,
    refresh_all: bool,
}

impl Wake {
    fn add(&mut self, msg: Msg) {
        match msg {
            Msg::Stop => self.stop = true,
            Msg::RefreshAll => self.refresh_all = true,
            Msg::Dirty | Msg::Refresh => {}
        }
    }

    fn merge(&mut self, other: Wake) {
        self.stop |= other.stop;
        self.refresh_all |= other.refresh_all;
    }
}

/// Takes every message already queued, without waiting. A closed channel
/// counts as a stop request.
fn drain(rx: &Receiver<Msg>) -> Wake {
    let mut wake = Wake::default();
    loop {
        match rx.try_recv() {
            Ok(msg) => wake.add(msg),
            Err(TryRecvError::Empty) => return wake,
            Err(TryRecvError::Disconnected) => {
                wake.stop = true;
                return wake;
            }
        }
    }
}

impl Worker {
    fn maintain_watches(&mut self) {
        for slot in &mut self.watches {
            slot.maintain(&self.tx);
        }
    }

    /// When to run the next pass on its own: the tick, or sooner when the
    /// foreground settle delay ends.
    fn next_run(&self) -> Instant {
        let now = Instant::now();
        let tick = now + TICK;
        match self.screen.next_check(now) {
            Some(at) => at.max(now + MIN_SELF_WAKE).min(tick),
            None => tick,
        }
    }

    /// Adds the saved acks to the ones already given (the latest wins).
    fn load_acks(&mut self) {
        let mut shared = lock(&self.shared);
        let Some(store) = shared.ack_store.as_mut() else {
            return;
        };
        let loaded = store.load();
        for (key, at) in loaded {
            let slot = shared.acks.entry(key).or_insert(at);
            *slot = (*slot).max(at);
        }
    }

    /// Writes the acks now if they changed since the last write.
    fn flush_acks(&mut self) {
        lock(&self.shared).save_acks(true);
    }

    fn recompute(&mut self, refresh_all: bool) {
        let now = paths::now_ms();
        let settings = lock(&self.shared).settings.clone();
        let mut errors: Vec<String> = self
            .watches
            .iter()
            .filter_map(|slot| slot.error.clone())
            .collect();
        errors.extend(self.git_pr_error.clone());

        if let Some(service) = &self.git_pr
            && settings.pr_poll_secs != self.applied_poll_secs
        {
            let secs = settings.pr_poll_secs;
            if let Err(msg) = guarded(|| service.set_poll_secs(secs)) {
                errors.push(internal_error("Service Git/PR", &msg));
            }
            self.applied_poll_secs = secs;
        }

        let pass = self.readers.read(&self.paths, &settings, &mut errors);
        {
            let mut live = self
                .live_ids
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *live != pass.listed_ids {
                *live = pass.listed_ids.clone();
            }
        }

        if pass.registry_ok {
            self.turns.update(chosen_entries(&pass), now);
        }

        let (git_pr, gh_ok) = match &self.git_pr {
            Some(service) => {
                let requests = git_pr_requests(&pass, &settings);
                if self.last_requests.as_ref() != Some(&requests) {
                    let tracked = requests.clone();
                    if let Err(msg) = guarded(|| service.track(tracked)) {
                        errors.push(internal_error("Service Git/PR", &msg));
                    }
                    self.last_requests = Some(requests);
                }
                if refresh_all {
                    for request in self.last_requests.iter().flatten() {
                        if let Err(msg) = guarded(|| service.bump(&request.key)) {
                            errors.push(internal_error("Service Git/PR", &msg));
                            break;
                        }
                    }
                }
                let results = guarded(|| service.results()).unwrap_or_else(|msg| {
                    errors.push(internal_error("Service Git/PR", &msg));
                    HashMap::new()
                });
                let gh_ok = guarded(|| service.gh_ok()).unwrap_or_else(|msg| {
                    errors.push(internal_error("Service Git/PR", &msg));
                    None
                });
                (results, gh_ok)
            }
            None => (HashMap::new(), None),
        };

        let on_screen = screen_keys(&mut self.screen, &mut self.panes, &self.shared, &pass);
        let acks = {
            let mut shared = lock(&self.shared);
            for key in &on_screen {
                let latest = latest_activity(pass.desktop.get(key), self.turns.ended_at.get(key));
                credit(&mut shared.acks, key, latest, now);
            }
            // A source that could not be read would forget saved acks.
            if pass.registry_ok && pass.desktop_ok {
                let keep: HashSet<&str> = pass
                    .keys
                    .iter()
                    .map(String::as_str)
                    .chain(pass.desktop.keys().map(String::as_str))
                    .collect();
                prune_acks(&mut shared.acks, &keep);
            }
            shared.save_acks(false);
            errors.extend(shared.ack_store.as_ref().and_then(|s| s.error.clone()));
            shared.acks.clone()
        };

        let prev = lock(&self.shared).last.clone();
        let state = BuildState {
            git_pr: &git_pr,
            acks: &acks,
            on_screen: &on_screen,
            turn_ended_at: &self.turns.ended_at,
        };
        let (mut next, built) = build(&pass, &state, &settings, gh_ok, errors, now, prev.as_ref());
        next.ready = true;
        if prev.as_ref().is_some_and(|p| same_content(p, &next)) {
            return;
        }
        self.seq += 1;
        next.seq = self.seq;
        let changes = match &prev {
            Some(p) if self.primed => {
                guarded(|| aggregate::transitions(Some(p), &next)).unwrap_or_default()
            }
            _ => Vec::new(),
        };
        lock(&self.shared).last = Some(next.clone());
        self.primed |= built && pass.registry_ok;

        if let Some(service) = &self.git_pr {
            for change in changes
                .iter()
                .filter(|c| c.from == Some(SessionState::Working))
            {
                let _ = guarded(|| service.bump(&change.key));
            }
        }
        let sink = &self.sink;
        let _ = guarded(|| sink(&next, &changes));
    }
}

/// Keys of the live sessions the user can see in Claude Desktop now.
fn screen_keys(
    screen: &mut ScreenTracker,
    panes: &mut PaneReader,
    shared: &Mutex<Shared>,
    pass: &Pass<'_>,
) -> HashSet<String> {
    if let Some((key, deadline)) = lock(shared).open_request.take() {
        screen.note_open(key, deadline);
    }
    let foreground = desktop_app_in_foreground();
    let visible = if foreground {
        let layout = guarded(|| panes.read().map(<[String]>::to_vec)).unwrap_or(None);
        visible_keys(layout.as_deref(), pass)
    } else {
        Vec::new()
    };
    screen.update(Instant::now(), foreground, &visible)
}

/// What the aggregation needs besides the sources.
struct BuildState<'a> {
    git_pr: &'a HashMap<String, (Option<GitInfo>, Option<PrInfo>)>,
    acks: &'a HashMap<String, i64>,
    on_screen: &'a HashSet<String>,
    turn_ended_at: &'a HashMap<String, i64>,
}

/// Builds the snapshot, falling back to the previous sessions with an error
/// in `health` when the aggregation panics. The flag is false on fallback.
fn build(
    pass: &Pass<'_>,
    state: &BuildState<'_>,
    settings: &Settings,
    gh_ok: Option<bool>,
    errors: Vec<String>,
    now: i64,
    prev: Option<&Snapshot>,
) -> (Snapshot, bool) {
    let health = SourceHealth {
        registry_ok: pass.registry_ok,
        desktop_ok: pass.desktop_ok,
        gh_ok,
        errors: dedup(errors),
    };
    let input = AggregateInput {
        now_ms: now,
        registry: &pass.registry,
        desktop: pass.desktop,
        transcripts: &pass.transcripts,
        git_pr: state.git_pr,
        acks: state.acks,
        on_screen: state.on_screen,
        turn_ended_at: state.turn_ended_at,
        settings,
        health: health.clone(),
    };
    match guarded(|| aggregate::build_snapshot(&input)) {
        Ok(snapshot) => (snapshot, true),
        Err(msg) => {
            let mut snapshot = prev.cloned().unwrap_or_default();
            snapshot.generated_at = now;
            snapshot.health = health;
            snapshot
                .health
                .errors
                .push(internal_error("Agrégation", &msg));
            (snapshot, false)
        }
    }
}

// ---------------------------------------------------------------------------
// Reading the local sources
// ---------------------------------------------------------------------------

/// Readers whose caches survive between two recomputes. Built lazily so a
/// constructor that panics is retried on the next pass.
#[derive(Default)]
struct Readers {
    desktop: Option<DesktopIndex>,
    transcripts: Option<TranscriptTracker>,
    /// Latest registry read that succeeded, reused when a read fails so a
    /// transient error does not make every session vanish and reappear.
    last_registry: Vec<RegistryEntry>,
}

/// The local sources as read by one pass.
struct Pass<'a> {
    /// Live entries.
    registry: Vec<RegistryEntry>,
    registry_ok: bool,
    desktop: &'a HashMap<String, DesktopSession>,
    desktop_ok: bool,
    /// By CLI session id, for the listed sessions only.
    transcripts: HashMap<String, TranscriptInfo>,
    /// CLI ids of the listed sessions, whose transcripts are read.
    listed_ids: HashSet<String>,
    /// Session key of each live entry, same order as `registry`.
    keys: Vec<String>,
    /// Desktop id -> session key, for the live Desktop sessions.
    live_key_by_host: HashMap<String, String>,
}

impl Pass<'_> {
    fn desktop_of(&self, entry: &RegistryEntry) -> Option<&DesktopSession> {
        find_desktop(self.desktop, entry)
    }
}

/// Desktop session of a live entry: by its Desktop id, else by CLI id.
fn find_desktop<'d>(
    desktop: &'d HashMap<String, DesktopSession>,
    entry: &RegistryEntry,
) -> Option<&'d DesktopSession> {
    entry
        .host_session_id
        .as_ref()
        .and_then(|host| desktop.get(host))
        .or_else(|| {
            desktop
                .values()
                .find(|d| d.cli_session_id.as_deref() == Some(entry.session_id.as_str()))
        })
}

fn empty_desktop() -> &'static HashMap<String, DesktopSession> {
    static EMPTY: OnceLock<HashMap<String, DesktopSession>> = OnceLock::new();
    EMPTY.get_or_init(HashMap::new)
}

impl Readers {
    fn read<'a>(
        &'a mut self,
        paths: &Paths,
        settings: &Settings,
        errors: &mut Vec<String>,
    ) -> Pass<'a> {
        let (registry, registry_ok) = match guarded(|| registry::read_live(&paths.sessions)) {
            Ok(Ok(entries)) => {
                self.last_registry = entries.clone();
                (entries, true)
            }
            Ok(Err(e)) => {
                errors.push(format!("Registre : {e}"));
                (self.last_registry.clone(), false)
            }
            Err(msg) => {
                errors.push(internal_error("Registre", &msg));
                (self.last_registry.clone(), false)
            }
        };

        if self.desktop.is_none() {
            let root = paths.desktop_root.clone();
            match guarded(|| DesktopIndex::new(root)) {
                Ok(index) => self.desktop = Some(index),
                Err(msg) => errors.push(internal_error("Desktop", &msg)),
            }
        }
        let mut desktop_ok = false;
        let mut desktop = empty_desktop();
        if let Some(index) = &mut self.desktop {
            match guarded(|| index.refresh()) {
                Ok(Ok(_)) => desktop_ok = true,
                Ok(Err(e)) => errors.push(format!("Desktop : {e}")),
                Err(msg) => errors.push(internal_error("Desktop", &msg)),
            }
            let index: &'a DesktopIndex = index;
            match guarded(|| index.sessions()) {
                Ok(map) => desktop = map,
                Err(msg) => {
                    desktop_ok = false;
                    errors.push(internal_error("Desktop", &msg));
                }
            }
        }

        let keys: Vec<String> = registry.iter().map(key_of).collect();

        if self.transcripts.is_none() {
            let dir = paths.projects.clone();
            match guarded(|| TranscriptTracker::new(dir)) {
                Ok(tracker) => self.transcripts = Some(tracker),
                Err(msg) => errors.push(internal_error("Transcriptions", &msg)),
            }
        }
        let listed_ids: HashSet<String> = registry
            .iter()
            .filter(|e| is_listed(e, settings, find_desktop(desktop, e).is_some()))
            .map(|e| e.session_id.clone())
            .collect();
        let mut transcripts = HashMap::new();
        if let Some(tracker) = &mut self.transcripts {
            for id in &listed_ids {
                match guarded(|| tracker.update(id)) {
                    Ok(Some(info)) => {
                        transcripts.insert(id.clone(), info);
                    }
                    Ok(None) => {}
                    Err(msg) => errors.push(internal_error("Transcriptions", &msg)),
                }
            }
            if let Err(msg) = guarded(|| tracker.retain(&listed_ids)) {
                errors.push(internal_error("Transcriptions", &msg));
            }
        }

        let mut pass = Pass {
            registry,
            registry_ok,
            desktop,
            desktop_ok,
            transcripts,
            listed_ids,
            keys,
            live_key_by_host: HashMap::new(),
        };
        let live_key_by_host = pass
            .registry
            .iter()
            .zip(&pass.keys)
            .filter_map(|(entry, key)| {
                pass.desktop_of(entry)
                    .map(|d| (d.host_session_id.clone(), key.clone()))
            })
            .collect();
        pass.live_key_by_host = live_key_by_host;
        pass
    }
}

/// Session key of a registry entry; falls back to the documented rule when
/// `aggregate::session_key` fails.
fn key_of(entry: &RegistryEntry) -> String {
    guarded(|| aggregate::session_key(entry)).unwrap_or_else(|_| {
        match entry.host_session_id.as_deref().filter(|h| !h.is_empty()) {
            Some(host) => host.to_owned(),
            None => format!("cli:{}", entry.session_id),
        }
    })
}

/// True when the session can be shown with these settings, so its
/// transcript and PR are worth reading. `has_desktop_file` tells whether a
/// Desktop session file belongs to it.
fn is_listed(entry: &RegistryEntry, settings: &Settings, has_desktop_file: bool) -> bool {
    settings.include_cli_sessions
        || has_desktop_file
        || entry.host_session_id.as_deref().is_some_and(|h| !h.is_empty())
        || entry.entrypoint.as_deref() == Some("claude-desktop")
}

/// Rank of a live entry among those sharing its key: same rule as
/// `aggregate::entry_rank`.
fn rank_of(entry: &RegistryEntry, pass: &Pass<'_>) -> aggregate::EntryRank {
    guarded(|| aggregate::entry_rank(entry, pass.desktop)).unwrap_or_else(|_| {
        let linked = pass
            .desktop_of(entry)
            .is_some_and(|d| d.cli_session_id.as_deref() == Some(entry.session_id.as_str()));
        (linked, entry.started_at, entry.pid)
    })
}

/// The process shown for each session key (several processes can share a
/// key while Desktop restarts one).
fn chosen_entries<'p>(pass: &'p Pass<'_>) -> Vec<(&'p str, &'p RegistryEntry)> {
    let mut chosen: Vec<(&str, &RegistryEntry, aggregate::EntryRank)> = Vec::new();
    for (entry, key) in pass.registry.iter().zip(&pass.keys) {
        let rank = rank_of(entry, pass);
        match chosen.iter_mut().find(|(k, _, _)| *k == key.as_str()) {
            Some(slot) => {
                if rank > slot.2 {
                    *slot = (key.as_str(), entry, rank);
                }
            }
            None => chosen.push((key.as_str(), entry, rank)),
        }
    }
    chosen
        .into_iter()
        .map(|(key, entry, _)| (key, entry))
        .collect()
}

/// One request per listed live session key. When several processes share a
/// key (Desktop restarted one and the old one is still alive), the process
/// the snapshot shows wins: same rule as `aggregate::entry_rank`.
fn git_pr_requests(pass: &Pass<'_>, settings: &Settings) -> Vec<GitPrRequest> {
    let mut requests: Vec<GitPrRequest> = Vec::new();
    let mut chosen: HashMap<String, (usize, aggregate::EntryRank)> = HashMap::new();
    let candidates = pass
        .registry
        .iter()
        .zip(&pass.keys)
        .filter_map(|(entry, key)| {
            let desktop = pass.desktop_of(entry);
            is_listed(entry, settings, desktop.is_some()).then_some((entry, key, desktop))
        })
        .map(|(entry, key, desktop)| {
            let branch_hint = desktop
                .and_then(|d| d.branch.clone())
                .filter(|b| !b.is_empty())
                .or_else(|| {
                    pass.transcripts
                        .get(&entry.session_id)
                        .and_then(|t| t.git_branch.clone())
                });
            let rank = rank_of(entry, pass);
            let cwd = if entry.cwd.trim().is_empty() {
                desktop.and_then(|d| d.cwd.clone()).unwrap_or_default()
            } else {
                entry.cwd.clone()
            };
            let request = GitPrRequest {
                key: key.clone(),
                cwd,
                branch_hint,
                desktop_pr: desktop.and_then(|d| d.pr.clone()),
                turn_edge_at: desktop.and_then(|d| d.last_turn_edge_at),
                working: status_of(entry) == "busy",
            };
            (request, rank)
        });
    for (request, rank) in candidates {
        match chosen.get_mut(&request.key) {
            Some((i, best)) => {
                if rank > *best {
                    *best = rank;
                    requests[*i] = request;
                }
            }
            None => {
                chosen.insert(request.key.clone(), (requests.len(), rank));
                requests.push(request);
            }
        }
    }
    requests
}

fn status_of(entry: &RegistryEntry) -> String {
    entry
        .status
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// Turn ends
// ---------------------------------------------------------------------------

/// Notes when each session's process leaves "busy" (or a "waiting" prompt)
/// for good: the end of its turn, known 1-3 s before Desktop writes it.
#[derive(Default)]
struct TurnTracker {
    /// Key -> the process seen at the previous pass.
    last: HashMap<String, SeenProcess>,
    /// Key -> end of its latest turn seen by the engine.
    ended_at: HashMap<String, i64>,
}

impl TurnTracker {
    /// Takes the process shown for each live key. A turn ends when the same
    /// process (same CLI session, pid and start time) goes from busy or
    /// waiting to anything else; its time is the registry's `statusUpdatedAt`
    /// when plausible, else `now`. A new process ends nothing, even when
    /// Desktop restarted it under the same CLI session id: Desktop's own edge
    /// covers it.
    fn update<'a>(&mut self, shown: impl IntoIterator<Item = (&'a str, &'a RegistryEntry)>, now: i64) {
        let mut next = HashMap::new();
        for (key, entry) in shown {
            let seen = SeenProcess {
                session_id: entry.session_id.clone(),
                pid: entry.pid,
                proc_start: entry.proc_start.clone(),
                status: status_of(entry),
                status_at: entry.status_updated_at,
            };
            if let Some(before) = self.last.get(key)
                && before.same_process(&seen)
                && in_turn(&before.status)
                && !in_turn(&seen.status)
            {
                let at = entry
                    .status_updated_at
                    .filter(|at| before.status_at.is_none_or(|before| *at > before))
                    .filter(|at| *at <= now + CLOCK_SLACK_MS)
                    .unwrap_or(now);
                self.ended_at.insert(key.to_owned(), at);
            }
            next.insert(key.to_owned(), seen);
        }
        self.ended_at.retain(|key, _| next.contains_key(key));
        self.last = next;
    }
}

/// One registry process as the turn tracker saw it.
struct SeenProcess {
    session_id: String,
    pid: u32,
    proc_start: Option<String>,
    status: String,
    status_at: Option<i64>,
}

impl SeenProcess {
    /// Same CLI session in the same process (start times compared when both
    /// are known: a pid may be reused).
    fn same_process(&self, other: &SeenProcess) -> bool {
        self.session_id == other.session_id
            && self.pid == other.pid
            && match (&self.proc_start, &other.proc_start) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

fn in_turn(status: &str) -> bool {
    status == "busy" || status == "waiting"
}

// ---------------------------------------------------------------------------
// What the user sees in Claude Desktop
// ---------------------------------------------------------------------------

/// Decides which sessions are on screen: shown in Desktop's panes while its
/// window has been in front for [`FOREGROUND_SETTLE`], and not while Desktop
/// is still on its way to a session the user asked for.
#[derive(Default)]
struct ScreenTracker {
    foreground_since: Option<Instant>,
    /// Session the user asked Desktop to show, and until when to wait for it.
    pending_open: Option<(String, Instant)>,
}

impl ScreenTracker {
    /// The user asked Desktop to show `key`: credit nothing else until it is
    /// shown or `deadline` passes, and wait for Desktop to settle in front.
    fn note_open(&mut self, key: String, deadline: Instant) {
        self.pending_open = Some((key, deadline));
        self.foreground_since = None;
    }

    /// Keys on screen now, given whether Desktop is in front and the live
    /// keys it shows.
    fn update(&mut self, now: Instant, foreground: bool, visible: &[String]) -> HashSet<String> {
        if self
            .pending_open
            .as_ref()
            .is_some_and(|(_, deadline)| now >= *deadline)
        {
            self.pending_open = None;
        }
        if !foreground {
            self.foreground_since = None;
            return HashSet::new();
        }
        let since = *self.foreground_since.get_or_insert(now);
        if now.saturating_duration_since(since) < FOREGROUND_SETTLE {
            return HashSet::new();
        }
        if let Some((key, _)) = &self.pending_open {
            if !visible.contains(key) {
                return HashSet::new();
            }
            self.pending_open = None;
        }
        visible.iter().cloned().collect()
    }

    /// When a pass is needed to see the settle delay end.
    fn next_check(&self, now: Instant) -> Option<Instant> {
        let settled = self.foreground_since? + FOREGROUND_SETTLE;
        (settled > now).then_some(settled)
    }
}

/// Live keys Desktop shows: the sessions of its panes, from its config file.
/// When that file cannot be read, the session the user switched to last.
fn visible_keys(layout: Option<&[String]>, pass: &Pass<'_>) -> Vec<String> {
    match layout {
        Some(ids) => ids
            .iter()
            .filter_map(|id| pass.live_key_by_host.get(id).cloned())
            .collect(),
        None => on_screen_session(
            pass.desktop
                .values()
                .map(|d| (d.host_session_id.as_str(), d.last_focused_at)),
        )
        .and_then(|host| pass.live_key_by_host.get(host).cloned())
        .into_iter()
        .collect(),
    }
}

/// Sessions shown in the panes of Claude Desktop's window, from its config
/// file. Re-read only when the file's time or size changes.
struct PaneReader {
    path: PathBuf,
    stamp: Option<(SystemTime, u64)>,
    panes: Option<Vec<String>>,
}

impl PaneReader {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            stamp: None,
            panes: None,
        }
    }

    /// Desktop ids of the sessions in the panes; None when unknown (no file,
    /// or a file without a pane layout). A file that cannot be read or parsed
    /// for a moment (Desktop rewriting it) keeps the last layout read, and is
    /// read again on the next pass.
    fn read(&mut self) -> Option<&[String]> {
        let Some(stamp) = file_stamp(&self.path) else {
            self.stamp = None;
            self.panes = None;
            return None;
        };
        if self.stamp != Some(stamp) {
            let root = std::fs::read(&self.path)
                .ok()
                .and_then(|bytes| parse_json(&String::from_utf8_lossy(&bytes)));
            match root {
                Some(root) => {
                    self.panes = pane_store_of(&root);
                    self.stamp = Some(stamp);
                }
                None => self.stamp = None,
            }
        }
        self.panes.as_deref()
    }
}

fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len()))
}

/// Desktop ids of the sessions shown in Desktop's panes, from
/// `claude_desktop_config.json`:
/// `preferences.epitaxyPrefs["desktop-frame.paneStore.v1"].state`, its
/// `lastPrimaryCodeSession.id` then each `extraPanesByMode.code[].ref.id`.
/// Any level may be stored as a JSON string. None when the layout is not
/// there. The file does not tell which tab (Code, Chat) is shown, nor the
/// pop-out windows.
#[cfg(test)]
fn parse_pane_store(json: &str) -> Option<Vec<String>> {
    pane_store_of(&parse_json(json)?)
}

/// JSON text, a leading BOM accepted.
fn parse_json(json: &str) -> Option<Value> {
    serde_json::from_str(json.strip_prefix('\u{feff}').unwrap_or(json)).ok()
}

/// The pane layout of Desktop's parsed config file; see the format above.
fn pane_store_of(root: &Value) -> Option<Vec<String>> {
    let prefs = nested(root, "preferences")?;
    let epitaxy = nested(&prefs, "epitaxyPrefs")?;
    let store = nested(&epitaxy, PANE_STORE_KEY)?;
    let state = nested(&store, "state").unwrap_or(store);
    let primary = nested(&state, "lastPrimaryCodeSession");
    let extra = nested(&state, "extraPanesByMode").and_then(|modes| nested(&modes, "code"));
    if primary.is_none() && extra.is_none() {
        return None;
    }
    let mut ids: Vec<String> = Vec::new();
    let mut add = |id: Option<&Value>| {
        if let Some(id) = id.and_then(Value::as_str).map(str::trim)
            && !id.is_empty()
            && !ids.iter().any(|known| known == id)
        {
            ids.push(id.to_owned());
        }
    };
    if let Some(primary) = &primary {
        add(primary.get("id"));
    }
    if let Some(Value::Array(panes)) = &extra {
        for pane in panes {
            add(pane.get("ref").and_then(|r| r.get("id")).or_else(|| pane.get("id")));
        }
    }
    Some(ids)
}

/// `value[key]`, parsed when it is stored as a JSON string. None when
/// missing or null.
fn nested(value: &Value, key: &str) -> Option<Value> {
    match value.get(key)? {
        Value::Null => None,
        Value::String(text) => serde_json::from_str(text).ok(),
        other => Some(other.clone()),
    }
}

/// Latest moment something happened in a session that the user may have to
/// see: turn edge, prompt, activity, or the turn end the engine saw.
fn latest_activity(desktop: Option<&DesktopSession>, turn_ended_at: Option<&i64>) -> Option<i64> {
    let desktop_times = desktop.map(|d| {
        d.last_turn_edge_at
            .max(d.latest_user_frame_at)
            .max(d.last_activity_at)
    });
    desktop_times.flatten().max(turn_ended_at.copied())
}

/// Acknowledges an on-screen session up to now, only when something
/// happened since its last ack: the acks change (and are saved) only when it
/// matters.
fn credit(acks: &mut HashMap<String, i64>, key: &str, latest: Option<i64>, now: i64) {
    let Some(latest) = latest else {
        return;
    };
    if acks.get(key).is_none_or(|at| *at < latest) {
        acks.insert(key.to_owned(), now.max(latest));
    }
}

// ---------------------------------------------------------------------------
// Saved acks
// ---------------------------------------------------------------------------

/// Acks kept in a small JSON file, so a restart does not bring sessions
/// already seen back as "finished, not seen".
struct AckStore {
    path: PathBuf,
    /// What the file holds.
    written: HashMap<String, i64>,
    written_at: Option<Instant>,
    /// Why the latest write failed, until one succeeds.
    error: Option<String>,
}

impl AckStore {
    fn new(dir: PathBuf) -> Self {
        Self {
            path: dir.join(ACKS_FILE),
            written: HashMap::new(),
            written_at: None,
            error: None,
        }
    }

    /// Reads the file; a missing or damaged file gives no acks.
    fn load(&mut self) -> HashMap<String, i64> {
        let acks = std::fs::read(&self.path)
            .ok()
            .map(|bytes| parse_acks(&String::from_utf8_lossy(&bytes)))
            .unwrap_or_default();
        self.written = acks.clone();
        acks
    }

    /// Writes `acks` when they differ from the file, at most every
    /// [`ACKS_WRITE_INTERVAL`] unless `now` is set.
    fn save(&mut self, acks: &HashMap<String, i64>, now: bool) {
        let acks = capped(acks);
        if acks == self.written {
            return;
        }
        if !now
            && self
                .written_at
                .is_some_and(|at| at.elapsed() < ACKS_WRITE_INTERVAL)
        {
            return;
        }
        self.written_at = Some(Instant::now());
        match write_atomically(&self.path, &acks_json(&acks)) {
            Ok(()) => {
                self.written = acks;
                self.error = None;
            }
            Err(e) => {
                self.error = Some(format!("Impossible d'enregistrer les sessions vues : {e}"));
            }
        }
    }
}

/// At most [`MAX_STORED_ACKS`] acks, the most recent ones.
fn capped(acks: &HashMap<String, i64>) -> HashMap<String, i64> {
    if acks.len() <= MAX_STORED_ACKS {
        return acks.clone();
    }
    let mut sorted: Vec<(&String, &i64)> = acks.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    sorted
        .into_iter()
        .take(MAX_STORED_ACKS)
        .map(|(key, at)| (key.clone(), *at))
        .collect()
}

/// `{"version":1,"acks":{"<key>":<ms>,…}}`, keys sorted.
fn acks_json(acks: &HashMap<String, i64>) -> String {
    let mut keys: Vec<&String> = acks.keys().collect();
    keys.sort();
    let mut map = Map::new();
    for key in keys {
        map.insert(key.clone(), Value::from(acks[key]));
    }
    let mut root = Map::new();
    root.insert("version".into(), Value::from(1));
    root.insert("acks".into(), Value::Object(map));
    Value::Object(root).to_string()
}

/// Acks from the file's JSON. Entries that are not a non-empty key with a
/// positive integer time are skipped; anything else gives no acks.
fn parse_acks(json: &str) -> HashMap<String, i64> {
    let Ok(Value::Object(root)) =
        serde_json::from_str::<Value>(json.strip_prefix('\u{feff}').unwrap_or(json))
    else {
        return HashMap::new();
    };
    let Some(Value::Object(acks)) = root.get("acks") else {
        return HashMap::new();
    };
    let parsed: HashMap<String, i64> = acks
        .iter()
        .filter(|(key, _)| !key.trim().is_empty())
        .filter_map(|(key, at)| {
            let at = at.as_i64().or_else(|| at.as_f64().map(|f| f as i64))?;
            (at > 0).then(|| (key.clone(), at))
        })
        .collect();
    capped(&parsed)
}

/// Writes through a temporary file renamed over `path`, so a crash never
/// leaves a half-written file.
fn write_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

// ---------------------------------------------------------------------------
// Small pure helpers
// ---------------------------------------------------------------------------

/// Runs `f`, turning a panic into its message.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "panique".to_owned()
        }
    })
}

fn internal_error(source: &str, msg: &str) -> String {
    format!("{source} : erreur interne ({msg})")
}

/// Removes repeated messages, keeping the first occurrence of each.
fn dedup(errors: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    errors
        .into_iter()
        .filter(|e| seen.insert(e.clone()))
        .collect()
}

/// True when both snapshots show the same thing (`generated_at` and `seq` ignored).
fn same_content(a: &Snapshot, b: &Snapshot) -> bool {
    let Snapshot {
        generated_at: _,
        seq: _,
        ready,
        sessions,
        counts,
        health,
    } = a;
    *ready == b.ready && *sessions == b.sessions && *counts == b.counts && *health == b.health
}

/// Forgets acks of sessions that are neither live nor known to Desktop.
fn prune_acks(acks: &mut HashMap<String, i64>, keep: &HashSet<&str>) {
    acks.retain(|key, _| keep.contains(key.as_str()));
}

/// Desktop id of the session the user switched to last. Stands in for the
/// pane layout when Desktop's config file cannot be read. `sessions` yields
/// `(host_session_id, last_focused_at)` for every Desktop session.
fn on_screen_session<'a>(
    sessions: impl IntoIterator<Item = (&'a str, Option<i64>)>,
) -> Option<&'a str> {
    sessions
        .into_iter()
        .filter_map(|(host, at)| at.map(|at| (at, host)))
        .max()
        .map(|(_, host)| host)
}

/// True when `image` (full path of an executable) is the Claude Desktop app:
/// `claude.exe` from the Store package or the classic installer. CLI
/// processes share the file name but live elsewhere.
fn is_desktop_app_image(image: &str) -> bool {
    let lower = image.to_lowercase();
    let file_name = lower.rsplit(['\\', '/']).next().unwrap_or_default();
    file_name == "claude.exe"
        && (lower.contains("\\windowsapps\\claude_") || lower.contains("\\anthropicclaude\\"))
}

/// True when the foreground window is a main window of the Claude Desktop
/// app: visible, titled, unowned and not a tool window. Its dialogs, menus
/// and quick-entry popup do not show the Code panes.
#[cfg(windows)]
fn desktop_app_in_foreground() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GW_OWNER, GWL_EXSTYLE, GetForegroundWindow, GetWindow, GetWindowLongPtrW,
        GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
    };

    // SAFETY: plain Win32 calls with valid out-pointers; the process handle
    // is closed before returning and the buffer length is passed in `len`.
    unsafe {
        let window = GetForegroundWindow();
        if window.is_null() {
            return false;
        }
        let main_window = IsWindowVisible(window) != 0
            && GetWindowTextLengthW(window) > 0
            && GetWindow(window, GW_OWNER).is_null()
            && (GetWindowLongPtrW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW) == 0;
        if !main_window {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, &mut pid);
        if pid == 0 {
            return false;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(process);
        if ok == 0 {
            return false;
        }
        let len = (len as usize).min(buf.len());
        is_desktop_app_image(&String::from_utf16_lossy(&buf[..len]))
    }
}

#[cfg(not(windows))]
fn desktop_app_in_foreground() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Counts, SessionView};

    fn engine_with_channel() -> (Engine, Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        let engine = Engine {
            inner: Arc::new(Inner {
                shared: Arc::new(Mutex::new(Shared::default())),
                tx,
            }),
        };
        (engine, rx)
    }

    fn view(key: &str, state: SessionState) -> SessionView {
        SessionView {
            key: key.into(),
            title: key.into(),
            state,
            ..SessionView::default()
        }
    }

    /// Fresh scratch directory under the system temp folder, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "monitor-core-engine-{tag}-{}-{nanos} é",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn write(&self, name: &str, content: &str) {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(path, content).expect("write file");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    type Published = Arc<Mutex<Vec<(Snapshot, Vec<Transition>)>>>;

    /// A worker reading the sources of `root` (sessions, desktop, projects
    /// and config.json), without watchers nor git.
    fn test_worker(root: &Path, state_dir: Option<PathBuf>) -> (Worker, Published) {
        for dir in ["sessions", "desktop", "projects"] {
            std::fs::create_dir_all(root.join(dir)).expect("create source dir");
        }
        let (tx, _rx) = mpsc::channel();
        let published: Published = Arc::default();
        let store = Arc::clone(&published);
        let sink: SnapshotSink = Box::new(move |snapshot, changes| {
            lock(&store).push((snapshot.clone(), changes.to_vec()));
        });
        let paths = Paths {
            sessions: root.join("sessions"),
            desktop_root: root.join("desktop"),
            projects: root.join("projects"),
            desktop_config: root.join("config.json"),
        };
        let worker = Worker {
            shared: Arc::new(Mutex::new(Shared {
                ack_store: state_dir.map(AckStore::new),
                ..Shared::default()
            })),
            tx,
            sink,
            panes: PaneReader::new(paths.desktop_config.clone()),
            paths,
            watches: Vec::new(),
            readers: Readers::default(),
            git_pr: None,
            git_pr_error: None,
            applied_poll_secs: 60,
            last_requests: None,
            primed: false,
            seq: 0,
            live_ids: Arc::default(),
            turns: TurnTracker::default(),
            screen: ScreenTracker::default(),
        };
        (worker, published)
    }

    #[test]
    fn engine_is_clone_send_sync() {
        fn check<T: Clone + Send + Sync + 'static>() {}
        check::<Engine>();
    }

    #[test]
    fn last_clone_dropped_sends_stop_once() {
        let (engine, rx) = engine_with_channel();
        let other = engine.clone();
        drop(engine);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        drop(other);
        assert_eq!(rx.try_recv(), Ok(Msg::Stop));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn ack_records_time_and_wakes_worker() {
        let (engine, rx) = engine_with_channel();
        let before = paths::now_ms();
        engine.ack("local_a");
        let at = lock(&engine.inner.shared).acks.get("local_a").copied();
        assert!(at.is_some_and(|at| at >= before));
        assert_eq!(rx.try_recv(), Ok(Msg::Refresh));
    }

    #[test]
    fn explicit_acks_are_saved_at_once() {
        let dir = TempDir::new("ackflush");
        let state = dir.0.join("state");
        let (engine, _rx) = engine_with_channel();
        lock(&engine.inner.shared).ack_store = Some(AckStore::new(state.clone()));
        engine.ack("local_a");
        let saved = parse_acks(&std::fs::read_to_string(state.join(ACKS_FILE)).expect("saved"));
        assert!(saved.contains_key("local_a"));
        // A second click right after is saved too (no 5 s throttle).
        engine.ack("local_b");
        let saved = parse_acks(&std::fs::read_to_string(state.join(ACKS_FILE)).expect("saved"));
        assert!(saved.contains_key("local_b"));
        // An automatic credit is written by flush (called on quit).
        lock(&engine.inner.shared).acks.insert("local_c".into(), 1_792_000_000_000);
        engine.flush();
        let saved = parse_acks(&std::fs::read_to_string(state.join(ACKS_FILE)).expect("saved"));
        assert_eq!(saved.get("local_c"), Some(&1_792_000_000_000));
    }

    #[test]
    fn note_open_is_handed_to_the_worker() {
        let (engine, rx) = engine_with_channel();
        let before = Instant::now();
        engine.note_open("local_b");
        let request = lock(&engine.inner.shared).open_request.clone();
        let (key, deadline) = request.expect("open request");
        assert_eq!(key, "local_b");
        assert!(deadline >= before + OPEN_GRACE);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "the ack that follows wakes it");
    }

    #[test]
    fn settings_round_trip_and_refresh() {
        let (engine, rx) = engine_with_channel();
        let settings = Settings {
            pr_poll_secs: 15,
            include_cli_sessions: true,
            ..Settings::default()
        };
        engine.set_settings(settings.clone());
        assert_eq!(engine.settings(), settings);
        assert_eq!(rx.try_recv(), Ok(Msg::Refresh));
        engine.refresh_now();
        assert_eq!(rx.try_recv(), Ok(Msg::RefreshAll));
    }

    #[test]
    fn snapshot_is_not_ready_until_first_compute() {
        let (engine, _rx) = engine_with_channel();
        let first = engine.snapshot();
        assert_eq!(first, Snapshot::default());
        assert!(!first.ready, "an empty list before the first pass means 'not read yet'");
        assert_eq!(first.seq, 0);
        let stored = Snapshot {
            generated_at: 42,
            ready: true,
            seq: 1,
            ..Snapshot::default()
        };
        lock(&engine.inner.shared).last = Some(stored.clone());
        assert_eq!(engine.snapshot(), stored);
    }

    #[test]
    fn published_snapshots_are_ready_and_numbered() {
        let dir = TempDir::new("seq");
        let (mut worker, published) = test_worker(&dir.0, None);
        worker.recompute(false);
        worker.recompute(false);
        {
            let published = lock(&published);
            assert_eq!(published.len(), 1, "an unchanged pass publishes nothing");
            assert!(published[0].0.ready);
            assert_eq!(published[0].0.seq, 1);
            assert!(published[0].0.health.registry_ok);
        }
        assert_eq!(worker.seq, 1);
        assert!(lock(&worker.shared).last.as_ref().is_some_and(|s| s.seq == 1));

        // A source going missing changes the health: a new snapshot.
        std::fs::remove_dir_all(dir.0.join("sessions")).expect("remove sessions");
        worker.recompute(false);
        let published = lock(&published);
        assert_eq!(published.len(), 2);
        assert_eq!(published[1].0.seq, 2);
        assert!(!published[1].0.health.registry_ok);
    }

    #[test]
    fn snapshot_once_is_ready() {
        let snapshot = Engine::snapshot_once(&Settings::default());
        assert!(snapshot.ready);
        assert_eq!(snapshot.seq, 1);
    }

    #[test]
    fn acks_survive_a_restart() {
        let dir = TempDir::new("acks");
        dir.write(
            "desktop/account/org/local_a.json",
            r#"{"sessionId":"local_a","title":"A"}"#,
        );
        let state = dir.0.join("state");
        let (mut worker, _) = test_worker(&dir.0, Some(state.clone()));
        worker.load_acks();
        {
            let mut shared = lock(&worker.shared);
            shared.acks.insert("local_a".into(), 1_792_000_000_000);
            shared.acks.insert("local_gone".into(), 1_792_000_000_001);
        }
        worker.recompute(false);
        let saved = std::fs::read_to_string(state.join(ACKS_FILE)).expect("acks saved");
        let parsed = parse_acks(&saved);
        assert_eq!(parsed.get("local_a"), Some(&1_792_000_000_000));
        assert!(!parsed.contains_key("local_gone"), "pruned: neither live nor in Desktop");

        let (mut restarted, _) = test_worker(&dir.0, Some(state));
        restarted.load_acks();
        assert_eq!(
            lock(&restarted.shared).acks.get("local_a"),
            Some(&1_792_000_000_000)
        );
    }

    /// The registry goes idle seconds before Desktop writes the end of the
    /// turn: the session must go from Working straight to Unseen.
    #[test]
    fn turn_end_goes_straight_to_unseen() {
        let dir = TempDir::new("turnend");
        let now = paths::now_ms();
        let started = now - 60_000;
        let ended = now - 30_000;
        dir.write(
            "desktop/account/org/local_a.json",
            &serde_json::json!({
                "sessionId": "local_a",
                "cliSessionId": "s1",
                "title": "A",
                "lastTurnEdgeAt": started,
                "latestUserFrameAt": started,
                "lastFocusedAt": started - 1_000,
                "completedTurns": 1
            })
            .to_string(),
        );
        let pid = std::process::id();
        let registry = |status: &str, at: i64| {
            serde_json::json!({
                "pid": pid,
                "sessionId": "s1",
                "hostSessionId": "local_a",
                "entrypoint": "claude-desktop",
                "status": status,
                "statusUpdatedAt": at
            })
            .to_string()
        };
        dir.write(&format!("sessions/{pid}.json"), &registry("busy", started));
        let (mut worker, published) = test_worker(&dir.0, None);
        // Seen during the turn, after it started.
        lock(&worker.shared).acks.insert("local_a".into(), started + 500);
        worker.recompute(false);
        dir.write(&format!("sessions/{pid}.json"), &registry("idle", ended));
        worker.recompute(false);

        assert_eq!(worker.turns.ended_at.get("local_a"), Some(&ended));
        let published = lock(&published);
        assert_eq!(published.len(), 2);
        assert_eq!(published[0].0.sessions[0].state, SessionState::Working);
        assert_eq!(published[1].0.sessions[0].state, SessionState::Unseen);
        let change = &published[1].1[0];
        assert_eq!(
            (change.from, change.to),
            (Some(SessionState::Working), SessionState::Unseen)
        );
    }

    #[test]
    fn acks_are_not_pruned_when_desktop_cannot_be_read() {
        let dir = TempDir::new("noprune");
        let (mut worker, _) = test_worker(&dir.0, None);
        std::fs::remove_dir_all(dir.0.join("desktop")).expect("remove desktop");
        lock(&worker.shared).acks.insert("local_a".into(), 5);
        worker.recompute(false);
        assert_eq!(lock(&worker.shared).acks.get("local_a"), Some(&5));
    }

    #[test]
    fn ack_store_writes_only_changes_and_not_too_often() {
        let dir = TempDir::new("store");
        let mut store = AckStore::new(dir.0.join("nested").join("state"));
        assert!(store.load().is_empty(), "no file yet");
        let mut acks: HashMap<String, i64> = [("local_a".to_owned(), 10)].into();
        store.save(&acks, false);
        let path = dir.0.join("nested").join("state").join(ACKS_FILE);
        assert_eq!(parse_acks(&std::fs::read_to_string(&path).expect("file")), acks);
        assert_eq!(store.error, None);

        acks.insert("local_b".into(), 20);
        store.save(&acks, false);
        assert_eq!(
            parse_acks(&std::fs::read_to_string(&path).expect("file")).len(),
            1,
            "a second write within 5 s waits"
        );
        store.save(&acks, true);
        assert_eq!(parse_acks(&std::fs::read_to_string(&path).expect("file")), acks);
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("dir"))
            .expect("list")
            .flatten()
            .filter(|e| e.file_name() != ACKS_FILE)
            .collect();
        assert!(leftovers.is_empty(), "no temporary file left");

        let mut reloaded = AckStore::new(dir.0.join("nested").join("state"));
        assert_eq!(reloaded.load(), acks);
    }

    #[test]
    fn ack_store_reports_write_failures() {
        let dir = TempDir::new("blocked");
        dir.write("state", "a file where the folder should be");
        let mut store = AckStore::new(dir.0.join("state"));
        store.save(&[("local_a".to_owned(), 1)].into(), true);
        assert!(store.error.as_deref().is_some_and(|e| e.starts_with("Impossible d'enregistrer")));
    }

    #[test]
    fn saved_acks_are_read_tolerantly() {
        assert!(parse_acks("").is_empty());
        assert!(parse_acks("not json").is_empty());
        assert!(parse_acks("[1,2]").is_empty());
        assert!(parse_acks(r#"{"acks":[1]}"#).is_empty());
        let acks = parse_acks(
            "\u{feff}{\"version\":9,\"acks\":{\"local_a\":12,\"local_b\":\"x\",\"\":3,\
             \"local_c\":-1,\"local_d\":1.5e12,\"cli:e\":7}}",
        );
        let expected: HashMap<String, i64> = [
            ("local_a".to_owned(), 12),
            ("local_d".to_owned(), 1_500_000_000_000),
            ("cli:e".to_owned(), 7),
        ]
        .into();
        assert_eq!(acks, expected);
        assert_eq!(parse_acks(&acks_json(&expected)), expected);
    }

    #[test]
    fn stored_acks_are_capped_to_the_most_recent() {
        let many: HashMap<String, i64> = (0..(MAX_STORED_ACKS as i64 + 10))
            .map(|i| (format!("local_{i}"), i + 1))
            .collect();
        let kept = capped(&many);
        assert_eq!(kept.len(), MAX_STORED_ACKS);
        assert!(!kept.contains_key("local_0"));
        assert!(kept.contains_key(&format!("local_{}", MAX_STORED_ACKS + 9)));
    }

    #[test]
    fn comparison_ignores_generated_at_and_seq() {
        let a = Snapshot {
            generated_at: 1,
            ready: true,
            seq: 1,
            sessions: vec![view("k", SessionState::Working)],
            counts: Counts {
                total: 1,
                working: 1,
                ..Counts::default()
            },
            health: SourceHealth::default(),
        };
        let mut b = a.clone();
        b.generated_at = 2;
        b.seq = 7;
        assert!(same_content(&a, &b));

        let mut c = b.clone();
        c.sessions[0].state = SessionState::Unseen;
        assert!(!same_content(&a, &c));

        let mut d = b.clone();
        d.health.errors.push("x".into());
        assert!(!same_content(&a, &d));

        let mut e = b.clone();
        e.counts.pr_open = 1;
        assert!(!same_content(&a, &e));

        let mut f = b;
        f.ready = false;
        assert!(!same_content(&a, &f));
    }

    #[test]
    fn on_screen_fallback_is_latest_focused() {
        let sessions = [
            ("local_a", Some(10)),
            ("local_b", Some(30)),
            ("local_c", None),
            ("local_d", Some(20)),
        ];
        assert_eq!(on_screen_session(sessions), Some("local_b"));
        assert_eq!(on_screen_session([("local_c", None)]), None);
        assert_eq!(on_screen_session(std::iter::empty()), None);
    }

    // -- what the user sees -------------------------------------------------

    const PANES_REAL: &str = r#"{"mcpServers":{},"preferences":{"sidebarMode":"epitaxy","epitaxyPrefs":{
        "desktop-frame.paneStore.v1":{"state":{"extraPanesByMode":{"code":[
            {"ref":{"kind":"code","id":"local_f0981680"},"pos":{"col":2,"row":0}},
            {"ref":{"kind":"code","id":"local_2ee8bd24"},"pos":{"col":1,"row":0}},
            {"ref":{"kind":"code","id":"local_63d0dbad"},"pos":{"col":0,"row":1}}]},
            "colWeightsByMode":{},"rowSplit":0.48,"draftNonce":3,
            "lastPrimaryCodeSession":{"id":"local_63d0dbad","type":"local"}},"version":4}}}}"#;

    #[test]
    fn pane_store_lists_the_sessions_in_view() {
        assert_eq!(
            parse_pane_store(PANES_REAL),
            Some(vec![
                "local_63d0dbad".to_owned(),
                "local_f0981680".to_owned(),
                "local_2ee8bd24".to_owned()
            ]),
            "primary pane first, no duplicates"
        );
        // Levels stored as JSON strings.
        let store = r#"{"state":{"lastPrimaryCodeSession":{"id":"local_x"}},"version":4}"#;
        let prefs = serde_json::json!({ PANE_STORE_KEY: store }).to_string();
        let as_strings = serde_json::json!({ "preferences": { "epitaxyPrefs": prefs } }).to_string();
        assert_eq!(parse_pane_store(&as_strings), Some(vec!["local_x".to_owned()]));
        // A single pane, with a BOM.
        let single = format!(
            "\u{feff}{}",
            r#"{"preferences":{"epitaxyPrefs":{"desktop-frame.paneStore.v1":{"state":{
                "extraPanesByMode":{"code":[]},"lastPrimaryCodeSession":{"id":" local_y "}}}}}}"#
        );
        assert_eq!(parse_pane_store(&single), Some(vec!["local_y".to_owned()]));
        // Known layout, nothing open.
        let none_open = r#"{"preferences":{"epitaxyPrefs":{"desktop-frame.paneStore.v1":{"state":{
            "extraPanesByMode":{"code":[]},"lastPrimaryCodeSession":null}}}}}"#;
        assert_eq!(parse_pane_store(none_open), Some(Vec::new()));
    }

    #[test]
    fn unknown_pane_layouts_give_none() {
        for json in [
            "",
            "not json",
            "{}",
            r#"{"preferences":{}}"#,
            r#"{"preferences":{"epitaxyPrefs":{}}}"#,
            r#"{"preferences":{"epitaxyPrefs":"{broken"}}"#,
            r#"{"preferences":{"epitaxyPrefs":{"desktop-frame.paneStore.v1":{"state":{"other":1}}}}}"#,
        ] {
            assert_eq!(parse_pane_store(json), None, "{json}");
        }
    }

    #[test]
    fn pane_reader_rereads_only_changed_files() {
        let dir = TempDir::new("panes");
        let path = dir.0.join("claude_desktop_config.json");
        let mut reader = PaneReader::new(path.clone());
        assert_eq!(reader.read(), None, "no file");
        dir.write("claude_desktop_config.json", PANES_REAL);
        assert_eq!(reader.read().map(<[String]>::len), Some(3));
        let partial = &PANES_REAL[..PANES_REAL.len() / 2];
        dir.write("claude_desktop_config.json", partial);
        // Caught mid-write: the last layout stands, and the file is read again.
        assert_eq!(reader.read().map(<[String]>::len), Some(3), "caught mid-write");
        assert_eq!(reader.stamp, None);
        // A valid file without a layout: unknown.
        dir.write("claude_desktop_config.json", r#"{"preferences":{}}"#);
        assert_eq!(reader.read(), None, "no layout");
        let single = PANES_REAL.replace("local_f0981680", "local_63d0dbad");
        dir.write("claude_desktop_config.json", &single);
        assert_eq!(reader.read().map(<[String]>::len), Some(2));
        std::fs::remove_file(&path).expect("remove");
        assert_eq!(reader.read(), None);
    }

    fn pass_with<'a>(
        desktop: &'a HashMap<String, DesktopSession>,
        live: &[(&str, &str)],
    ) -> Pass<'a> {
        Pass {
            registry: Vec::new(),
            registry_ok: true,
            desktop,
            desktop_ok: true,
            transcripts: HashMap::new(),
            listed_ids: HashSet::new(),
            keys: Vec::new(),
            live_key_by_host: live
                .iter()
                .map(|(host, key)| ((*host).to_owned(), (*key).to_owned()))
                .collect(),
        }
    }

    fn desktop_focus(entries: &[(&str, Option<i64>)]) -> HashMap<String, DesktopSession> {
        entries
            .iter()
            .map(|(id, focused)| {
                (
                    (*id).to_owned(),
                    DesktopSession {
                        host_session_id: (*id).to_owned(),
                        last_focused_at: *focused,
                        ..DesktopSession::default()
                    },
                )
            })
            .collect()
    }

    #[test]
    fn every_live_pane_is_visible() {
        let desktop = desktop_focus(&[
            ("local_a", Some(10)),
            ("local_b", Some(5)),
            ("local_c", Some(99)),
        ]);
        let pass = pass_with(&desktop, &[("local_a", "local_a"), ("local_b", "local_b")]);
        let layout = vec![
            "local_b".to_owned(),
            "local_a".to_owned(),
            "local_archived".to_owned(),
        ];
        assert_eq!(visible_keys(Some(&layout), &pass), ["local_b", "local_a"]);
        // An empty layout shows nothing, whatever the focus times say.
        assert!(visible_keys(Some(&[]), &pass).is_empty());
        // Without the layout: the session switched to last, when it is live.
        assert!(visible_keys(None, &pass).is_empty(), "local_c is not live");
        let desktop = desktop_focus(&[("local_a", Some(10)), ("local_b", Some(5))]);
        let pass = pass_with(&desktop, &[("local_a", "local_a"), ("local_b", "local_b")]);
        assert_eq!(visible_keys(None, &pass), ["local_a"]);
    }

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|k| (*k).to_owned()).collect()
    }

    fn set(list: &[&str]) -> HashSet<String> {
        list.iter().map(|k| (*k).to_owned()).collect()
    }

    #[test]
    fn desktop_must_stay_in_front_before_anything_counts_as_seen() {
        let mut screen = ScreenTracker::default();
        let t0 = Instant::now();
        let shown = keys(&["local_a", "local_b"]);
        assert!(screen.update(t0, true, &shown).is_empty(), "just came to the front");
        assert_eq!(screen.next_check(t0), Some(t0 + FOREGROUND_SETTLE));
        let settled = t0 + FOREGROUND_SETTLE;
        assert_eq!(screen.update(settled, true, &shown), set(&["local_a", "local_b"]));
        assert_eq!(screen.next_check(settled), None);

        // Leaving Desktop, even briefly, restarts the delay.
        assert!(screen.update(settled, false, &shown).is_empty());
        let back = settled + Duration::from_millis(100);
        assert!(screen.update(back, true, &shown).is_empty());
        assert!(screen.update(back + Duration::from_millis(500), true, &shown).is_empty());
        assert_eq!(screen.update(back + FOREGROUND_SETTLE, true, &shown).len(), 2);
    }

    #[test]
    fn opening_a_session_credits_nothing_else_until_desktop_shows_it() {
        let mut screen = ScreenTracker::default();
        let t0 = Instant::now();
        let before = keys(&["local_a"]);
        screen.update(t0, true, &before);
        assert_eq!(screen.update(t0 + FOREGROUND_SETTLE, true, &before).len(), 1);

        // The card of local_b is clicked: Desktop still shows local_a.
        let t1 = t0 + Duration::from_secs(3);
        screen.note_open("local_b".into(), t1 + OPEN_GRACE);
        assert!(screen.update(t1, true, &before).is_empty());
        let t2 = t1 + FOREGROUND_SETTLE;
        assert!(screen.update(t2, true, &before).is_empty(), "local_a is not credited");
        let after = keys(&["local_a", "local_b"]);
        assert_eq!(screen.update(t2, true, &after), set(&["local_a", "local_b"]));
        assert!(screen.pending_open.is_none());
        assert_eq!(screen.update(t2, true, &before).len(), 1, "back to normal");

        // Desktop never shows it: the wait ends after the grace period.
        screen.note_open("local_z".into(), t2 + OPEN_GRACE);
        let t3 = t2 + FOREGROUND_SETTLE;
        screen.update(t2, true, &before);
        assert!(screen.update(t3, true, &before).is_empty());
        assert_eq!(screen.update(t2 + OPEN_GRACE, true, &before).len(), 1);
    }

    #[test]
    fn on_screen_sessions_are_credited_only_for_new_activity() {
        let mut acks = HashMap::new();
        credit(&mut acks, "local_a", None, 100);
        assert!(acks.is_empty(), "nothing happened: nothing to credit");
        credit(&mut acks, "local_a", Some(50), 100);
        assert_eq!(acks["local_a"], 100);
        credit(&mut acks, "local_a", Some(80), 120);
        assert_eq!(acks["local_a"], 100, "unchanged: nothing to save");
        credit(&mut acks, "local_a", Some(110), 130);
        assert_eq!(acks["local_a"], 130);
        credit(&mut acks, "local_a", Some(500), 140);
        assert_eq!(acks["local_a"], 500, "never before the activity it covers");

        let desktop = DesktopSession {
            last_turn_edge_at: Some(10),
            latest_user_frame_at: Some(30),
            last_activity_at: Some(20),
            ..DesktopSession::default()
        };
        assert_eq!(latest_activity(Some(&desktop), None), Some(30));
        assert_eq!(latest_activity(Some(&desktop), Some(&40)), Some(40));
        assert_eq!(latest_activity(None, Some(&40)), Some(40));
        assert_eq!(latest_activity(None, None), None);
    }

    // -- turn ends ------------------------------------------------------------

    fn entry(session: &str, status: &str, at: Option<i64>) -> RegistryEntry {
        RegistryEntry {
            session_id: session.into(),
            status: Some(status.into()),
            status_updated_at: at,
            ..RegistryEntry::default()
        }
    }

    #[test]
    fn turn_end_is_when_the_process_leaves_busy() {
        let mut turns = TurnTracker::default();
        let busy = entry("s1", "busy", Some(1_000));
        turns.update([("local_a", &busy)], 1_500);
        assert!(turns.ended_at.is_empty(), "first sight ends nothing");

        let idle = entry("s1", "idle", Some(9_000));
        turns.update([("local_a", &idle)], 9_150);
        assert_eq!(turns.ended_at.get("local_a"), Some(&9_000));
        turns.update([("local_a", &idle)], 11_000);
        assert_eq!(turns.ended_at.get("local_a"), Some(&9_000), "idle stays idle");

        // A permission prompt is part of the turn; leaving it ends the turn.
        let busy = entry("s1", "busy", Some(12_000));
        turns.update([("local_a", &busy)], 12_100);
        let waiting = entry("s1", "waiting", Some(13_000));
        turns.update([("local_a", &waiting)], 13_100);
        assert_eq!(turns.ended_at.get("local_a"), Some(&9_000));
        let idle = entry("s1", "Idle ", Some(14_000));
        turns.update([("local_a", &idle)], 14_100);
        assert_eq!(turns.ended_at.get("local_a"), Some(&14_000));
    }

    #[test]
    fn implausible_status_times_fall_back_to_now() {
        let mut turns = TurnTracker::default();
        let busy = entry("s1", "busy", Some(5_000));
        turns.update([("local_a", &busy)], 5_100);
        let stale = entry("s1", "idle", Some(4_000));
        turns.update([("local_a", &stale)], 6_000);
        assert_eq!(turns.ended_at.get("local_a"), Some(&6_000));

        turns.update([("local_a", &busy)], 7_000);
        let future = entry("s1", "idle", Some(60_000));
        turns.update([("local_a", &future)], 8_000);
        assert_eq!(turns.ended_at.get("local_a"), Some(&8_000));

        turns.update([("local_a", &busy)], 9_000);
        let missing = entry("s1", "idle", None);
        turns.update([("local_a", &missing)], 10_000);
        assert_eq!(turns.ended_at.get("local_a"), Some(&10_000));
    }

    #[test]
    fn a_restarted_process_ends_no_turn() {
        let mut turns = TurnTracker::default();
        let busy = entry("old", "busy", Some(1_000));
        turns.update([("local_a", &busy)], 1_100);
        let fresh = entry("new", "idle", Some(2_000));
        turns.update([("local_a", &fresh)], 2_100);
        assert!(turns.ended_at.is_empty());

        // A session that went away is forgotten.
        let busy = entry("new", "busy", Some(3_000));
        turns.update([("local_a", &busy)], 3_100);
        let idle = entry("new", "idle", Some(4_000));
        turns.update([("local_a", &idle)], 4_100);
        assert_eq!(turns.ended_at.len(), 1);
        turns.update(std::iter::empty(), 5_000);
        assert!(turns.ended_at.is_empty());
        assert!(turns.last.is_empty());

        // Desktop restarted the process under the same CLI session id.
        let mut turns = TurnTracker::default();
        let old = RegistryEntry {
            pid: 100,
            ..entry("s1", "busy", Some(1_000))
        };
        turns.update([("local_a", &old)], 1_100);
        let resumed = RegistryEntry {
            pid: 200,
            ..entry("s1", "idle", Some(5_000))
        };
        turns.update([("local_a", &resumed)], 5_100);
        assert!(turns.ended_at.is_empty(), "a new pid is a new process");
        // Same pid, another start time: the pid was reused.
        let busy = RegistryEntry {
            pid: 200,
            proc_start: Some("1".into()),
            ..entry("s1", "busy", Some(6_000))
        };
        turns.update([("local_a", &busy)], 6_100);
        let reused = RegistryEntry {
            pid: 200,
            proc_start: Some("2".into()),
            ..entry("s1", "idle", Some(7_000))
        };
        turns.update([("local_a", &reused)], 7_100);
        assert!(turns.ended_at.is_empty(), "a reused pid is a new process");
    }

    #[test]
    fn the_shown_process_of_each_key_is_tracked() {
        let desktop: HashMap<String, DesktopSession> = [(
            "local_a".to_owned(),
            DesktopSession {
                host_session_id: "local_a".into(),
                cli_session_id: Some("new".into()),
                ..DesktopSession::default()
            },
        )]
        .into_iter()
        .collect();
        let mut pass = pass_with(&desktop, &[]);
        let with_host = |id: &str, status: &str| RegistryEntry {
            host_session_id: Some("local_a".into()),
            ..entry(id, status, Some(1))
        };
        pass.registry = vec![
            with_host("old", "busy"),
            with_host("new", "idle"),
            entry("cli", "busy", Some(1)),
        ];
        pass.keys = vec!["local_a".into(), "local_a".into(), "cli:cli".into()];
        let chosen = chosen_entries(&pass);
        assert_eq!(chosen.len(), 2);
        assert_eq!(chosen[0].0, "local_a");
        assert_eq!(chosen[0].1.session_id, "new", "the process Desktop links to");
        assert_eq!(chosen[1].1.session_id, "cli");
    }

    // -- file events ----------------------------------------------------------

    #[test]
    fn only_relevant_files_wake_the_worker() {
        assert!(registry_event(Path::new(r"C:\u\.claude\sessions\36220.json")));
        assert!(!registry_event(Path::new(r"C:\u\.claude\sessions\36220.0a1b.key")));
        assert!(!registry_event(Path::new(r"C:\u\.claude\sessions")));

        assert!(desktop_event(Path::new(r"C:\a\claude-code-sessions\acc\org\local_1.json")));
        assert!(!desktop_event(Path::new(r"C:\a\claude-code-sessions\acc\org\local_1.json.tmp")));
        assert!(!desktop_event(Path::new(r"C:\a\claude-code-sessions\acc\org\other.json")));
        assert!(!desktop_event(Path::new(r"C:\a\claude-code-sessions\acc\org\local_.json")));

        let root = Path::new(r"C:\u\.claude\projects");
        let live: HashSet<String> = ["abc-123".to_owned()].into();
        assert!(transcript_event(root, &root.join("D--repo").join("abc-123.jsonl"), &live));
        assert!(!transcript_event(root, &root.join("D--repo").join("zzz-999.jsonl"), &live));
        assert!(!transcript_event(
            root,
            &root.join("D--repo").join("abc-123").join("subagents").join("agent-1.jsonl"),
            &live
        ));
        assert!(!transcript_event(
            root,
            &root.join("D--repo").join("abc-123").join("tool-results").join("x.txt"),
            &live
        ));
        assert!(!transcript_event(root, &root.join("D--repo").join("abc-123.json"), &live));
        assert!(!transcript_event(root, Path::new(r"C:\elsewhere\D--repo\abc-123.jsonl"), &live));
    }

    #[test]
    fn events_are_filtered_by_kind_and_path() {
        use notify::event::{AccessKind, CreateKind, Flag, ModifyKind};
        let accept_json: &(dyn Fn(&Path) -> bool + Send + Sync) =
            &|path: &Path| path.extension().is_some_and(|e| e == "json");
        let modify = notify::Event::new(EventKind::Modify(ModifyKind::Any));
        assert!(wakes_up(&modify.clone().add_path(PathBuf::from("a.json")), accept_json));
        assert!(!wakes_up(&modify.clone().add_path(PathBuf::from("a.txt")), accept_json));
        let read = notify::Event::new(EventKind::Access(AccessKind::Any)).add_path(PathBuf::from("a.json"));
        assert!(!wakes_up(&read, accept_json));
        let rescan = notify::Event::new(EventKind::Create(CreateKind::Any)).set_flag(Flag::Rescan);
        assert!(wakes_up(&rescan, accept_json));
    }

    // -- unchanged helpers ----------------------------------------------------

    #[test]
    fn desktop_app_image_detection() {
        assert!(is_desktop_app_image(
            r"C:\Program Files\WindowsApps\Claude_3.1623.4.0_x64__pzs8sxrjxfjjc\app\claude.exe"
        ));
        assert!(is_desktop_app_image(
            r"C:\Users\Zoé\AppData\Local\AnthropicClaude\app-1.2.3\Claude.exe"
        ));
        assert!(!is_desktop_app_image(r"C:\Users\Zoé\.local\bin\claude.exe"));
        assert!(!is_desktop_app_image(
            r"C:\Program Files\WindowsApps\Claude_3.1623.4.0_x64__pzs8sxrjxfjjc\app\helper.exe"
        ));
        assert!(!is_desktop_app_image(
            r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal_1.0\claude.exe"
        ));
        assert!(!is_desktop_app_image(""));
    }

    #[test]
    fn foreground_check_runs() {
        // The answer depends on the machine; this only exercises the Win32 path.
        let _ = desktop_app_in_foreground();
    }

    #[test]
    fn drain_merges_queued_messages() {
        let (tx, rx) = mpsc::channel();
        assert_eq!(drain(&rx), Wake::default());

        tx.send(Msg::Dirty).ok();
        tx.send(Msg::RefreshAll).ok();
        tx.send(Msg::Refresh).ok();
        assert_eq!(
            drain(&rx),
            Wake {
                stop: false,
                refresh_all: true
            }
        );
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        tx.send(Msg::Dirty).ok();
        tx.send(Msg::Stop).ok();
        assert!(drain(&rx).stop);

        drop(tx);
        assert!(drain(&rx).stop);
    }

    #[test]
    fn wake_merge_keeps_requests() {
        let mut wake = Wake::default();
        wake.add(Msg::Dirty);
        wake.merge(Wake {
            stop: false,
            refresh_all: true,
        });
        assert!(wake.refresh_all && !wake.stop);
        wake.add(Msg::Stop);
        assert!(wake.stop);
    }

    #[test]
    fn prune_keeps_live_and_desktop_keys() {
        let mut acks: HashMap<String, i64> = [
            ("local_live".to_owned(), 1),
            ("local_archived".to_owned(), 2),
            ("cli:gone".to_owned(), 3),
        ]
        .into_iter()
        .collect();
        let keep: HashSet<&str> = ["local_live", "local_archived", "cli:other"].into();
        prune_acks(&mut acks, &keep);
        assert_eq!(acks.len(), 2);
        assert!(!acks.contains_key("cli:gone"));
    }

    #[test]
    fn guarded_turns_panics_into_messages() {
        assert_eq!(guarded(|| 7), Ok(7));
        assert_eq!(guarded(|| -> i32 { panic!("boom") }), Err("boom".to_owned()));
        let n = 3;
        assert_eq!(
            guarded(|| -> i32 { panic!("bad {n}") }),
            Err("bad 3".to_owned())
        );
    }

    #[test]
    fn dedup_keeps_first_occurrences() {
        let errors = vec!["a".into(), "b".into(), "a".into(), "c".into(), "b".into()];
        assert_eq!(dedup(errors), vec!["a", "b", "c"]);
    }

    #[test]
    fn listed_sessions_follow_settings() {
        let desktop = RegistryEntry {
            session_id: "s1".into(),
            host_session_id: Some("local_x".into()),
            ..RegistryEntry::default()
        };
        let desktop_no_host = RegistryEntry {
            session_id: "s2".into(),
            entrypoint: Some("claude-desktop".into()),
            ..RegistryEntry::default()
        };
        let cli = RegistryEntry {
            session_id: "s3".into(),
            entrypoint: Some("cli".into()),
            host_session_id: Some(String::new()),
            ..RegistryEntry::default()
        };
        let default = Settings::default();
        assert!(is_listed(&desktop, &default, false));
        assert!(is_listed(&desktop_no_host, &default, false));
        assert!(!is_listed(&cli, &default, false));
        assert!(is_listed(&cli, &default, true));
        let all = Settings {
            include_cli_sessions: true,
            ..Settings::default()
        };
        assert!(is_listed(&cli, &all, false));
    }

    #[test]
    fn requests_use_desktop_then_transcript_branch() {
        let desktop: HashMap<String, DesktopSession> = [(
            "local_a".to_owned(),
            DesktopSession {
                host_session_id: "local_a".into(),
                cli_session_id: Some("cli-a".into()),
                branch: Some("feat/a".into()),
                last_turn_edge_at: Some(99),
                ..DesktopSession::default()
            },
        )]
        .into_iter()
        .collect();
        let registry = vec![
            RegistryEntry {
                session_id: "cli-a".into(),
                cwd: r"D:\Données Utilisateur\repo".into(),
                status: Some("busy".into()),
                ..RegistryEntry::default()
            },
            RegistryEntry {
                session_id: "cli-b".into(),
                cwd: r"D:\b".into(),
                entrypoint: Some("claude-desktop".into()),
                host_session_id: Some("local_b".into()),
                status: Some("idle".into()),
                ..RegistryEntry::default()
            },
        ];
        let transcripts: HashMap<String, TranscriptInfo> = [(
            "cli-b".to_owned(),
            TranscriptInfo {
                git_branch: Some("fix/b".into()),
                ..TranscriptInfo::default()
            },
        )]
        .into_iter()
        .collect();
        let pass = Pass {
            registry,
            registry_ok: true,
            desktop: &desktop,
            desktop_ok: true,
            transcripts,
            listed_ids: HashSet::new(),
            keys: vec!["cli:cli-a".into(), "local_b".into()],
            live_key_by_host: HashMap::new(),
        };
        let all = Settings {
            include_cli_sessions: true,
            ..Settings::default()
        };
        let requests = git_pr_requests(&pass, &all);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].key, "cli:cli-a");
        assert_eq!(requests[0].branch_hint.as_deref(), Some("feat/a"));
        assert_eq!(requests[0].turn_edge_at, Some(99));
        assert!(requests[0].working);
        assert_eq!(requests[1].branch_hint.as_deref(), Some("fix/b"));
        assert_eq!(requests[1].turn_edge_at, None);
        assert!(!requests[1].working);

        let mut pass = pass;
        pass.registry.push(RegistryEntry {
            session_id: "cli-c".into(),
            cwd: r"D:\c".into(),
            ..RegistryEntry::default()
        });
        pass.keys.push("cli:cli-c".into());
        let listed = git_pr_requests(&pass, &Settings::default());
        let keys: Vec<&str> = listed.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["cli:cli-a", "local_b"]);
    }

    #[test]
    fn requests_keep_one_per_key_preferring_linked_process() {
        let desktop: HashMap<String, DesktopSession> = [(
            "local_a".to_owned(),
            DesktopSession {
                host_session_id: "local_a".into(),
                cli_session_id: Some("new".into()),
                ..DesktopSession::default()
            },
        )]
        .into_iter()
        .collect();
        let entry = |id: &str, cwd: &str| RegistryEntry {
            session_id: id.into(),
            cwd: cwd.into(),
            host_session_id: Some("local_a".into()),
            ..RegistryEntry::default()
        };
        let pass = Pass {
            registry: vec![
                entry("old", r"D:\old"),
                entry("new", r"D:\new"),
                entry("other", r"D:\x"),
            ],
            registry_ok: true,
            desktop: &desktop,
            desktop_ok: true,
            transcripts: HashMap::new(),
            listed_ids: HashSet::new(),
            keys: vec!["local_a".into(), "local_a".into(), "local_a".into()],
            live_key_by_host: HashMap::new(),
        };
        let requests = git_pr_requests(&pass, &Settings::default());
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].cwd, r"D:\new");
    }

    /// Read-only: runs the real engine for 75 s on this machine's sessions
    /// and prints what it publishes. Used to count the git processes it
    /// starts. `cargo test engine::tests::live_engine -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the real sessions and runs git/gh for 75 s"]
    fn live_engine() {
        println!("pid {}", std::process::id());
        let count = Arc::new(Mutex::new(0usize));
        let counter = Arc::clone(&count);
        let engine = Engine::start(
            Settings::default(),
            Box::new(move |snapshot, changes| {
                *lock(&counter) += 1;
                println!(
                    "seq {} ready {} sessions {} on screen {} changes {}",
                    snapshot.seq,
                    snapshot.ready,
                    snapshot.sessions.len(),
                    snapshot.sessions.iter().filter(|s| s.on_screen).count(),
                    changes.len()
                );
            }),
        );
        thread::sleep(Duration::from_secs(75));
        println!("snapshots published: {}", lock(&count));
        engine.shutdown();
    }
}
