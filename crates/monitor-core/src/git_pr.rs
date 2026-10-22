//! Branch, uncommitted files and pull request of each session, through `git`
//! and `gh`, on a background thread.
//!
//! One worker thread runs every command, one at a time. Git state is cached
//! per folder, the default branch per repository and pull requests per lookup
//! (PR URL, or repository + branch), so sessions sharing a folder or a branch
//! cost a single call.
//!
//! A folder costs one `git status` per read. It is read again when a turn of
//! one of its sessions ends, when its repository files (HEAD, index, branch
//! ref) change, every 30 s while one of its sessions works and every 5 min
//! otherwise.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use crate::model::{ChecksState, DesktopPrBinding, GitInfo, PrInfo};
use crate::paths::now_ms;

/// What the service needs to know about one session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitPrRequest {
    /// Session key (see `aggregate::session_key`).
    pub key: String,
    pub cwd: String,
    /// Branch known from Desktop or the transcript, if any. Only used when
    /// git cannot tell the branch checked out (detached HEAD).
    pub branch_hint: Option<String>,
    /// PR bound by Desktop, if any.
    pub desktop_pr: Option<DesktopPrBinding>,
    /// End of the latest turn: a new value triggers a refresh for this key.
    pub turn_edge_at: Option<i64>,
    /// The session is running a turn: its folder is read more often.
    pub working: bool,
}

pub type ChangeCallback = Arc<dyn Fn() + Send + Sync>;

/// Git and PR result of one session.
type GitPrResult = (Option<GitInfo>, Option<PrInfo>);

/// A folder is read again this often while one of its sessions works...
const GIT_REFRESH_ACTIVE: Duration = Duration::from_secs(30);
/// ...and this often otherwise (turn ends and repository changes come first).
const GIT_REFRESH_IDLE: Duration = Duration::from_secs(300);
/// The repository files of a folder are compared with its last read this often.
const GIT_STAMP_CHECK: Duration = Duration::from_secs(5);
/// A change of the repository files is not read sooner than this after the
/// previous read, so a busy repository costs at most one read per 10 s.
const GIT_STAMP_MIN_AGE: Duration = Duration::from_secs(10);
/// A forced git refresh is skipped if the folder was read this recently.
const GIT_FORCE_MIN: Duration = Duration::from_secs(2);
/// A pull request is never looked up more often than this.
const PR_MIN_INTERVAL: Duration = Duration::from_secs(10);
/// Merged and closed pull requests are polled at most this often.
const PR_CLOSED_INTERVAL: Duration = Duration::from_secs(600);
const DEFAULT_BRANCH_TTL: Duration = Duration::from_secs(600);
/// No `gh` call at all for this long after a failure every lookup would hit
/// too: not installed, not logged in, network down, rate limit.
const GH_BACKOFF: Duration = Duration::from_secs(300);
/// A lookup that failed on its own (no access to the repository, SSO,
/// timeout) waits this long, doubled after each new failure...
const LOOKUP_BACKOFF: Duration = Duration::from_secs(300);
/// ...up to this.
const LOOKUP_BACKOFF_MAX: Duration = Duration::from_secs(3600);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
/// Time left to the pipe readers once the child has exited.
const PIPE_GRACE: Duration = Duration::from_secs(2);
const MAX_IDLE_WAIT: Duration = Duration::from_secs(60);
const MIN_WAIT: Duration = Duration::from_millis(200);
/// How long `Drop` waits for the worker to stop.
const STOP_WAIT: Duration = Duration::from_secs(2);
const MIN_POLL_SECS: u64 = 10;

const PR_FIELDS: &str = "number,url,title,state,isDraft,reviewDecision,statusCheckRollup,\
                         mergeStateStatus,headRefName";
const SOURCE_DESKTOP: &str = "desktop";
const SOURCE_BRANCH: &str = "gh-branch";
/// One command gives the branch and the uncommitted files.
const STATUS_ARGS: &[&str] = &[
    "-c",
    "core.quotepath=off",
    "status",
    "--porcelain=v2",
    "--branch",
    "--untracked-files=normal",
];
/// Where the repository of a folder lives; run once per folder.
const LAYOUT_ARGS: &[&str] = &[
    "rev-parse",
    "--path-format=absolute",
    "--show-toplevel",
    "--git-dir",
    "--git-common-dir",
];

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Runs `git` and `gh` on a background thread and keeps the latest results.
pub struct GitPrService {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl GitPrService {
    /// Starts the worker. `on_change` is called whenever a result changes.
    pub fn start(poll_secs: u64, on_change: ChangeCallback) -> Self {
        Self::start_with_runner(poll_secs, on_change, Box::new(ProcessRunner::default()))
    }

    fn start_with_runner(
        poll_secs: u64,
        on_change: ChangeCallback,
        runner: Box<dyn Runner>,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                poll_secs: poll_secs.max(MIN_POLL_SECS),
                ..State::default()
            }),
            cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let worker = Worker::new(Arc::clone(&shared), runner, on_change);
        let handle = thread::Builder::new()
            .name("git-pr".into())
            .spawn(move || worker.run())
            .ok();
        Self {
            shared,
            worker: handle,
        }
    }

    /// Replaces the set of tracked sessions. Never blocks.
    pub fn track(&self, requests: Vec<GitPrRequest>) {
        let mut seen = HashSet::new();
        let mut requests: Vec<GitPrRequest> = requests
            .into_iter()
            .filter(|r| seen.insert(r.key.clone()))
            .collect();
        requests.sort_by(|a, b| a.key.cmp(&b.key));
        let mut st = self.shared.lock();
        if st.requests == requests {
            return;
        }
        st.results.retain(|key, _| seen.contains(key));
        st.bumped.retain(|key| seen.contains(key));
        st.requests = requests;
        st.wake = true;
        drop(st);
        self.shared.cv.notify_all();
    }

    /// Latest results by session key.
    pub fn results(&self) -> HashMap<String, (Option<GitInfo>, Option<PrInfo>)> {
        let st = self.shared.lock();
        let tracked: HashSet<&str> = st.requests.iter().map(|r| r.key.as_str()).collect();
        st.results
            .iter()
            .filter(|(key, _)| tracked.contains(key.as_str()))
            .map(|(key, result)| (key.clone(), result.clone()))
            .collect()
    }

    /// None until `gh` has been run once.
    pub fn gh_ok(&self) -> Option<bool> {
        self.shared.lock().gh_ok
    }

    /// Asks for a refresh of one session soon. Also lets its PR lookups try
    /// `gh` once even during a back-off that follows a `gh` failure.
    pub fn bump(&self, key: &str) {
        let mut st = self.shared.lock();
        if !st.requests.iter().any(|r| r.key == key) {
            return;
        }
        st.bumped.insert(key.to_string());
        st.wake = true;
        drop(st);
        self.shared.cv.notify_all();
    }

    /// Sets the PR polling interval (at least 10 s).
    pub fn set_poll_secs(&self, poll_secs: u64) {
        let poll_secs = poll_secs.max(MIN_POLL_SECS);
        let mut st = self.shared.lock();
        if st.poll_secs == poll_secs {
            return;
        }
        st.poll_secs = poll_secs;
        st.wake = true;
        drop(st);
        self.shared.cv.notify_all();
    }
}

impl Drop for GitPrService {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.lock().wake = true;
        self.shared.cv.notify_all();
        let Some(handle) = self.worker.take() else {
            return;
        };
        // The last owner may be dropped from `on_change`, on the worker itself.
        if handle.thread().id() == thread::current().id() {
            return;
        }
        let deadline = Instant::now() + STOP_WAIT;
        let mut st = self.shared.lock();
        while !st.exited {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = match self.shared.cv.wait_timeout(st, left) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        let exited = st.exited;
        drop(st);
        if exited {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// State shared between the service handle and the worker
// ---------------------------------------------------------------------------

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
    shutdown: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Default)]
struct State {
    requests: Vec<GitPrRequest>,
    results: HashMap<String, GitPrResult>,
    gh_ok: Option<bool>,
    poll_secs: u64,
    bumped: HashSet<String>,
    /// Set by the handle when the worker should start a cycle now.
    wake: bool,
    /// Set by the worker when it stops.
    exited: bool,
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

struct GitEntry {
    info: Option<GitInfo>,
    fetched_at: Instant,
    /// Where the folder's repository lives; None outside a repository.
    layout: Option<RepoLayout>,
    /// Repository files as of the last read; None when they cannot be watched.
    stamp: Option<Stamp>,
    /// When `stamp` was last compared with the files.
    stamp_checked_at: Instant,
}

impl GitEntry {
    /// A folder that is not inside a git work tree (or does not exist).
    fn outside(now: Instant) -> Self {
        Self {
            info: None,
            fetched_at: now,
            layout: None,
            stamp: None,
            stamp_checked_at: now,
        }
    }
}

/// Where the repository of a folder lives.
#[derive(Debug, Clone, PartialEq)]
struct RepoLayout {
    /// Root of the work tree, native path.
    top: String,
    /// `.git` of this work tree, and the one its worktrees share. None with a
    /// git too old to report them as absolute paths.
    git_dir: Option<PathBuf>,
    common_dir: Option<PathBuf>,
}

/// Modification time and size of the repository files a folder's state
/// depends on (None for a missing file).
type Stamp = Vec<Option<(SystemTime, u64)>>;

struct PrEntry {
    /// Latest PR found; kept when a later lookup fails.
    pr: Option<PrInfo>,
    /// Branch the PR was opened from (`headRefName`).
    head: Option<String>,
    /// Latest attempt, successful or not.
    fetched_at: Instant,
}

/// Back-off of one lookup that failed on its own.
struct Backoff {
    until: Instant,
    failures: u32,
}

/// One deduplicated PR lookup.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PrLookup {
    /// `gh pr view <url>` for a PR bound by Desktop.
    Url(String),
    /// `gh pr list --head <branch>` run in the repository.
    Branch { repo_root: String, branch: String },
}

/// The lookups of one session: the PR Desktop bound to it, and the PR of the
/// branch checked out.
#[derive(Debug, Clone, Default, PartialEq)]
struct LookupPlan {
    binding: Option<PrLookup>,
    branch: Option<PrLookup>,
}

impl LookupPlan {
    fn iter(&self) -> impl Iterator<Item = &PrLookup> {
        self.binding.iter().chain(self.branch.iter())
    }
}

/// Why a `gh` call gave no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GhFailure {
    /// Every lookup would fail too: not installed, not logged in, network,
    /// rate limit.
    Global,
    /// This lookup only: no access to the repository, SSO, timeout.
    Lookup,
    /// Nothing to learn (cancelled, unexpected output): retried at the next poll.
    Transient,
}

/// A PR found by `gh`, with its branch. None when there is no PR.
type Found = Option<(PrInfo, Option<String>)>;

struct Worker {
    shared: Arc<Shared>,
    runner: Box<dyn Runner>,
    on_change: ChangeCallback,
    git: HashMap<String, GitEntry>,
    default_branches: HashMap<String, (Option<String>, Instant)>,
    prs: HashMap<PrLookup, PrEntry>,
    /// Latest `turn_edge_at` seen per session key.
    seen_edges: HashMap<String, Option<i64>>,
    force_git: HashSet<String>,
    force_pr: HashSet<PrLookup>,
    bypass_backoff: HashSet<PrLookup>,
    lookup_backoff: HashMap<PrLookup, Backoff>,
    gh_ok: Option<bool>,
    gh_backoff_until: Option<Instant>,
}

impl Worker {
    fn new(shared: Arc<Shared>, runner: Box<dyn Runner>, on_change: ChangeCallback) -> Self {
        Self {
            shared,
            runner,
            on_change,
            git: HashMap::new(),
            default_branches: HashMap::new(),
            prs: HashMap::new(),
            seen_edges: HashMap::new(),
            force_git: HashSet::new(),
            force_pr: HashSet::new(),
            bypass_backoff: HashSet::new(),
            lookup_backoff: HashMap::new(),
            gh_ok: None,
            gh_backoff_until: None,
        }
    }

    fn run(mut self) {
        while !self.cancelled() {
            let wait = self.cycle();
            self.wait(wait);
        }
        self.shared.lock().exited = true;
        self.shared.cv.notify_all();
    }

    fn cancelled(&self) -> bool {
        self.shared.shutdown.load(Ordering::SeqCst)
    }

    fn wait(&self, wait: Duration) {
        let deadline = Instant::now() + wait;
        let mut st = self.shared.lock();
        while !st.wake && !self.cancelled() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = match self.shared.cv.wait_timeout(st, left) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Refreshes whatever is due and returns how long to sleep.
    fn cycle(&mut self) -> Duration {
        let (requests, bumped, poll_secs) = {
            let mut st = self.shared.lock();
            st.wake = false;
            (
                st.requests.clone(),
                std::mem::take(&mut st.bumped),
                st.poll_secs,
            )
        };
        let poll = Duration::from_secs(poll_secs.max(MIN_POLL_SECS));
        let forced = self.forced_keys(&requests, &bumped);

        for r in requests.iter().filter(|r| forced.contains(&r.key)) {
            self.force_git.insert(r.cwd.clone());
        }
        let active = active_cwds(&requests);
        let mut cwds: Vec<&str> = Vec::new();
        for r in &requests {
            if !cwds.contains(&r.cwd.as_str()) {
                cwds.push(&r.cwd);
            }
        }
        cwds.sort_by_key(|cwd| !self.force_git.contains(*cwd));
        for cwd in cwds {
            if self.cancelled() {
                return MIN_WAIT;
            }
            if self.git_due(cwd, active.contains(cwd), Instant::now()) {
                self.refresh_git(cwd);
                self.force_git.remove(cwd);
            }
        }

        let plans: Vec<LookupPlan> = requests.iter().map(|r| self.plan_for(r)).collect();
        for (r, plan) in requests.iter().zip(&plans) {
            for lookup in plan.iter() {
                if forced.contains(&r.key) {
                    self.force_pr.insert(lookup.clone());
                }
                if bumped.contains(&r.key) {
                    self.bypass_backoff.insert(lookup.clone());
                }
            }
        }
        self.prune(&requests, &plans);
        self.publish(&requests, &plans);

        let now = Instant::now();
        for lookup in self.fetch_order(&plans) {
            if self.cancelled() {
                return MIN_WAIT;
            }
            if self.pr_due_at(&lookup, poll).is_some_and(|due| due > now) {
                continue;
            }
            if self.blocked(&lookup, now) {
                continue;
            }
            self.run_lookup(&lookup);
            self.publish(&requests, &plans);
            // New requests start a new cycle; at least one lookup was done.
            if self.shared.lock().wake {
                return MIN_WAIT;
            }
        }
        self.next_wait(&requests, &plans, poll)
    }

    /// Keys to refresh right away: new keys, bumped keys and keys whose turn
    /// just ended.
    fn forced_keys(
        &mut self,
        requests: &[GitPrRequest],
        bumped: &HashSet<String>,
    ) -> HashSet<String> {
        let mut forced = HashSet::new();
        for r in requests {
            let previous = self.seen_edges.get(&r.key).copied();
            let is_new = match previous {
                None => true,
                Some(edge) => r.turn_edge_at.is_some() && r.turn_edge_at != edge,
            };
            if is_new || bumped.contains(&r.key) {
                forced.insert(r.key.clone());
            }
            self.seen_edges
                .insert(r.key.clone(), r.turn_edge_at.or(previous.flatten()));
        }
        let keys: HashSet<&str> = requests.iter().map(|r| r.key.as_str()).collect();
        self.seen_edges.retain(|key, _| keys.contains(key.as_str()));
        forced
    }

    /// True when the folder must be read now: never read, forced, its
    /// interval elapsed, or its repository files changed since the last read.
    fn git_due(&mut self, cwd: &str, active: bool, now: Instant) -> bool {
        let forced = self.force_git.contains(cwd);
        let Some(entry) = self.git.get_mut(cwd) else {
            return true;
        };
        let age = now.saturating_duration_since(entry.fetched_at);
        if age >= git_interval(entry, active) || (forced && age >= GIT_FORCE_MIN) {
            return true;
        }
        if age < GIT_STAMP_MIN_AGE
            || now.saturating_duration_since(entry.stamp_checked_at) < GIT_STAMP_CHECK
        {
            return false;
        }
        let Some(layout) = &entry.layout else {
            return false;
        };
        entry.stamp_checked_at = now;
        let branch = entry.info.as_ref().and_then(|info| info.branch.as_deref());
        let stamp = read_stamp(layout, branch);
        stamp.is_some() && stamp != entry.stamp
    }

    /// Reads the branch, root, dirty count and default branch of a folder.
    fn refresh_git(&mut self, cwd: &str) {
        let previous = self.git.remove(cwd);
        let dir = Path::new(cwd);
        if cwd.trim().is_empty() || !dir.is_dir() {
            self.git
                .insert(cwd.to_string(), GitEntry::outside(Instant::now()));
            return;
        }
        let previous_info = previous.as_ref().and_then(|entry| entry.info.clone());
        let known_layout = previous.and_then(|entry| entry.layout);
        let Some(mut layout) = known_layout.or_else(|| self.repo_layout(dir)) else {
            self.git
                .insert(cwd.to_string(), GitEntry::outside(Instant::now()));
            return;
        };
        let old_branch = previous_info.as_ref().and_then(|info| info.branch.clone());
        // Taken before the read, so a change made during it is seen next time.
        let stamp_before = read_stamp(&layout, old_branch.as_deref());
        let status = self.git_cmd(dir, STATUS_ARGS).map(|out| parse_status_v2(&out));
        let info = match status {
            Some(status) => {
                let default = self.default_branch(&layout, dir);
                Some(GitInfo {
                    is_default_branch: status.branch.is_some() && status.branch == default,
                    branch: status.branch,
                    repo_root: Some(layout.top.clone()),
                    dirty_files: status.dirty,
                    checked_at: now_ms(),
                })
            }
            // Not a repository any more, or a passing failure: find out which.
            None => match self.repo_layout(dir) {
                None => {
                    self.git
                        .insert(cwd.to_string(), GitEntry::outside(Instant::now()));
                    return;
                }
                Some(fresh) => {
                    layout = fresh;
                    previous_info
                }
            },
        };
        let branch = info.as_ref().and_then(|info| info.branch.clone());
        let stamp = if branch == old_branch {
            stamp_before
        } else {
            read_stamp(&layout, branch.as_deref())
        };
        let now = Instant::now();
        self.git.insert(
            cwd.to_string(),
            GitEntry {
                info,
                fetched_at: now,
                layout: Some(layout),
                stamp,
                stamp_checked_at: now,
            },
        );
    }

    /// Work tree root and git folders of `dir`. None outside a repository.
    fn repo_layout(&mut self, dir: &Path) -> Option<RepoLayout> {
        let out = self
            .runner
            .run("git", LAYOUT_ARGS, Some(dir), &self.shared.shutdown)
            .ok()?;
        if out.success {
            if let Some(layout) = parse_layout(&out.stdout) {
                return Some(layout);
            }
        } else if out
            .stderr
            .to_ascii_lowercase()
            .contains("not a git repository")
        {
            return None;
        }
        // Git before 2.31 has no --path-format; the root alone still works.
        let top = self.git_cmd(dir, &["rev-parse", "--show-toplevel"])?;
        let top = native_path(top.trim());
        (!top.is_empty()).then_some(RepoLayout {
            top,
            git_dir: None,
            common_dir: None,
        })
    }

    /// Default branch of a repository, cached per repository root. Read from
    /// the repository files when possible, which needs no process.
    fn default_branch(&mut self, layout: &RepoLayout, dir: &Path) -> Option<String> {
        if let Some((branch, at)) = self.default_branches.get(&layout.top)
            && at.elapsed() < DEFAULT_BRANCH_TTL
        {
            return branch.clone();
        }
        let found = match layout
            .common_dir
            .as_deref()
            .and_then(default_branch_from_files)
        {
            Some(found) => found,
            None => self.default_branch_from_git(dir),
        };
        self.default_branches
            .insert(layout.top.clone(), (found.clone(), Instant::now()));
        found
    }

    fn default_branch_from_git(&mut self, dir: &Path) -> Option<String> {
        let found = self
            .git_cmd(
                dir,
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            )
            .and_then(|out| parse_default_branch(&out));
        if found.is_some() {
            return found;
        }
        for candidate in ["main", "master"] {
            let reference = format!("refs/heads/{candidate}");
            if self
                .git_cmd(dir, &["rev-parse", "--verify", "--quiet", &reference])
                .is_some()
            {
                return Some(candidate.to_string());
            }
        }
        None
    }

    /// Stdout of a successful git command.
    fn git_cmd(&mut self, dir: &Path, args: &[&str]) -> Option<String> {
        match self
            .runner
            .run("git", args, Some(dir), &self.shared.shutdown)
        {
            Ok(out) if out.success => Some(out.stdout),
            _ => None,
        }
    }

    /// What to ask `gh` for this session.
    fn plan_for(&self, r: &GitPrRequest) -> LookupPlan {
        let binding = r.desktop_pr.as_ref().and_then(binding_url).map(PrLookup::Url);
        // An open bound PR has never been asked about yet: its branch, known
        // after that first answer, may make the branch lookup useless.
        let wait_for_binding = binding.as_ref().is_some_and(|lookup| {
            !self.prs.contains_key(lookup)
                && r.desktop_pr
                    .as_ref()
                    .and_then(|b| b.state.as_deref())
                    .is_some_and(|state| state.trim().eq_ignore_ascii_case("OPEN"))
        });
        let branch = if wait_for_binding {
            None
        } else {
            self.branch_lookup(r)
                .filter(|lookup| !self.binding_covers(binding.as_ref(), lookup))
        };
        LookupPlan { binding, branch }
    }

    /// Lookup of the PR of the branch checked out in the session's folder.
    fn branch_lookup(&self, r: &GitPrRequest) -> Option<PrLookup> {
        let git = self.git.get(&r.cwd)?.info.as_ref()?;
        let repo_root = git.repo_root.clone()?;
        // The branch checked out is what the card shows; the hint only stands
        // in when git cannot name it (detached HEAD).
        let branch = usable_branch(git.branch.as_deref())
            .or_else(|| usable_branch(r.branch_hint.as_deref()))?;
        let default = self
            .default_branches
            .get(&repo_root)
            .and_then(|(branch, _)| branch.as_deref());
        if default == Some(branch) {
            return None;
        }
        Some(PrLookup::Branch {
            repo_root,
            branch: branch.to_string(),
        })
    }

    /// True when the bound PR is open and was opened from `lookup`'s branch:
    /// looking that branch up would find the same PR.
    fn binding_covers(&self, binding: Option<&PrLookup>, lookup: &PrLookup) -> bool {
        let PrLookup::Branch { branch, .. } = lookup else {
            return false;
        };
        binding
            .and_then(|binding| self.prs.get(binding))
            .is_some_and(|entry| {
                entry.pr.as_ref().is_some_and(is_open)
                    && entry.head.as_deref() == Some(branch.as_str())
            })
    }

    /// Lookups in the order to try them: forced ones first, then the ones
    /// that waited longest, so a lookup that keeps failing cannot always go
    /// first.
    fn fetch_order(&self, plans: &[LookupPlan]) -> Vec<PrLookup> {
        let mut order: Vec<PrLookup> = Vec::new();
        for lookup in plans.iter().flat_map(LookupPlan::iter) {
            if !order.contains(lookup) {
                order.push(lookup.clone());
            }
        }
        order.sort_by_key(|lookup| {
            (
                !self.force_pr.contains(lookup),
                self.prs.get(lookup).map(|entry| entry.fetched_at),
            )
        });
        order
    }

    /// When a lookup is next due; None if it has never run.
    fn pr_due_at(&self, lookup: &PrLookup, poll: Duration) -> Option<Instant> {
        let entry = self.prs.get(lookup)?;
        let interval = if self.force_pr.contains(lookup) {
            PR_MIN_INTERVAL
        } else {
            pr_interval(entry.pr.as_ref(), poll)
        };
        Some(entry.fetched_at + interval)
    }

    /// True while a back-off holds this lookup back.
    fn blocked(&self, lookup: &PrLookup, now: Instant) -> bool {
        if self.bypass_backoff.contains(lookup) {
            return false;
        }
        self.gh_backoff_until.is_some_and(|until| now < until)
            || self
                .lookup_backoff
                .get(lookup)
                .is_some_and(|backoff| now < backoff.until)
    }

    /// Runs one lookup and stores its answer, or keeps the previous one when
    /// it failed.
    fn run_lookup(&mut self, lookup: &PrLookup) {
        let result = self.fetch_pr(lookup);
        let previous = self.prs.remove(lookup);
        let (pr, head) = match result {
            Ok(found) => {
                self.lookup_backoff.remove(lookup);
                match found {
                    Some((pr, head)) => (Some(pr), head),
                    None => (None, None),
                }
            }
            Err(failure) => {
                match failure {
                    GhFailure::Global => self.gh_failed(),
                    GhFailure::Lookup => self.lookup_failed(lookup),
                    GhFailure::Transient => {}
                }
                previous.map(|entry| (entry.pr, entry.head)).unwrap_or_default()
            }
        };
        self.prs.insert(
            lookup.clone(),
            PrEntry {
                pr,
                head,
                fetched_at: Instant::now(),
            },
        );
        self.force_pr.remove(lookup);
        self.bypass_backoff.remove(lookup);
    }

    /// Runs `gh` for one lookup.
    fn fetch_pr(&mut self, lookup: &PrLookup) -> Result<Found, GhFailure> {
        let (args, dir, source): (Vec<&str>, Option<&Path>, &str) = match lookup {
            PrLookup::Url(url) => (
                vec!["pr", "view", url.as_str(), "--json", PR_FIELDS],
                None,
                SOURCE_DESKTOP,
            ),
            PrLookup::Branch { repo_root, branch } => (
                vec![
                    "pr",
                    "list",
                    "--head",
                    branch.as_str(),
                    "--state",
                    "all",
                    "--limit",
                    "1",
                    "--json",
                    PR_FIELDS,
                ],
                Some(Path::new(repo_root.as_str())),
                SOURCE_BRANCH,
            ),
        };
        if dir.is_some_and(|dir| !dir.is_dir()) {
            return Ok(None);
        }
        match self.runner.run("gh", &args, dir, &self.shared.shutdown) {
            Err(RunError::NotInstalled) => Err(GhFailure::Global),
            Err(RunError::TimedOut) => Err(GhFailure::Lookup),
            Err(RunError::Cancelled | RunError::Io(_)) => Err(GhFailure::Transient),
            Ok(out) if out.success => match parse_gh_pr(&out.stdout, source, now_ms()) {
                Ok(found) => {
                    self.gh_succeeded();
                    Ok(found)
                }
                Err(_) => Err(GhFailure::Transient),
            },
            Ok(out) => match classify_gh_error(&out.stderr) {
                GhError::NoRepo => Ok(None),
                GhError::NotFound => {
                    self.gh_succeeded();
                    Ok(None)
                }
                error => Err(failure_scope(error, lookup)),
            },
        }
    }

    fn gh_failed(&mut self) {
        self.gh_ok = Some(false);
        self.gh_backoff_until = Some(Instant::now() + GH_BACKOFF);
    }

    fn gh_succeeded(&mut self) {
        self.gh_ok = Some(true);
        self.gh_backoff_until = None;
    }

    /// Holds one lookup back; the others keep their pace and `gh_ok` stays.
    fn lookup_failed(&mut self, lookup: &PrLookup) {
        let failures = self
            .lookup_backoff
            .get(lookup)
            .map_or(0, |backoff| backoff.failures)
            .saturating_add(1);
        let delay = LOOKUP_BACKOFF
            .checked_mul(1 << failures.saturating_sub(1).min(4))
            .unwrap_or(LOOKUP_BACKOFF_MAX)
            .min(LOOKUP_BACKOFF_MAX);
        self.lookup_backoff.insert(
            lookup.clone(),
            Backoff {
                until: Instant::now() + delay,
                failures,
            },
        );
    }

    /// Forgets folders, repositories and lookups no tracked session uses.
    fn prune(&mut self, requests: &[GitPrRequest], plans: &[LookupPlan]) {
        let cwds: HashSet<&str> = requests.iter().map(|r| r.cwd.as_str()).collect();
        self.git.retain(|cwd, _| cwds.contains(cwd.as_str()));
        self.force_git.retain(|cwd| cwds.contains(cwd.as_str()));
        let roots: HashSet<String> = self
            .git
            .values()
            .filter_map(|entry| entry.info.as_ref()?.repo_root.clone())
            .collect();
        self.default_branches.retain(|root, _| roots.contains(root));
        let used: HashSet<&PrLookup> = plans.iter().flat_map(LookupPlan::iter).collect();
        self.prs.retain(|lookup, _| used.contains(lookup));
        self.force_pr.retain(|lookup| used.contains(lookup));
        self.bypass_backoff.retain(|lookup| used.contains(lookup));
        self.lookup_backoff.retain(|lookup, _| used.contains(lookup));
    }

    fn result_for(&self, r: &GitPrRequest, plan: &LookupPlan, now: i64) -> GitPrResult {
        let git = self.git.get(&r.cwd).and_then(|entry| entry.info.clone());
        let found = |lookup: Option<&PrLookup>| {
            lookup
                .and_then(|lookup| self.prs.get(lookup))
                .and_then(|entry| entry.pr.clone())
        };
        let branch_pr = found(plan.branch.as_ref());
        let bound_pr = found(plan.binding.as_ref()).or_else(|| {
            r.desktop_pr
                .as_ref()
                .and_then(|binding| pr_from_binding(binding, now))
        });
        (git, choose_pr(branch_pr, bound_pr))
    }

    /// Stores the current results and calls `on_change` if any of them (or
    /// `gh_ok`) changed. A result equal to the stored one but for its check
    /// time is not stored, so an unchanged session never looks new.
    fn publish(&self, requests: &[GitPrRequest], plans: &[LookupPlan]) {
        let now = now_ms();
        let fresh: Vec<(String, GitPrResult)> = requests
            .iter()
            .zip(plans)
            .map(|(r, plan)| (r.key.clone(), self.result_for(r, plan, now)))
            .collect();
        let changed = {
            let mut st = self.shared.lock();
            let tracked: HashSet<String> = st.requests.iter().map(|r| r.key.clone()).collect();
            let mut changed = false;
            for (key, result) in fresh {
                if !tracked.contains(&key) {
                    continue;
                }
                let differs = match st.results.get(&key) {
                    Some(old) if same_result(old, &result) => continue,
                    Some(_) => true,
                    None => result.0.is_some() || result.1.is_some(),
                };
                changed |= differs;
                st.results.insert(key, result);
            }
            if st.gh_ok != self.gh_ok {
                st.gh_ok = self.gh_ok;
                changed = true;
            }
            changed
        };
        if changed {
            let on_change = Arc::clone(&self.on_change);
            let _ = catch_unwind(AssertUnwindSafe(|| on_change()));
        }
    }

    /// Time until the next folder, file check or lookup is due.
    fn next_wait(&self, requests: &[GitPrRequest], plans: &[LookupPlan], poll: Duration) -> Duration {
        let now = Instant::now();
        let active = active_cwds(requests);
        let mut next = now + MAX_IDLE_WAIT;
        for (cwd, entry) in &self.git {
            next = next.min(entry.fetched_at + git_interval(entry, active.contains(cwd.as_str())));
            if self.force_git.contains(cwd) {
                next = next.min(entry.fetched_at + GIT_FORCE_MIN);
            }
            if entry.stamp.is_some() {
                let check = (entry.stamp_checked_at + GIT_STAMP_CHECK)
                    .max(entry.fetched_at + GIT_STAMP_MIN_AGE);
                next = next.min(check);
            }
        }
        for lookup in plans.iter().flat_map(LookupPlan::iter) {
            let mut due = self.pr_due_at(lookup, poll).unwrap_or(now);
            if !self.bypass_backoff.contains(lookup) {
                if let Some(until) = self.gh_backoff_until {
                    due = due.max(until);
                }
                if let Some(backoff) = self.lookup_backoff.get(lookup) {
                    due = due.max(backoff.until);
                }
            }
            next = next.min(due);
        }
        next.saturating_duration_since(now).max(MIN_WAIT)
    }
}

/// Folders where a session is running a turn.
fn active_cwds(requests: &[GitPrRequest]) -> HashSet<&str> {
    requests
        .iter()
        .filter(|r| r.working)
        .map(|r| r.cwd.as_str())
        .collect()
}

/// Polling interval of a folder: fast while one of its sessions works,
/// slow otherwise and for folders outside git.
fn git_interval(entry: &GitEntry, active: bool) -> Duration {
    if active && entry.layout.is_some() {
        GIT_REFRESH_ACTIVE
    } else {
        GIT_REFRESH_IDLE
    }
}

/// Stamps of HEAD, the index, packed-refs and the current branch's ref.
/// None when the git folders are unknown.
fn read_stamp(layout: &RepoLayout, branch: Option<&str>) -> Option<Stamp> {
    let git_dir = layout.git_dir.as_deref()?;
    let common = layout.common_dir.as_deref().unwrap_or(git_dir);
    let mut files = vec![
        git_dir.join("HEAD"),
        git_dir.join("index"),
        common.join("packed-refs"),
    ];
    if let Some(branch) = usable_branch(branch) {
        files.push(common.join("refs").join("heads").join(branch));
    }
    Some(
        files
            .iter()
            .map(|file| {
                let meta = std::fs::metadata(file).ok()?;
                Some((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len()))
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Child processes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct CmdOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunError {
    /// The program is not installed (or not on PATH).
    NotInstalled,
    TimedOut,
    /// The service is shutting down.
    Cancelled,
    Io(String),
}

/// Runs one command. Replaced by a fake in tests.
trait Runner: Send {
    fn run(
        &mut self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        cancel: &AtomicBool,
    ) -> Result<CmdOutput, RunError>;
}

/// Runs the real programs. `git` is resolved once to Git for Windows' real
/// `git.exe`, skipping the `cmd\git.exe` launcher: one process less per
/// command, and a kill reaches the process that holds the pipes.
#[derive(Default)]
struct ProcessRunner {
    git: Option<GitProgram>,
}

#[derive(Debug, Clone, PartialEq)]
struct GitProgram {
    exe: PathBuf,
    /// Folder of the real git, put first in `gh`'s PATH so that its own git
    /// calls skip the launcher too.
    bin_dir: Option<PathBuf>,
}

impl ProcessRunner {
    fn git_program(&mut self, cancel: &AtomicBool) -> GitProgram {
        if let Some(git) = &self.git {
            return git.clone();
        }
        let resolved = if cfg!(windows) {
            run_process("git", &["--exec-path"], None, cancel, COMMAND_TIMEOUT, None)
                .ok()
                .filter(|out| out.success)
                .and_then(|out| real_git_from_exec_path(&out.stdout))
        } else {
            None
        };
        let program = match resolved {
            Some(exe) => GitProgram {
                bin_dir: exe.parent().map(Path::to_path_buf),
                exe,
            },
            None => GitProgram {
                exe: PathBuf::from("git"),
                bin_dir: None,
            },
        };
        // Resolved again next time if shutdown interrupted it.
        if !cancel.load(Ordering::SeqCst) {
            self.git = Some(program.clone());
        }
        program
    }
}

impl Runner for ProcessRunner {
    fn run(
        &mut self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        cancel: &AtomicBool,
    ) -> Result<CmdOutput, RunError> {
        match program {
            "git" => {
                let git = self.git_program(cancel);
                run_process(&git.exe, args, cwd, cancel, COMMAND_TIMEOUT, None)
            }
            "gh" => {
                let git = self.git_program(cancel);
                let prefix = git.bin_dir.as_deref();
                run_process(program, args, cwd, cancel, COMMAND_TIMEOUT, prefix)
            }
            _ => run_process(program, args, cwd, cancel, COMMAND_TIMEOUT, None),
        }
    }
}

/// Real `git.exe` from `git --exec-path`
/// (`<prefix>/mingw64/libexec/git-core` -> `<prefix>\mingw64\bin\git.exe`).
fn real_git_from_exec_path(output: &str) -> Option<PathBuf> {
    let exec = PathBuf::from(native_path(output.lines().next()?.trim()));
    let exe = exec.parent()?.parent()?.join("bin").join("git.exe");
    exe.is_file().then_some(exe)
}

/// Runs a program without a console window, stdin closed, and kills it with
/// every process it started after `timeout` or when `cancel` is set.
/// `path_prefix` is put first in the child's PATH.
fn run_process(
    program: impl AsRef<OsStr>,
    args: &[&str],
    cwd: Option<&Path>,
    cancel: &AtomicBool,
    timeout: Duration,
    path_prefix: Option<&Path>,
) -> Result<CmdOutput, RunError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_SPINNER_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        // Keeps `git status` from taking index.lock under the sessions' feet.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("NO_COLOR", "1");
    if let Some(prefix) = path_prefix {
        let mut path = OsString::from(prefix.as_os_str());
        if let Some(current) = std::env::var_os("PATH") {
            path.push(if cfg!(windows) { ";" } else { ":" });
            path.push(current);
        }
        command.env("PATH", path);
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => RunError::NotInstalled,
        _ => RunError::Io(e.to_string()),
    })?;
    let tree = ProcessTree::adopt(&child);
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let mut pause = Duration::from_millis(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                kill(&mut child, tree.as_ref());
                return Err(RunError::Io(e.to_string()));
            }
        }
        if cancel.load(Ordering::SeqCst) {
            kill(&mut child, tree.as_ref());
            return Err(RunError::Cancelled);
        }
        if Instant::now() >= deadline {
            kill(&mut child, tree.as_ref());
            return Err(RunError::TimedOut);
        }
        thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(50));
    };
    Ok(CmdOutput {
        success: status.success(),
        stdout: collect(stdout, tree.as_ref()),
        stderr: collect(stderr, tree.as_ref()),
    })
}

/// Ends the child and everything it started.
fn kill(child: &mut Child, tree: Option<&ProcessTree>) {
    if let Some(tree) = tree {
        tree.terminate();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads a pipe to the end on its own thread so the child never blocks on a
/// full pipe.
fn drain<R: Read + Send + 'static>(mut pipe: R) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("git-pr-pipe".into())
        .spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    rx
}

/// What a reader got. When a process the child started still holds the pipe
/// after the grace period, the whole tree is ended so the reader is released.
fn collect(rx: Option<mpsc::Receiver<Vec<u8>>>, tree: Option<&ProcessTree>) -> String {
    let Some(rx) = rx else {
        return String::new();
    };
    let bytes = rx.recv_timeout(PIPE_GRACE).ok().or_else(|| {
        tree?.terminate();
        rx.recv_timeout(PIPE_GRACE).ok()
    });
    bytes
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// A Windows job object holding a child and every process it starts, so a
/// timeout can end them all. Closing it leaves them running.
#[cfg(windows)]
struct ProcessTree(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl ProcessTree {
    fn adopt(child: &Child) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW,
        };
        // SAFETY: null attributes and name create an anonymous job; the
        // handle is owned (and closed) by the value returned.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return None;
        }
        let tree = Self(job);
        // SAFETY: both handles are valid for the duration of the call.
        let assigned = unsafe { AssignProcessToJobObject(tree.0, child.as_raw_handle()) };
        (assigned != 0).then_some(tree)
    }

    fn terminate(&self) {
        // SAFETY: valid job handle owned by `self`.
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and is closed once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(not(windows))]
struct ProcessTree;

#[cfg(not(windows))]
impl ProcessTree {
    fn adopt(_child: &Child) -> Option<Self> {
        None
    }

    fn terminate(&self) {}
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Why a `gh` call failed, from its stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GhError {
    /// Not a repository, or no GitHub remote: not a `gh` failure.
    NoRepo,
    /// GitHub answered that there is no such PR or repository.
    NotFound,
    /// API rate limit reached.
    RateLimit,
    /// No access to this repository: permissions, SAML/SSO enforcement.
    Forbidden,
    Auth,
    Network,
    Other,
}

fn classify_gh_error(stderr: &str) -> GhError {
    let text = stderr.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));
    // Checked first: the "no GitHub remote" message also mentions `gh auth login`.
    if has(&[
        "no git remotes found",
        "none of the git remotes",
        "not a git repository",
        "failed to run git",
        "could not determine base repo",
        "unable to determine base repo",
        "no default remote repository",
    ]) {
        GhError::NoRepo
    } else if has(&[
        "no pull requests found",
        "could not resolve to a pullrequest",
        "could not resolve to a repository",
        "could not resolve to a node",
        "http 404",
    ]) {
        GhError::NotFound
    } else if has(&["rate limit", "http 429"]) {
        // Before 403: GitHub also answers a rate limit with a 403.
        GhError::RateLimit
    } else if has(&[
        "saml",
        "http 403",
        "resource not accessible",
        "must have push access",
        "forbidden",
    ]) {
        GhError::Forbidden
    } else if has(&[
        "gh auth login",
        "not logged in",
        "not logged into",
        "authentication",
        "bad credentials",
        "http 401",
        "token has expired",
    ]) {
        GhError::Auth
    } else if has(&[
        "http 5",
        "http 408",
        "error connecting",
        "dial tcp",
        "could not resolve host",
        "no such host",
        "timeout",
        "timed out",
        "connection refused",
        "connection reset",
        "tls handshake",
        "proxyconnect",
        "network is unreachable",
        "x509",
    ]) {
        GhError::Network
    } else {
        GhError::Other
    }
}

/// Whether a `gh` error stops every lookup or only this one. A login error
/// about another host than github.com (GitHub Enterprise) concerns only the
/// lookups of that host.
fn failure_scope(error: GhError, lookup: &PrLookup) -> GhFailure {
    match error {
        GhError::Forbidden => GhFailure::Lookup,
        GhError::Auth
            if lookup_host(lookup).is_some_and(|host| !host.eq_ignore_ascii_case("github.com")) =>
        {
            GhFailure::Lookup
        }
        GhError::Auth | GhError::Network | GhError::RateLimit => GhFailure::Global,
        GhError::NoRepo | GhError::NotFound | GhError::Other => GhFailure::Transient,
    }
}

/// Host of a URL lookup.
fn lookup_host(lookup: &PrLookup) -> Option<&str> {
    let PrLookup::Url(url) = lookup else {
        return None;
    };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, rest)| rest);
    rest.split('/').next().filter(|host| !host.is_empty())
}

/// Parses `gh pr view --json` (an object) or `gh pr list --json` (an array).
/// `Ok(None)` when the list is empty.
fn parse_gh_pr(stdout: &str, source: &str, checked_at: i64) -> Result<Found, String> {
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|e| e.to_string())?;
    let item = match &value {
        Value::Array(items) => match items.first() {
            None => return Ok(None),
            Some(item) => item,
        },
        Value::Object(_) => &value,
        _ => return Err("unexpected JSON from gh".into()),
    };
    let pr = pr_from_json(item, source, checked_at).ok_or("PR without a number")?;
    Ok(Some((pr, str_field(item, "headRefName"))))
}

fn pr_from_json(value: &Value, source: &str, checked_at: i64) -> Option<PrInfo> {
    let number = json_u32(value.get("number")?)?;
    let url = str_field(value, "url").unwrap_or_default();
    Some(PrInfo {
        number,
        repository: repository_from_url(&url),
        url,
        title: str_field(value, "title"),
        state: str_field(value, "state")
            .map(|state| state.to_ascii_uppercase())
            .unwrap_or_else(|| "UNKNOWN".into()),
        is_draft: value
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        review_decision: str_field(value, "reviewDecision"),
        checks: rollup_state(value.get("statusCheckRollup")),
        merge_state: str_field(value, "mergeStateStatus").filter(|state| state != "UNKNOWN"),
        checked_at,
        source: source.to_string(),
    })
}

/// A trimmed, non-empty string field.
fn str_field(value: &Value, key: &str) -> Option<String> {
    let text = value.get(key)?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn json_u32(value: &Value) -> Option<u32> {
    match value {
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// "owner/name" from `https://<host>/<owner>/<name>/pull/<n>`.
fn repository_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let mut parts = rest.split('/');
    let _host = parts.next()?;
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{name}"))
}

/// PR number from `.../pull/<n>`.
fn number_from_url(url: &str) -> Option<u32> {
    let (_, tail) = url.split_once("/pull/")?;
    tail.split(['/', '?', '#']).next()?.parse().ok()
}

/// Overall CI state of a `statusCheckRollup` array.
fn rollup_state(rollup: Option<&Value>) -> ChecksState {
    let Some(Value::Array(entries)) = rollup else {
        return ChecksState::None;
    };
    let mut pending = false;
    let mut passed = false;
    for entry in entries {
        match check_entry_state(entry) {
            Some(ChecksState::Fail) => return ChecksState::Fail,
            Some(ChecksState::Pending) => pending = true,
            Some(ChecksState::Pass) => passed = true,
            _ => {}
        }
    }
    if pending {
        ChecksState::Pending
    } else if passed {
        ChecksState::Pass
    } else {
        ChecksState::None
    }
}

/// State of one rollup entry: a CheckRun (`status` + `conclusion`) or a
/// StatusContext (`state`). None when it says nothing usable.
fn check_entry_state(entry: &Value) -> Option<ChecksState> {
    let upper = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
    };
    let typename = entry.get("__typename").and_then(Value::as_str).unwrap_or("");
    let is_status_context = typename == "StatusContext"
        || (typename != "CheckRun" && entry.get("state").is_some() && entry.get("status").is_none());
    if is_status_context {
        return upper("state").and_then(|state| classify_check_word(&state));
    }
    let conclusion = upper("conclusion").and_then(|c| classify_check_word(&c));
    if conclusion == Some(ChecksState::Fail) {
        return conclusion;
    }
    match upper("status") {
        Some(status) if status != "COMPLETED" => Some(ChecksState::Pending),
        _ => conclusion,
    }
}

fn classify_check_word(word: &str) -> Option<ChecksState> {
    match word {
        "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
        | "ERROR" => Some(ChecksState::Fail),
        "QUEUED" | "IN_PROGRESS" | "PENDING" | "WAITING" | "EXPECTED" | "REQUESTED" => {
            Some(ChecksState::Pending)
        }
        "SUCCESS" | "NEUTRAL" | "SKIPPED" => Some(ChecksState::Pass),
        _ => None,
    }
}

/// Branch name from `git symbolic-ref --short refs/remotes/origin/HEAD`
/// ("origin/main" -> "main").
fn parse_default_branch(output: &str) -> Option<String> {
    let line = output.lines().next()?.trim();
    let line = line.strip_prefix("refs/remotes/").unwrap_or(line);
    let branch = line.split_once('/').map_or(line, |(_, branch)| branch);
    (!branch.is_empty()).then(|| branch.to_string())
}

/// Default branch read from the repository files: `refs/remotes/origin/HEAD`,
/// else `main` or `master` when it exists. The outer None means the files
/// cannot tell (reftable storage, unknown folder): ask git.
fn default_branch_from_files(common_dir: &Path) -> Option<Option<String>> {
    if !common_dir.is_dir() || common_dir.join("reftable").exists() {
        return None;
    }
    let origin_head = common_dir.join("refs").join("remotes").join("origin").join("HEAD");
    if let Ok(text) = std::fs::read_to_string(origin_head)
        && let Some(target) = text.trim().strip_prefix("ref:")
        && let Some(branch) = parse_default_branch(target.trim())
    {
        return Some(Some(branch));
    }
    let packed = std::fs::read_to_string(common_dir.join("packed-refs")).unwrap_or_default();
    for candidate in ["main", "master"] {
        let reference = format!("refs/heads/{candidate}");
        let loose = common_dir.join("refs").join("heads").join(candidate).is_file();
        let in_packed = packed
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some(reference.as_str()));
        if loose || in_packed {
            return Some(Some(candidate.to_string()));
        }
    }
    Some(None)
}

/// Work tree root, git folder and common git folder from `git rev-parse
/// --path-format=absolute --show-toplevel --git-dir --git-common-dir`.
fn parse_layout(output: &str) -> Option<RepoLayout> {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let [top, git_dir, common_dir] = lines.as_slice() else {
        return None;
    };
    let top = native_path(top);
    let git_dir = PathBuf::from(native_path(git_dir));
    let common_dir = PathBuf::from(native_path(common_dir));
    let absolute = Path::new(&top).is_absolute() && git_dir.is_absolute() && common_dir.is_absolute();
    absolute.then_some(RepoLayout {
        top,
        git_dir: Some(git_dir),
        common_dir: Some(common_dir),
    })
}

/// Branch and uncommitted entries from `git status --porcelain=v2 --branch`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StatusV2 {
    /// "HEAD" for a detached HEAD, as `git rev-parse --abbrev-ref` says.
    branch: Option<String>,
    dirty: u32,
}

fn parse_status_v2(output: &str) -> StatusV2 {
    let mut status = StatusV2::default();
    let mut dirty: u64 = 0;
    for line in output.lines().map(|line| line.trim_end_matches('\r')) {
        if let Some(header) = line.strip_prefix("# ") {
            if let Some(head) = header.strip_prefix("branch.head ") {
                status.branch = match head.trim() {
                    "" => None,
                    "(detached)" => Some("HEAD".to_string()),
                    name => Some(name.to_string()),
                };
            }
        } else if !line.trim().is_empty() {
            dirty += 1;
        }
    }
    status.dirty = u32::try_from(dirty).unwrap_or(u32::MAX);
    status
}

/// A branch worth looking up: not empty and not a detached HEAD.
fn usable_branch(branch: Option<&str>) -> Option<&str> {
    branch
        .map(str::trim)
        .filter(|branch| !branch.is_empty() && *branch != "HEAD")
}

/// Git prints forward slashes; the rest of the app uses native paths.
fn native_path(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.to_string()
    }
}

/// URL to give `gh pr view` for a Desktop binding.
fn binding_url(binding: &DesktopPrBinding) -> Option<String> {
    if let Some(url) = binding.url.as_deref().map(str::trim)
        && !url.is_empty()
    {
        return Some(url.to_string());
    }
    let repository = binding.repository.as_deref().map(str::trim)?;
    let number = binding.number?;
    (!repository.is_empty()).then(|| format!("https://github.com/{repository}/pull/{number}"))
}

/// What the window shows for a Desktop binding when `gh` cannot tell more.
fn pr_from_binding(binding: &DesktopPrBinding, checked_at: i64) -> Option<PrInfo> {
    let url = binding_url(binding).unwrap_or_default();
    let number = binding.number.or_else(|| number_from_url(&url))?;
    let repository = binding
        .repository
        .as_deref()
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .map(str::to_string)
        .or_else(|| repository_from_url(&url));
    let state = binding
        .state
        .as_deref()
        .map(str::trim)
        .filter(|state| !state.is_empty())
        .map_or_else(|| "UNKNOWN".to_string(), str::to_ascii_uppercase);
    Some(PrInfo {
        number,
        url,
        title: None,
        repository,
        state,
        is_draft: false,
        review_decision: None,
        checks: ChecksState::None,
        merge_state: None,
        checked_at,
        source: SOURCE_DESKTOP.to_string(),
    })
}

fn is_open(pr: &PrInfo) -> bool {
    pr.state.eq_ignore_ascii_case("OPEN")
}

/// PR shown for a session: an open PR of the branch checked out, else the
/// open PR Desktop bound, else whichever exists (the branch's first). An old
/// merged PR Desktop remembers never hides the open PR of the current branch.
fn choose_pr(branch: Option<PrInfo>, bound: Option<PrInfo>) -> Option<PrInfo> {
    match (branch, bound) {
        (Some(branch), _) if is_open(&branch) => Some(branch),
        (_, Some(bound)) if is_open(&bound) => Some(bound),
        (branch, bound) => branch.or(bound),
    }
}

/// Polling interval of a PR: slower once it is merged or closed.
fn pr_interval(pr: Option<&PrInfo>, poll: Duration) -> Duration {
    let poll = poll.max(PR_MIN_INTERVAL);
    match pr.map(|pr| pr.state.as_str()) {
        Some("MERGED" | "CLOSED") => poll.max(PR_CLOSED_INTERVAL),
        _ => poll,
    }
}

/// Equal results, `checked_at` aside.
fn same_result(a: &GitPrResult, b: &GitPrResult) -> bool {
    without_times(a) == without_times(b)
}

fn without_times(result: &GitPrResult) -> GitPrResult {
    let (mut git, mut pr) = result.clone();
    if let Some(git) = git.as_mut() {
        git.checked_at = 0;
    }
    if let Some(pr) = pr.as_mut() {
        pr.checked_at = 0;
    }
    (git, pr)
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const VIEW_FAILING: &str = include_str!("../tests/fixtures/git_pr/pr_view_open_failing.json");
    const LIST_PENDING: &str = include_str!("../tests/fixtures/git_pr/pr_list_draft_pending.json");
    const VIEW_MERGED: &str = include_str!("../tests/fixtures/git_pr/pr_view_merged_passing.json");
    const LIST_EMPTY: &str = include_str!("../tests/fixtures/git_pr/pr_list_empty.json");

    fn parsed(stdout: &str, source: &str, checked_at: i64) -> PrInfo {
        parse_gh_pr(stdout, source, checked_at)
            .expect("valid JSON")
            .expect("a PR")
            .0
    }

    // -- pure helpers ------------------------------------------------------

    #[test]
    fn parses_view_with_failing_checks() {
        let pr = parsed(VIEW_FAILING, SOURCE_DESKTOP, 5);
        assert_eq!(pr.number, 42);
        assert_eq!(pr.url, "https://github.com/example-org/example-repo/pull/42");
        assert_eq!(pr.repository.as_deref(), Some("example-org/example-repo"));
        assert_eq!(pr.title.as_deref(), Some("fix: un titre synthétique avec des accents"));
        assert_eq!(pr.state, "OPEN");
        assert!(!pr.is_draft);
        assert_eq!(pr.review_decision, None);
        assert_eq!(pr.checks, ChecksState::Fail);
        assert_eq!(pr.merge_state.as_deref(), Some("UNSTABLE"));
        assert_eq!(pr.checked_at, 5);
        assert_eq!(pr.source, "desktop");
    }

    #[test]
    fn parses_list_with_pending_checks() {
        let pr = parsed(LIST_PENDING, SOURCE_BRANCH, 1);
        assert_eq!(pr.number, 7);
        assert!(pr.is_draft);
        assert_eq!(pr.review_decision.as_deref(), Some("REVIEW_REQUIRED"));
        assert_eq!(pr.checks, ChecksState::Pending);
        assert_eq!(pr.merge_state.as_deref(), Some("BLOCKED"));
        assert_eq!(pr.source, "gh-branch");
    }

    #[test]
    fn parses_merged_pr_and_drops_unknown_merge_state() {
        let pr = parsed(VIEW_MERGED, SOURCE_DESKTOP, 1);
        assert_eq!(pr.state, "MERGED");
        assert_eq!(pr.review_decision.as_deref(), Some("APPROVED"));
        assert_eq!(pr.checks, ChecksState::Pass);
        assert_eq!(pr.merge_state, None);
        assert_eq!(pr.repository.as_deref(), Some("example-org/other-repo"));
    }

    #[test]
    fn keeps_the_branch_of_the_pr() {
        let head = |stdout: &str| {
            parse_gh_pr(stdout, SOURCE_BRANCH, 1)
                .expect("valid JSON")
                .expect("a PR")
                .1
        };
        assert_eq!(head(VIEW_FAILING).as_deref(), Some("fix/perimetre"));
        assert_eq!(head(LIST_PENDING).as_deref(), Some("feature/x"));
        assert_eq!(head(r#"{"number":2}"#), None);
        assert_eq!(head(r#"{"number":2,"headRefName":" "}"#), None);
    }

    #[test]
    fn empty_list_means_no_pr() {
        assert_eq!(parse_gh_pr(LIST_EMPTY, SOURCE_BRANCH, 1), Ok(None));
    }

    #[test]
    fn bad_json_is_an_error_not_a_panic() {
        assert!(parse_gh_pr("", SOURCE_BRANCH, 1).is_err());
        assert!(parse_gh_pr("{\"number\":4", SOURCE_BRANCH, 1).is_err());
        assert!(parse_gh_pr("\"text\"", SOURCE_BRANCH, 1).is_err());
        assert!(parse_gh_pr("{\"title\":\"no number\"}", SOURCE_BRANCH, 1).is_err());
        assert!(parse_gh_pr("[{\"number\":-3}]", SOURCE_BRANCH, 1).is_err());
    }

    #[test]
    fn tolerates_wrong_types_and_missing_fields() {
        let pr = parsed(
            r#"{"number":"15","isDraft":"yes","state":"open","statusCheckRollup":{"x":1},"title":3,"headRefName":4}"#,
            SOURCE_BRANCH,
            1,
        );
        assert_eq!(pr.number, 15);
        assert!(!pr.is_draft);
        assert_eq!(pr.state, "OPEN");
        assert_eq!(pr.title, None);
        assert_eq!(pr.url, "");
        assert_eq!(pr.repository, None);
        assert_eq!(pr.checks, ChecksState::None);
        let bare = parsed(r#"{"number":2}"#, SOURCE_BRANCH, 1);
        assert_eq!(bare.state, "UNKNOWN");
        assert_eq!(bare.merge_state, None);
    }

    #[test]
    fn rollup_mapping() {
        let rollup = |json: &str| {
            let value: Value = serde_json::from_str(json).expect("test JSON");
            rollup_state(Some(&value))
        };
        assert_eq!(rollup("[]"), ChecksState::None);
        assert_eq!(rollup_state(None), ChecksState::None);
        assert_eq!(rollup("null"), ChecksState::None);
        assert_eq!(
            rollup(
                r#"[{"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
                    {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SKIPPED"},
                    {"__typename":"CheckRun","status":"COMPLETED","conclusion":"NEUTRAL"},
                    {"__typename":"StatusContext","state":"SUCCESS"}]"#
            ),
            ChecksState::Pass
        );
        assert_eq!(
            rollup(
                r#"[{"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":""},
                    {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"}]"#
            ),
            ChecksState::Pending
        );
        assert_eq!(
            rollup(
                r#"[{"__typename":"CheckRun","status":"QUEUED","conclusion":""},
                    {"__typename":"CheckRun","status":"COMPLETED","conclusion":"TIMED_OUT"}]"#
            ),
            ChecksState::Fail
        );
        assert_eq!(
            rollup(r#"[{"__typename":"StatusContext","state":"ERROR"}]"#),
            ChecksState::Fail
        );
        assert_eq!(
            rollup(r#"[{"__typename":"StatusContext","state":"EXPECTED"}]"#),
            ChecksState::Pending
        );
        assert_eq!(
            rollup(r#"[{"state":"PENDING"},{"status":"COMPLETED","conclusion":"SUCCESS"}]"#),
            ChecksState::Pending
        );
        assert_eq!(
            rollup(r#"[{"__typename":"CheckRun","status":"COMPLETED","conclusion":"STARTUP_FAILURE"}]"#),
            ChecksState::Fail
        );
        assert_eq!(
            rollup(r#"[{"__typename":"CheckRun","status":"COMPLETED","conclusion":"STALE"}, 4, "x"]"#),
            ChecksState::None
        );
    }

    #[test]
    fn default_branch_parsing() {
        assert_eq!(parse_default_branch("origin/main\n").as_deref(), Some("main"));
        assert_eq!(
            parse_default_branch("origin/release/2.x\r\n").as_deref(),
            Some("release/2.x")
        );
        assert_eq!(
            parse_default_branch("refs/remotes/origin/develop").as_deref(),
            Some("develop")
        );
        assert_eq!(parse_default_branch("trunk").as_deref(), Some("trunk"));
        assert_eq!(parse_default_branch(""), None);
        assert_eq!(parse_default_branch("origin/"), None);
    }

    #[test]
    fn status_v2_parsing() {
        let out = "# branch.oid 1234abcd\n# branch.head feat/6-lot\n\
                   # branch.upstream origin/feat/6-lot\n# branch.ab +1 -0\n\
                   1 .M N... 100644 100644 100644 aa bb src/a.rs\n\
                   2 R. N... 100644 100644 100644 aa bb R100 nouveau nom.txt\tancien nom.txt\r\n\
                   u UU N... 100644 100644 100644 100644 aa bb cc conflit.rs\n? notes/\n\n";
        assert_eq!(
            parse_status_v2(out),
            StatusV2 {
                branch: Some("feat/6-lot".into()),
                dirty: 4
            }
        );
        let clean = "# branch.oid (initial)\n# branch.head main\n";
        assert_eq!(
            parse_status_v2(clean),
            StatusV2 {
                branch: Some("main".into()),
                dirty: 0
            }
        );
        let detached = "# branch.oid 1234\n# branch.head (detached)\n? x\n";
        assert_eq!(parse_status_v2(detached).branch.as_deref(), Some("HEAD"));
        assert_eq!(parse_status_v2(""), StatusV2::default());
        assert_eq!(parse_status_v2(" \n").dirty, 0);
    }

    #[test]
    fn layout_parsing() {
        let out = "D:/Données Utilisateur/repo\nD:/Données Utilisateur/repo/.git/worktrees/wt\n\
                   D:/Données Utilisateur/repo/.git\n";
        if cfg!(windows) {
            let layout = parse_layout(out).expect("layout");
            assert_eq!(layout.top, r"D:\Données Utilisateur\repo");
            assert_eq!(
                layout.git_dir.as_deref(),
                Some(Path::new(r"D:\Données Utilisateur\repo\.git\worktrees\wt"))
            );
            assert_eq!(
                layout.common_dir.as_deref(),
                Some(Path::new(r"D:\Données Utilisateur\repo\.git"))
            );
        }
        // Old git: the unknown option comes back as a line, not a path.
        assert_eq!(parse_layout("--path-format=absolute\nD:/r\n.git\n.git\n"), None);
        assert_eq!(parse_layout("D:/r\n.git\n.git\n"), None);
        assert_eq!(parse_layout(""), None);
    }

    #[test]
    fn gh_error_classification() {
        use GhError::*;
        let cases = [
            ("no git remotes found", NoRepo),
            (
                "none of the git remotes configured for this repository point to a known GitHub host. To tell gh about a new GitHub host, please use `gh auth login`",
                NoRepo,
            ),
            (
                "failed to run git: fatal: not a git repository (or any of the parent directories): .git",
                NoRepo,
            ),
            ("no pull requests found for branch \"feature/x\"", NotFound),
            (
                "GraphQL: Could not resolve to a PullRequest with the number of 99999. (repository.pullRequest)",
                NotFound,
            ),
            ("To get started with GitHub CLI, please run:  gh auth login", Auth),
            ("HTTP 401: Bad credentials (https://api.github.com/graphql)", Auth),
            ("HTTP 502: Bad Gateway (https://api.github.com/graphql)", Network),
            (
                "error connecting to api.github.com\ncheck your internet connection or https://githubstatus.com",
                Network,
            ),
            ("GraphQL: API rate limit exceeded for user ID 1.", RateLimit),
            ("HTTP 403: API rate limit exceeded for 1.2.3.4.", RateLimit),
            (
                "HTTP 403: Resource not accessible by integration (https://api.github.com/graphql)",
                Forbidden,
            ),
            (
                "GraphQL: Resource protected by organization SAML enforcement. You must grant your OAuth token access to this organization.",
                Forbidden,
            ),
            ("something unexpected", Other),
            ("", Other),
        ];
        for (stderr, expected) in cases {
            assert_eq!(classify_gh_error(stderr), expected, "{stderr}");
        }
    }

    #[test]
    fn only_global_errors_stop_every_lookup() {
        let github = PrLookup::Url("https://github.com/o/r/pull/1".into());
        let enterprise = PrLookup::Url("https://ghe.example.com/o/r/pull/1".into());
        let branch = PrLookup::Branch {
            repo_root: "C:\\r".into(),
            branch: "feat/x".into(),
        };
        assert_eq!(failure_scope(GhError::Forbidden, &github), GhFailure::Lookup);
        assert_eq!(failure_scope(GhError::Auth, &github), GhFailure::Global);
        assert_eq!(failure_scope(GhError::Auth, &branch), GhFailure::Global);
        assert_eq!(failure_scope(GhError::Auth, &enterprise), GhFailure::Lookup);
        assert_eq!(failure_scope(GhError::Network, &enterprise), GhFailure::Global);
        assert_eq!(failure_scope(GhError::RateLimit, &branch), GhFailure::Global);
        assert_eq!(failure_scope(GhError::Other, &branch), GhFailure::Transient);
        assert_eq!(lookup_host(&enterprise), Some("ghe.example.com"));
        assert_eq!(lookup_host(&branch), None);
    }

    #[test]
    fn urls_and_bindings() {
        assert_eq!(
            repository_from_url("https://github.com/o/r/pull/3").as_deref(),
            Some("o/r")
        );
        assert_eq!(repository_from_url("https://github.com/"), None);
        assert_eq!(repository_from_url(""), None);
        assert_eq!(number_from_url("https://github.com/o/r/pull/31/files"), Some(31));
        assert_eq!(number_from_url("https://github.com/o/r"), None);

        let from_number = DesktopPrBinding {
            number: Some(9),
            url: None,
            repository: Some("o/r".into()),
            state: Some("merged".into()),
        };
        assert_eq!(
            binding_url(&from_number).as_deref(),
            Some("https://github.com/o/r/pull/9")
        );
        let pr = pr_from_binding(&from_number, 3).expect("a PR");
        assert_eq!(pr.number, 9);
        assert_eq!(pr.state, "MERGED");
        assert_eq!(pr.repository.as_deref(), Some("o/r"));
        assert_eq!(pr.checks, ChecksState::None);
        assert_eq!(pr.source, "desktop");

        let from_url = DesktopPrBinding {
            url: Some(" https://github.com/o/r/pull/11 ".into()),
            ..DesktopPrBinding::default()
        };
        let pr = pr_from_binding(&from_url, 3).expect("a PR");
        assert_eq!(pr.number, 11);
        assert_eq!(pr.url, "https://github.com/o/r/pull/11");
        assert_eq!(pr.repository.as_deref(), Some("o/r"));
        assert_eq!(pr.state, "UNKNOWN");

        assert_eq!(pr_from_binding(&DesktopPrBinding::default(), 3), None);
        assert_eq!(binding_url(&DesktopPrBinding::default()), None);
    }

    #[test]
    fn open_pr_of_the_branch_wins() {
        let pr = |number: u32, state: &str| PrInfo {
            number,
            state: state.into(),
            ..PrInfo::default()
        };
        let pick = |branch: Option<PrInfo>, bound: Option<PrInfo>| {
            choose_pr(branch, bound).map(|pr| pr.number)
        };
        assert_eq!(pick(Some(pr(14, "OPEN")), Some(pr(13, "MERGED"))), Some(14));
        assert_eq!(pick(Some(pr(14, "OPEN")), Some(pr(20, "OPEN"))), Some(14));
        assert_eq!(pick(Some(pr(5, "MERGED")), Some(pr(20, "OPEN"))), Some(20));
        assert_eq!(pick(Some(pr(5, "MERGED")), Some(pr(13, "MERGED"))), Some(5));
        assert_eq!(pick(None, Some(pr(13, "MERGED"))), Some(13));
        assert_eq!(pick(Some(pr(5, "CLOSED")), None), Some(5));
        assert_eq!(pick(None, None), None);
    }

    #[test]
    fn branch_filters_and_paths() {
        assert_eq!(usable_branch(Some(" feature/x ")), Some("feature/x"));
        assert_eq!(usable_branch(Some("HEAD")), None);
        assert_eq!(usable_branch(Some("  ")), None);
        assert_eq!(usable_branch(None), None);
        if cfg!(windows) {
            assert_eq!(
                native_path("D:/Données Utilisateur/repo"),
                "D:\\Données Utilisateur\\repo"
            );
        }
    }

    #[test]
    fn checked_at_alone_is_not_a_change() {
        let pr = parsed(VIEW_FAILING, SOURCE_DESKTOP, 1);
        let git = GitInfo {
            branch: Some("feature/x".into()),
            repo_root: Some("C:\\repo".into()),
            dirty_files: 2,
            is_default_branch: false,
            checked_at: 1,
        };
        let a = (Some(git.clone()), Some(pr.clone()));
        let mut b = a.clone();
        b.0.as_mut().expect("git").checked_at = 99;
        b.1.as_mut().expect("pr").checked_at = 99;
        assert!(same_result(&a, &b));
        b.0.as_mut().expect("git").dirty_files = 3;
        assert!(!same_result(&a, &b));
    }

    #[test]
    fn pr_interval_slows_down_for_closed_prs() {
        let poll = Duration::from_secs(60);
        assert_eq!(pr_interval(None, poll), poll);
        assert_eq!(pr_interval(None, Duration::from_secs(1)), PR_MIN_INTERVAL);
        let merged = PrInfo {
            state: "MERGED".into(),
            ..PrInfo::default()
        };
        assert_eq!(pr_interval(Some(&merged), poll), PR_CLOSED_INTERVAL);
    }

    #[test]
    fn service_can_be_shared_between_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GitPrService>();
    }

    // -- repository files ---------------------------------------------------

    /// Fresh scratch directory under the system temp folder, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "monitor-core-git-{tag}-{}-{nanos} é",
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

    #[test]
    fn default_branch_from_repository_files() {
        let dir = TempDir::new("default");
        let common = dir.0.join(".git");
        assert_eq!(default_branch_from_files(&common), None, "unknown folder");

        dir.write(".git/refs/remotes/origin/HEAD", "ref: refs/remotes/origin/develop\n");
        assert_eq!(default_branch_from_files(&common), Some(Some("develop".into())));

        std::fs::remove_file(common.join("refs/remotes/origin/HEAD")).expect("remove");
        assert_eq!(default_branch_from_files(&common), Some(None));
        dir.write(".git/packed-refs", "# pack-refs with: peeled\nabc refs/heads/master\n");
        assert_eq!(default_branch_from_files(&common), Some(Some("master".into())));
        dir.write(".git/refs/heads/main", "abc\n");
        assert_eq!(default_branch_from_files(&common), Some(Some("main".into())));

        std::fs::create_dir_all(common.join("reftable")).expect("reftable");
        assert_eq!(default_branch_from_files(&common), None, "reftable: ask git");
    }

    #[test]
    fn real_git_is_found_next_to_the_exec_path() {
        let dir = TempDir::new("exec");
        dir.write("Git/mingw64/libexec/git-core/git-status.exe", "");
        let exec = dir.0.join("Git/mingw64/libexec/git-core");
        let output = format!("{}\n", exec.display()).replace('\\', "/");
        assert_eq!(real_git_from_exec_path(&output), None, "no bin\\git.exe yet");
        dir.write("Git/mingw64/bin/git.exe", "");
        let found = real_git_from_exec_path(&output).expect("real git");
        assert!(found.ends_with(Path::new("mingw64").join("bin").join("git.exe")));
        assert_eq!(real_git_from_exec_path(""), None);
    }

    fn worker(respond: Arc<Respond>) -> Worker {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let runner = FakeRunner {
            respond,
            log: Arc::new(Mutex::new(Vec::new())),
        };
        Worker::new(shared, Box::new(runner), Arc::new(|| {}))
    }

    fn ago(now: Instant, secs: u64) -> Instant {
        now.checked_sub(Duration::from_secs(secs)).unwrap_or(now)
    }

    #[test]
    fn idle_folders_wait_for_a_change_of_their_files() {
        let dir = TempDir::new("stamp");
        dir.write(".git/HEAD", "ref: refs/heads/feat/x\n");
        dir.write(".git/index", "v1");
        dir.write(".git/refs/heads/feat/x", "abc\n");
        let layout = RepoLayout {
            top: dir.0.display().to_string(),
            git_dir: Some(dir.0.join(".git")),
            common_dir: Some(dir.0.join(".git")),
        };
        let mut w = worker(Arc::new(|_, _| failed("unused")));
        let now = Instant::now();
        let cwd = "C:\\repo";
        let info = GitInfo {
            branch: Some("feat/x".into()),
            ..GitInfo::default()
        };
        w.git.insert(
            cwd.into(),
            GitEntry {
                stamp: read_stamp(&layout, Some("feat/x")),
                info: Some(info),
                fetched_at: ago(now, 60),
                layout: Some(layout),
                stamp_checked_at: ago(now, 6),
            },
        );
        assert!(!w.git_due(cwd, false, now), "idle and unchanged: no read");
        assert!(w.git_due(cwd, true, now), "a working session reads every 30 s");

        // A commit or a stage changes the index or the branch ref.
        dir.write(".git/index", "v2, longer");
        assert!(!w.git_due(cwd, false, now), "files are compared every 5 s only");
        let later = now + GIT_STAMP_CHECK;
        assert!(w.git_due(cwd, false, later));

        // A recent read is not repeated for a file change.
        let entry = w.git.get_mut(cwd).expect("entry");
        entry.fetched_at = later;
        entry.stamp_checked_at = ago(later, 6);
        dir.write(".git/refs/heads/feat/x", "def, longer\n");
        assert!(!w.git_due(cwd, false, later + Duration::from_secs(5)));
        assert!(w.git_due(cwd, false, later + GIT_STAMP_MIN_AGE));

        w.force_git.insert(cwd.into());
        let entry = w.git.get_mut(cwd).expect("entry");
        entry.fetched_at = ago(now, 3);
        assert!(w.git_due(cwd, false, now), "a turn end forces a read");
        assert!(w.git_due("C:\\never-read", false, now));
    }

    #[test]
    fn folders_outside_git_are_read_slowly() {
        let mut w = worker(Arc::new(|_, _| failed("unused")));
        let now = Instant::now();
        w.git.insert("C:\\plain".into(), GitEntry::outside(ago(now, 60)));
        assert!(!w.git_due("C:\\plain", true, now));
        w.git.insert("C:\\plain".into(), GitEntry::outside(ago(now, 301)));
        assert!(w.git_due("C:\\plain", false, now));
    }

    fn git_entry(branch: &str, root: &str) -> GitEntry {
        GitEntry {
            info: Some(GitInfo {
                branch: Some(branch.into()),
                repo_root: Some(root.into()),
                ..GitInfo::default()
            }),
            layout: Some(RepoLayout {
                top: root.into(),
                git_dir: None,
                common_dir: None,
            }),
            ..GitEntry::outside(Instant::now())
        }
    }

    #[test]
    fn branch_checked_out_wins_over_a_stale_hint() {
        let mut w = worker(Arc::new(|_, _| failed("unused")));
        w.git.insert("C:\\repo".into(), git_entry("feat/6-lot", "C:\\repo"));
        w.default_branches
            .insert("C:\\repo".into(), (Some("main".into()), Instant::now()));
        let mut r = request("a", "C:\\repo");
        r.branch_hint = Some("main".into());
        assert_eq!(
            w.plan_for(&r).branch,
            Some(PrLookup::Branch {
                repo_root: "C:\\repo".into(),
                branch: "feat/6-lot".into()
            })
        );
        // Detached HEAD: the hint stands in.
        w.git.insert("C:\\repo".into(), git_entry("HEAD", "C:\\repo"));
        r.branch_hint = Some("feat/old".into());
        assert_eq!(
            w.plan_for(&r).branch,
            Some(PrLookup::Branch {
                repo_root: "C:\\repo".into(),
                branch: "feat/old".into()
            })
        );
        // On the default branch: nothing to look up.
        w.git.insert("C:\\repo".into(), git_entry("main", "C:\\repo"));
        r.branch_hint = Some("feat/old".into());
        assert_eq!(w.plan_for(&r).branch, None);
    }

    #[test]
    fn an_open_bound_pr_of_the_same_branch_needs_no_branch_lookup() {
        let mut w = worker(Arc::new(|_, _| failed("unused")));
        w.git.insert("C:\\repo".into(), git_entry("fix/perimetre", "C:\\repo"));
        let url = "https://github.com/example-org/example-repo/pull/42";
        let mut r = request("a", "C:\\repo");
        r.desktop_pr = Some(DesktopPrBinding {
            url: Some(url.into()),
            state: Some("OPEN".into()),
            ..DesktopPrBinding::default()
        });
        let binding = PrLookup::Url(url.into());
        // Not asked yet: wait for its branch.
        let plan = w.plan_for(&r);
        assert_eq!(plan.binding, Some(binding.clone()));
        assert_eq!(plan.branch, None);

        let (pr, head) = parse_gh_pr(VIEW_FAILING, SOURCE_DESKTOP, 1)
            .expect("valid")
            .expect("a PR");
        w.prs.insert(
            binding.clone(),
            PrEntry {
                pr: Some(pr.clone()),
                head,
                fetched_at: Instant::now(),
            },
        );
        assert_eq!(w.plan_for(&r).branch, None, "same branch, open: covered");

        w.git.insert("C:\\repo".into(), git_entry("feat/autre", "C:\\repo"));
        assert!(w.plan_for(&r).branch.is_some(), "another branch is checked out");

        let merged = PrInfo {
            state: "MERGED".into(),
            ..pr
        };
        w.git.insert("C:\\repo".into(), git_entry("fix/perimetre", "C:\\repo"));
        w.prs.insert(
            binding,
            PrEntry {
                pr: Some(merged),
                head: Some("fix/perimetre".into()),
                fetched_at: Instant::now(),
            },
        );
        assert!(w.plan_for(&r).branch.is_some(), "a merged PR may have a successor");
    }

    #[test]
    fn lookups_that_waited_longest_go_first() {
        let mut w = worker(Arc::new(|_, _| failed("unused")));
        let now = Instant::now();
        let url = |n: u32| PrLookup::Url(format!("https://github.com/o/r/pull/{n}"));
        let entry = |secs: u64| PrEntry {
            pr: None,
            head: None,
            fetched_at: ago(now, secs),
        };
        w.prs.insert(url(1), entry(10));
        w.prs.insert(url(2), entry(300));
        let plans = vec![
            LookupPlan {
                binding: Some(url(1)),
                branch: None,
            },
            LookupPlan {
                binding: Some(url(2)),
                branch: Some(url(3)),
            },
            LookupPlan {
                binding: Some(url(1)),
                branch: Some(url(4)),
            },
        ];
        w.force_pr.insert(url(4));
        assert_eq!(w.fetch_order(&plans), vec![url(4), url(3), url(2), url(1)]);
    }

    #[test]
    fn lookup_backoff_grows_and_stays_local() {
        let mut w = worker(Arc::new(|_, _| failed("unused")));
        let lookup = PrLookup::Url("https://github.com/o/r/pull/1".into());
        let other = PrLookup::Url("https://github.com/o/r/pull/2".into());
        let now = Instant::now();
        w.lookup_failed(&lookup);
        assert!(w.lookup_backoff[&lookup].until >= now + LOOKUP_BACKOFF);
        w.lookup_failed(&lookup);
        assert!(w.lookup_backoff[&lookup].until >= now + 2 * LOOKUP_BACKOFF);
        for _ in 0..10 {
            w.lookup_failed(&lookup);
        }
        assert!(w.lookup_backoff[&lookup].until <= Instant::now() + LOOKUP_BACKOFF_MAX);
        assert!(w.blocked(&lookup, Instant::now()));
        assert!(!w.blocked(&other, Instant::now()));
        assert_eq!(w.gh_ok, None, "one lookup's failure says nothing about gh");
        w.bypass_backoff.insert(lookup.clone());
        assert!(!w.blocked(&lookup, Instant::now()), "a bump goes through");
    }

    // -- child processes ---------------------------------------------------

    #[test]
    fn missing_program_is_reported_as_not_installed() {
        let cancel = AtomicBool::new(false);
        let result = run_process(
            "monitor-core-no-such-program-4242",
            &["--version"],
            None,
            &cancel,
            Duration::from_secs(5),
            None,
        );
        assert_eq!(result, Err(RunError::NotInstalled));
    }

    #[test]
    fn runs_git_when_available() {
        let cancel = AtomicBool::new(false);
        match run_process("git", &["--version"], None, &cancel, Duration::from_secs(20), None) {
            Ok(out) => {
                assert!(out.success);
                assert!(out.stdout.starts_with("git version"), "{}", out.stdout);
            }
            Err(RunError::NotInstalled) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn process_runner_uses_the_real_git() {
        let cancel = AtomicBool::new(false);
        let mut runner = ProcessRunner::default();
        let git = runner.git_program(&cancel);
        if cfg!(windows) && git.exe != Path::new("git") {
            assert!(git.exe.is_file(), "{}", git.exe.display());
            assert!(git.exe.ends_with(Path::new("bin").join("git.exe")));
            assert!(git.bin_dir.is_some());
        }
        match runner.run("git", &["--version"], None, &cancel) {
            Ok(out) => assert!(out.stdout.starts_with("git version"), "{}", out.stdout),
            Err(RunError::NotInstalled) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(runner.git_program(&cancel), git, "resolved once");
    }

    /// A timeout ends the grandchildren too: here a `cmd` started by a `cmd`
    /// script, which writes a file about 2 s later unless it is ended.
    #[cfg(windows)]
    #[test]
    fn timeout_ends_the_whole_process_tree() {
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        // Batch files are read in the console code page: keep paths ASCII.
        let base = std::env::temp_dir();
        if !base.to_string_lossy().is_ascii() {
            return;
        }
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = Cleanup(base.join(format!("monitor-core-tree-{}-{nanos}", std::process::id())));
        std::fs::create_dir_all(&dir.0).expect("create temp dir");
        let cancel = AtomicBool::new(false);
        let run = |name: &str, inner_pings: u32, outer_pings: u32, timeout: Duration| {
            let marker = dir.0.join(format!("{name}.txt"));
            let inner = dir.0.join(format!("{name}-inner.cmd"));
            let outer = dir.0.join(format!("{name}-outer.cmd"));
            let inner_script = format!(
                "@ping -n {inner_pings} 127.0.0.1 >nul
@echo x>\"{}\"
",
                marker.display()
            );
            let outer_script = format!(
                "@start \"\" /b cmd /d /c \"{}\"
@ping -n {outer_pings} 127.0.0.1 >nul
",
                inner.display()
            );
            std::fs::write(&inner, inner_script).expect("write inner script");
            std::fs::write(&outer, outer_script).expect("write outer script");
            let outer = outer.to_string_lossy().into_owned();
            let result = run_process("cmd", &["/d", "/c", &outer], None, &cancel, timeout, None);
            (result, marker)
        };

        // Left alone, the grandchild writes its file: the script works here.
        let (result, marker) = run("control", 2, 4, Duration::from_secs(15));
        assert!(result.is_ok_and(|out| out.success));
        assert!(marker.exists(), "the control script did not run");

        let started = Instant::now();
        let (result, marker) = run("timeout", 3, 6, Duration::from_millis(700));
        assert_eq!(result, Err(RunError::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "a grandchild survived the timeout");
    }

    // -- worker, with a fake runner -----------------------------------------

    type Respond = dyn Fn(&str, &str) -> Result<CmdOutput, RunError> + Send + Sync;

    struct FakeRunner {
        respond: Arc<Respond>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Runner for FakeRunner {
        fn run(
            &mut self,
            program: &str,
            args: &[&str],
            _cwd: Option<&Path>,
            _cancel: &AtomicBool,
        ) -> Result<CmdOutput, RunError> {
            let line = args.join(" ");
            self.log
                .lock()
                .expect("log lock")
                .push(format!("{program} {line}"));
            (self.respond)(program, &line)
        }
    }

    fn ok(stdout: &str) -> Result<CmdOutput, RunError> {
        Ok(CmdOutput {
            success: true,
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }

    fn failed(stderr: &str) -> Result<CmdOutput, RunError> {
        Ok(CmdOutput {
            success: false,
            stdout: String::new(),
            stderr: stderr.into(),
        })
    }

    /// An existing folder, so that the worker's `is_dir` checks pass.
    fn repo_dir() -> String {
        env!("CARGO_MANIFEST_DIR").to_string()
    }

    fn fake_git(args: &str, branch: &str) -> Result<CmdOutput, RunError> {
        let repo = repo_dir();
        if args.starts_with("rev-parse --path-format=absolute") {
            // A git folder that does not exist: no stamps, default branch from git.
            ok(&format!("{repo}\n{repo}/.git-fake\n{repo}/.git-fake\n"))
        } else if args.contains("status --porcelain=v2") {
            ok(&format!(
                "# branch.oid 0123\n# branch.head {branch}\n\
                 1 .M N... 100644 100644 100644 a b src/a.rs\n? notes/\n"
            ))
        } else if args == "symbolic-ref --short refs/remotes/origin/HEAD" {
            ok("origin/main\n")
        } else {
            failed("fatal: unexpected")
        }
    }

    struct Harness {
        service: GitPrService,
        log: Arc<Mutex<Vec<String>>>,
        changes: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new(respond: Arc<Respond>) -> Self {
            let log = Arc::new(Mutex::new(Vec::new()));
            let changes = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&changes);
            let runner = FakeRunner {
                respond,
                log: Arc::clone(&log),
            };
            let service = GitPrService::start_with_runner(
                60,
                Arc::new(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                }),
                Box::new(runner),
            );
            Self {
                service,
                log,
                changes,
            }
        }

        fn calls(&self, prefix: &str) -> usize {
            self.log
                .lock()
                .expect("log lock")
                .iter()
                .filter(|line| line.starts_with(prefix))
                .count()
        }

        fn wait_until(&self, what: impl Fn(&GitPrService) -> bool) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if what(&self.service) {
                    return true;
                }
                thread::sleep(Duration::from_millis(10));
            }
            false
        }
    }

    fn request(key: &str, cwd: &str) -> GitPrRequest {
        GitPrRequest {
            key: key.into(),
            cwd: cwd.into(),
            ..GitPrRequest::default()
        }
    }

    #[test]
    fn finds_branch_pr_once_for_sessions_sharing_a_folder() {
        let h = Harness::new(Arc::new(|program, args| {
            if program == "git" {
                fake_git(args, "feature/x")
            } else if args.starts_with("pr list --head feature/x") {
                ok(LIST_PENDING)
            } else {
                failed("unexpected")
            }
        }));
        let cwd = repo_dir();
        h.service
            .track(vec![request("a", &cwd), request("b", &cwd)]);
        assert!(h.wait_until(|s| {
            let results = s.results();
            results.len() == 2 && results.values().all(|(_, pr)| pr.is_some())
        }));
        let results = h.service.results();
        for key in ["a", "b"] {
            let (git, pr) = &results[key];
            let git = git.as_ref().expect("git info");
            assert_eq!(git.branch.as_deref(), Some("feature/x"));
            assert_eq!(git.dirty_files, 2);
            assert!(!git.is_default_branch);
            assert_eq!(git.repo_root.as_deref(), Some(native_path(&cwd).as_str()));
            let pr = pr.as_ref().expect("PR");
            assert_eq!(pr.number, 7);
            assert_eq!(pr.source, "gh-branch");
            assert_eq!(pr.checks, ChecksState::Pending);
        }
        assert_eq!(h.service.gh_ok(), Some(true));
        assert_eq!(h.calls("git rev-parse"), 1, "layout found once per folder");
        assert_eq!(h.calls("git -c core.quotepath=off status"), 1);
        assert_eq!(h.calls("gh "), 1);
        assert!(h.changes.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn a_folder_read_costs_one_command() {
        let h = Harness::new(Arc::new(|program, args| {
            if program == "git" {
                fake_git(args, "main")
            } else {
                failed("unexpected")
            }
        }));
        h.service.track(vec![request("a", &repo_dir())]);
        assert!(h.wait_until(|s| s.results().contains_key("a")));
        thread::sleep(Duration::from_millis(300));
        let first = h.calls("git ");
        // layout + status + default branch (from git: the fake has no files).
        assert_eq!(first, 3, "{:?}", h.log.lock().expect("log"));

        // A turn end reads the folder again: one status, nothing else.
        let mut r = request("a", &repo_dir());
        r.turn_edge_at = Some(5);
        h.service.track(vec![r]);
        assert!(h.wait_until(|_| h.calls("git -c core.quotepath=off status") == 2));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(h.calls("git "), first + 1);
    }

    #[test]
    fn default_branch_needs_no_gh_call() {
        let h = Harness::new(Arc::new(|program, args| {
            if program == "git" {
                fake_git(args, "main")
            } else {
                failed("unexpected")
            }
        }));
        h.service.track(vec![request("a", &repo_dir())]);
        assert!(h.wait_until(|s| s.results().contains_key("a")));
        let (git, pr) = h.service.results().remove("a").expect("result");
        assert!(git.expect("git info").is_default_branch);
        assert_eq!(pr, None);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(h.calls("gh "), 0);
        assert_eq!(h.service.gh_ok(), None);
    }

    #[test]
    fn auth_failure_falls_back_to_binding_and_backs_off() {
        let h = Harness::new(Arc::new(|program, _args| {
            if program == "git" {
                failed("fatal: not a git repository (or any of the parent directories): .git")
            } else {
                failed("To get started with GitHub CLI, please run:  gh auth login")
            }
        }));
        let binding = |n: u32| DesktopPrBinding {
            number: Some(n),
            url: Some(format!("https://github.com/example-org/example-repo/pull/{n}")),
            repository: None,
            state: Some("OPEN".into()),
        };
        let mut first = request("a", &repo_dir());
        first.desktop_pr = Some(binding(3));
        h.service.track(vec![first.clone()]);
        assert!(h.wait_until(|s| s.gh_ok() == Some(false)));
        let (git, pr) = h.service.results().remove("a").expect("result");
        assert_eq!(git, None);
        let pr = pr.expect("PR from the binding");
        assert_eq!(pr.number, 3);
        assert_eq!(pr.repository.as_deref(), Some("example-org/example-repo"));
        assert_eq!(pr.checks, ChecksState::None);
        assert_eq!(pr.source, "desktop");
        assert_eq!(h.calls("git "), 1, "a folder outside git costs one command");

        let mut second = request("b", &repo_dir());
        second.desktop_pr = Some(binding(4));
        h.service.track(vec![first, second]);
        assert!(h.wait_until(|s| s.results().contains_key("b")));
        thread::sleep(Duration::from_millis(150));
        assert_eq!(h.calls("gh "), 1, "no gh call during the back-off");
        assert_eq!(
            h.service.results()["b"].1.as_ref().map(|pr| pr.number),
            Some(4)
        );

        h.service.bump("b");
        assert!(h.wait_until(|_| h.calls("gh ") == 2), "bump bypasses the back-off");
    }

    #[test]
    fn one_failing_lookup_does_not_block_the_others() {
        let h = Harness::new(Arc::new(|program, args| {
            if program == "git" {
                failed("fatal: not a git repository")
            } else if args.contains("/pull/1 ") {
                failed(
                    "HTTP 403: Resource not accessible by integration (https://api.github.com/graphql)",
                )
            } else {
                ok(VIEW_FAILING)
            }
        }));
        let binding = |n: u32| DesktopPrBinding {
            url: Some(format!("https://github.com/example-org/example-repo/pull/{n}")),
            ..DesktopPrBinding::default()
        };
        // "a" sorts first, so its lookup runs first.
        let mut a = request("a", &repo_dir());
        a.desktop_pr = Some(binding(1));
        let mut b = request("b", &repo_dir());
        b.desktop_pr = Some(binding(42));
        h.service.track(vec![a, b]);
        assert!(h.wait_until(|s| {
            s.results()
                .get("b")
                .and_then(|(_, pr)| pr.as_ref())
                .is_some_and(|pr| pr.checks == ChecksState::Fail)
        }));
        assert_eq!(
            h.service.gh_ok(),
            Some(true),
            "a 403 on one repository is not a gh failure"
        );
        assert_eq!(
            h.calls("gh pr view https://github.com/example-org/example-repo/pull/1 "),
            1
        );
        let a_pr = h.service.results()["a"].1.clone().expect("binding shown");
        assert_eq!(a_pr.number, 1);
        assert_eq!(a_pr.source, "desktop");
    }

    #[test]
    fn open_pr_of_the_checked_out_branch_beats_an_old_merged_card() {
        let h = Harness::new(Arc::new(|program, args| {
            if program == "git" {
                fake_git(args, "feature/x")
            } else if args.starts_with("pr view https://github.com/example-org/other-repo/pull/12")
            {
                ok(VIEW_MERGED)
            } else if args.starts_with("pr list --head feature/x") {
                ok(LIST_PENDING)
            } else {
                failed("unexpected")
            }
        }));
        let mut r = request("a", &repo_dir());
        r.desktop_pr = Some(DesktopPrBinding {
            number: Some(12),
            url: Some("https://github.com/example-org/other-repo/pull/12".into()),
            state: Some("MERGED".into()),
            ..DesktopPrBinding::default()
        });
        // A stale hint must not matter either.
        r.branch_hint = Some("main".into());
        h.service.track(vec![r]);
        assert!(h.wait_until(|s| {
            s.results()
                .get("a")
                .and_then(|(_, pr)| pr.as_ref())
                .is_some_and(|pr| pr.number == 7)
        }));
        let pr = h.service.results()["a"].1.clone().expect("PR");
        assert_eq!(pr.state, "OPEN");
        assert_eq!(pr.source, "gh-branch");
    }

    #[test]
    fn new_turn_rereads_git_but_respects_pr_min_interval() {
        let h = Harness::new(Arc::new(|program, args| {
            if program == "git" {
                fake_git(args, "feature/z")
            } else {
                ok(LIST_PENDING)
            }
        }));
        let mut r = request("a", &repo_dir());
        r.turn_edge_at = Some(1);
        h.service.track(vec![r.clone()]);
        assert!(h.wait_until(|s| s.results().get("a").is_some_and(|(_, pr)| pr.is_some())));
        let changes = h.changes.load(Ordering::SeqCst);
        let status_reads = h.calls("git -c core.quotepath=off status");
        let checked_at = |s: &GitPrService| {
            s.results()
                .get("a")
                .and_then(|(git, _)| git.as_ref().map(|g| g.checked_at))
                .unwrap_or(0)
        };
        let first_check = checked_at(&h.service);

        r.turn_edge_at = Some(2);
        h.service.track(vec![r]);
        assert!(h.wait_until(|_| h.calls("git -c core.quotepath=off status") > status_reads));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(h.calls("gh "), 1, "PR not looked up again within 10 s");
        assert_eq!(
            h.changes.load(Ordering::SeqCst),
            changes,
            "a new checked_at alone is not a change"
        );
        assert_eq!(
            checked_at(&h.service),
            first_check,
            "an unchanged result is kept as is"
        );
    }

    #[test]
    fn missing_folder_and_untracked_keys() {
        let h = Harness::new(Arc::new(|_, _| failed("unexpected")));
        h.service
            .track(vec![request("gone", "Z:\\no\\such\\folder\\à supprimer")]);
        assert!(h.wait_until(|s| s.results().contains_key("gone")));
        assert_eq!(h.service.results()["gone"], (None, None));
        assert_eq!(h.calls(""), 0);
        assert_eq!(h.changes.load(Ordering::SeqCst), 0);
        h.service.track(Vec::new());
        h.service.bump("gone");
        assert!(h.service.results().is_empty());
    }

    #[test]
    fn drop_stops_the_worker_quickly() {
        let h = Harness::new(Arc::new(|program, args| {
            if program == "git" {
                fake_git(args, "feature/y")
            } else {
                ok(LIST_EMPTY)
            }
        }));
        h.service.track(vec![request("a", &repo_dir())]);
        assert!(h.wait_until(|s| s.results().contains_key("a")));
        let started = Instant::now();
        drop(h);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// Read-only check against real folders, run by hand:
    /// `GIT_PR_LIVE="<cwd>[|<pr url>];<cwd>…" cargo test live_check -- --ignored --nocapture`
    #[test]
    #[ignore = "runs the real git and gh on folders given in GIT_PR_LIVE"]
    fn live_check() {
        let spec = std::env::var("GIT_PR_LIVE").unwrap_or_default();
        let requests: Vec<GitPrRequest> = spec
            .split(';')
            .filter(|entry| !entry.trim().is_empty())
            .enumerate()
            .map(|(i, entry)| {
                let (cwd, url) = entry.split_once('|').unwrap_or((entry, ""));
                GitPrRequest {
                    key: format!("live-{i}"),
                    cwd: cwd.trim().to_string(),
                    desktop_pr: (!url.trim().is_empty()).then(|| DesktopPrBinding {
                        url: Some(url.trim().to_string()),
                        ..DesktopPrBinding::default()
                    }),
                    ..GitPrRequest::default()
                }
            })
            .collect();
        let changes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&changes);
        let started = Instant::now();
        let service = GitPrService::start(
            60,
            Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        );
        service.track(requests.clone());
        let call_returned = started.elapsed();
        let mut stable_since = Instant::now();
        let mut last = service.results();
        while started.elapsed() < Duration::from_secs(60)
            && stable_since.elapsed() < Duration::from_secs(8)
        {
            thread::sleep(Duration::from_millis(250));
            let now = service.results();
            if now != last {
                last = now;
                stable_since = Instant::now();
            }
        }
        println!("track() returned after {call_returned:?}");
        for r in &requests {
            let result = last.get(&r.key);
            println!(
                "{} -> {}",
                r.cwd,
                serde_json::to_string(&result).unwrap_or_default()
            );
        }
        println!(
            "gh_ok = {:?}, on_change calls = {}",
            service.gh_ok(),
            changes.load(Ordering::SeqCst)
        );
        let dropping = Instant::now();
        drop(service);
        println!("drop took {:?}", dropping.elapsed());
    }
}
