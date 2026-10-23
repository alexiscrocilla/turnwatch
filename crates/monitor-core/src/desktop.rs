//! Desktop's per-session files under `%APPDATA%\Claude\claude-code-sessions`.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;
use serde_json::Value;

use crate::model::{
    DesktopPrBinding, DesktopSession, ErrorMark, PeerReceipt, SessionRef, SideNote,
    TaskSuggestion, TurnSummary,
};

/// Longest error message kept in an error label, in characters.
const ERROR_MESSAGE_MAX: usize = 120;

/// Parses one `local_<id>.json` file. None when it has no `sessionId`.
pub fn parse_session(json: &str) -> Option<DesktopSession> {
    let json = json.strip_prefix('\u{feff}').unwrap_or(json);
    if !json.trim_start().starts_with('{') {
        return None;
    }
    let raw = match serde_json::from_str::<RawSession>(json) {
        Ok(raw) => raw,
        // A data error (such as a duplicated key) still leaves a valid
        // document: go through a Value, which keeps the last duplicate.
        Err(e) if e.classify() == serde_json::error::Category::Data => {
            let value: Value = serde_json::from_str(json).ok()?;
            RawSession::deserialize(value).ok()?
        }
        Err(_) => return None,
    };
    raw.into_session()
}

/// The fields of a Desktop file the monitor needs. Every other field, some of
/// them large, is skipped without being built. Each field is kept as a raw
/// Value so that a type change in one of them never drops the whole session.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSession {
    session_id: Option<Value>,
    cli_session_id: Option<Value>,
    title: Option<Value>,
    cwd: Option<Value>,
    is_archived: Option<Value>,
    is_starred: Option<Value>,
    model: Option<Value>,
    effort: Option<Value>,
    permission_mode: Option<Value>,
    created_at: Option<Value>,
    last_activity_at: Option<Value>,
    last_focused_at: Option<Value>,
    last_turn_edge_at: Option<Value>,
    latest_user_frame_at: Option<Value>,
    completed_turns: Option<Value>,
    turn_wrap_up: Option<Value>,
    last_turn_report: Option<Value>,
    branch: Option<Value>,
    source_branch: Option<Value>,
    worktree_name: Option<Value>,
    worktree_path: Option<Value>,
    pr_number: Option<Value>,
    pr_url: Option<Value>,
    pr_repository: Option<Value>,
    pr_state: Option<Value>,
    prs: Option<Value>,
    spawned_from: Option<Value>,
    peer_receipts: Option<Value>,
    queued_side_session_notes: Option<Value>,
    context_exceeded_count: Option<Value>,
    post_turn_summary: Option<Value>,
    post_turn_summary_for: Option<Value>,
    last_assistant_uuid: Option<Value>,
    classifier_summary_enabled: Option<Value>,
    error: Option<Value>,
    error_category: Option<Value>,
    error_at: Option<Value>,
    interrupted_awaiting_person: Option<Value>,
    prior_error_mark: Option<Value>,
    query_crashes: Option<Value>,
    background_task_suggestions: Option<Value>,
}

impl RawSession {
    fn into_session(self) -> Option<DesktopSession> {
        let host_session_id = string(self.session_id.as_ref())?;
        Some(DesktopSession {
            host_session_id,
            cli_session_id: string(self.cli_session_id.as_ref()),
            title: text(self.title.as_ref()),
            cwd: string(self.cwd.as_ref()),
            is_archived: flag(self.is_archived.as_ref()),
            is_starred: flag(self.is_starred.as_ref()),
            model: string(self.model.as_ref()),
            effort: string(self.effort.as_ref()),
            permission_mode: string(self.permission_mode.as_ref()),
            created_at: int(self.created_at.as_ref()),
            last_activity_at: int(self.last_activity_at.as_ref()),
            last_focused_at: int(self.last_focused_at.as_ref()),
            last_turn_edge_at: int(self.last_turn_edge_at.as_ref()),
            latest_user_frame_at: int(self.latest_user_frame_at.as_ref()),
            completed_turns: count(self.completed_turns.as_ref()),
            turn_wrap_up: self.turn_wrap_up.as_ref().and_then(|v| {
                text(v.get("description")).or_else(|| text(Some(v)))
            }),
            last_turn_report: text(self.last_turn_report.as_ref()),
            branch: string(self.branch.as_ref()),
            source_branch: string(self.source_branch.as_ref()),
            worktree_name: string(self.worktree_name.as_ref()),
            worktree_path: string(self.worktree_path.as_ref()),
            pr: self.pr_binding(),
            spawned_from: self.spawned_from.as_ref().and_then(|v| {
                Some(SessionRef {
                    host_session_id: string(v.get("sessionId"))?,
                    title: text(v.get("title")),
                })
            }),
            peer_receipts: peer_receipts(self.peer_receipts.as_ref()),
            queued_notes: side_notes(self.queued_side_session_notes.as_ref()),
            context_exceeded_count: count(self.context_exceeded_count.as_ref()),
            turn_summary: self.turn_summary(),
            classifier_summary_enabled: flag(self.classifier_summary_enabled.as_ref()),
            error: self.desktop_error(),
            prior_error: self.prior_error(),
            last_crash: latest_crash(
                self.query_crashes.as_ref(),
                string(self.cli_session_id.as_ref()).as_deref(),
            ),
            suggestions: suggestions(self.background_task_suggestions.as_ref()),
        })
    }

    /// Desktop's `DS()`: `postTurnSummary` merged with `turnWrapUp`. A
    /// wrap-up that needs the person makes the turn "blocked"; a wrap-up that
    /// does not turns the classifier's "blocked" / "need_input" into
    /// "review_ready"; "failed" always wins.
    fn turn_summary(&self) -> Option<TurnSummary> {
        // Desktop drops a wrap-up whose description or needsUser is not text.
        let wrap_up = self.turn_wrap_up.as_ref().filter(|w| {
            w.get("description").is_some_and(Value::is_string)
                && w.get("needsUser").is_none_or(Value::is_string)
        });
        let summary = self.post_turn_summary.as_ref().filter(|v| v.is_object());
        if wrap_up.is_none() && summary.is_none() {
            return None;
        }
        let needs_user = wrap_up.and_then(|w| text(w.get("needsUser")));
        let raw_category = summary.and_then(|s| text(s.get("status_category")));
        let raw_needs = summary.and_then(|s| text(s.get("needs_action")));
        let failed = raw_category.as_deref() == Some("failed");
        let blocked = matches!(raw_category.as_deref(), Some("blocked" | "need_input"));
        let (category, needs_action) = match (wrap_up, needs_user) {
            (None, _) => (raw_category, raw_needs),
            (Some(_), needs) if failed => (raw_category, needs.or(raw_needs)),
            (Some(_), Some(needs)) => (Some("blocked".to_owned()), Some(needs)),
            (Some(_), None) if blocked => (Some("review_ready".to_owned()), None),
            (Some(_), None) => (raw_category, None),
        };
        // Desktop clears both at the next message; the summary also names
        // the assistant message it describes.
        let fresh = match summary {
            None => true,
            Some(s) => {
                let target = string(self.post_turn_summary_for.as_ref())
                    .or_else(|| string(s.get("summarizes_uuid")));
                target.is_some() && target == string(self.last_assistant_uuid.as_ref())
            }
        };
        Some(TurnSummary {
            category,
            detail: summary.and_then(|s| text(s.get("status_detail"))),
            needs_action,
            fresh,
        })
    }

    /// `error` with its `errorAt`, as Desktop shows it.
    fn desktop_error(&self) -> Option<ErrorMark> {
        let message = text(self.error.as_ref())?;
        let at = int(self.error_at.as_ref())?;
        Some(ErrorMark {
            at: Some(at),
            label: error_label(
                string(self.error_category.as_ref()),
                Some(&Value::String(message)),
            ),
        })
    }

    /// `priorErrorMark` of a turn that was running, or of an interruption
    /// Desktop holds for the person. Marks written when Desktop stops an idle
    /// process (`hadPendingTurn: false`) mean nothing was lost.
    fn prior_error(&self) -> Option<ErrorMark> {
        let mark = self.prior_error_mark.as_ref().filter(|v| v.is_object())?;
        let pending = matches!(mark.get("hadPendingTurn"), Some(Value::Bool(true)));
        // A boolean in Desktop: `false` holds nothing.
        let held = matches!(self.interrupted_awaiting_person, Some(Value::Bool(true)));
        if !pending && !held {
            return None;
        }
        let at = int(mark.get("at"));
        let label = error_label(
            string(mark.get("category")).or_else(|| string(mark.get("source"))),
            None,
        );
        (at.is_some() || label.is_some()).then_some(ErrorMark { at, label })
    }

    /// The top-level `prNumber`/`prUrl` binding, else the card Desktop shows
    /// from its `prs` list.
    fn pr_binding(&self) -> Option<DesktopPrBinding> {
        let number = count(self.pr_number.as_ref());
        let url = string(self.pr_url.as_ref());
        if number.is_some() || url.is_some() {
            return Some(DesktopPrBinding {
                number,
                url,
                repository: string(self.pr_repository.as_ref()),
                state: string(self.pr_state.as_ref()),
            });
        }
        pr_from_cards(self.prs.as_ref()?)
    }
}

/// Newer Desktop builds keep every PR a session touched in `prs`
/// (`{prNumber, url, repo, state, dismissed…}`), oldest first. Picks the
/// latest open card that is not dismissed, else the latest card not dismissed.
fn pr_from_cards(prs: &Value) -> Option<DesktopPrBinding> {
    let mut cards: Vec<DesktopPrBinding> = prs
        .as_array()?
        .iter()
        .filter(|card| !flag(card.get("dismissed")))
        .filter_map(|card| {
            let number = count(card.get("prNumber")).or_else(|| count(card.get("number")));
            let url = string(card.get("url"));
            (number.is_some() || url.is_some()).then(|| DesktopPrBinding {
                number,
                url,
                repository: string(card.get("repo")).or_else(|| string(card.get("repository"))),
                state: string(card.get("state")),
            })
        })
        .collect();
    let index = cards
        .iter()
        .rposition(|card| card.state.as_deref() == Some("OPEN"))
        .or_else(|| cards.len().checked_sub(1))?;
    Some(cards.swap_remove(index))
}

/// Well-formed entries of `peerReceipts`; malformed ones are skipped.
fn peer_receipts(value: Option<&Value>) -> Vec<PeerReceipt> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(PeerReceipt {
                message_id: string(item.get("messageId"))?,
                target_session_id: string(item.get("targetSessionId"))?,
                sent_at: int(item.get("sentAt"))?,
                delivery: string(item.get("delivery")),
                delivered_at: int(item.get("deliveredAt")),
                read_at: int(item.get("readAt")),
                replied_at: int(item.get("repliedAt")),
            })
        })
        .collect()
}

/// Well-formed entries of `queuedSideSessionNotes`; malformed ones are skipped.
fn side_notes(value: Option<&Value>) -> Vec<SideNote> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(SideNote {
                session_id: string(item.get("sessionId"))?,
                title: text(item.get("title")),
                at: int(item.get("at"))?,
            })
        })
        .collect()
}

/// Well-formed entries of `backgroundTaskSuggestions` (an id, a title and a
/// creation time); malformed ones are skipped.
fn suggestions(value: Option<&Value>) -> Vec<TaskSuggestion> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(TaskSuggestion {
                id: string(item.get("id"))?,
                title: text(item.get("title"))?,
                tldr: text(item.get("tldr")),
                created_at: int(item.get("createdAt"))?,
                expires_at: int(item.get("expiresAt")),
                offer: flag(item.get("offer")),
                accepted: flag(item.get("accepted")),
                auto_start: string(item.get("autoStart")).is_some(),
            })
        })
        .collect()
}

/// Latest entry of `queryCrashes[]` for the current CLI session (as Desktop
/// filters them; entries without `cliSessionId` count): its time and a label.
fn latest_crash(crashes: Option<&Value>, cli_session_id: Option<&str>) -> Option<ErrorMark> {
    let Some(Value::Array(items)) = crashes else {
        return None;
    };
    let mut latest: Option<ErrorMark> = None;
    for crash in items.iter().filter(|v| v.is_object()) {
        if let (Some(current), Some(of)) = (cli_session_id, string(crash.get("cliSessionId")))
            && of != current
        {
            continue;
        }
        let at = int(crash.get("at"));
        let label = error_label(string(crash.get("errorCategory")), crash.get("errorMessage"));
        if at.is_none() && label.is_none() {
            continue;
        }
        if latest.as_ref().is_none_or(|l| at >= l.at) {
            latest = Some(ErrorMark { at, label });
        }
    }
    latest
}

/// "category: message", with the message on one line and shortened.
fn error_label(category: Option<String>, message: Option<&Value>) -> Option<String> {
    let message = text(message).map(|m| one_line(&m, ERROR_MESSAGE_MAX));
    match (category, message) {
        (Some(category), Some(message)) => Some(format!("{category}: {message}")),
        (category, message) => category.or(message),
    }
}

/// Collapses whitespace (newlines included) and cuts to `max` characters,
/// ending with "…" when cut.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// A non-blank string, unchanged.
fn string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// A non-blank string, trimmed.
fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_owned())
        }
        _ => None,
    }
}

/// An integer, from a JSON number or a numeric string.
fn int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && f.abs() < 9.0e18)
                .map(|f| f.round() as i64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A non-negative integer that fits in `u32`.
fn count(value: Option<&Value>) -> Option<u32> {
    int(value).and_then(|n| u32::try_from(n).ok())
}

/// True only for a JSON `true`.
fn flag(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(true)))
}

/// Size and modification time of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

/// What the index knows about one `local_*.json` file.
struct CachedFile {
    /// Stamp of the file when it was last parsed successfully.
    parsed: Option<Stamp>,
    /// Result of that parse.
    session: Option<DesktopSession>,
}

/// Cache of every Desktop session file, re-read only when it changes.
pub struct DesktopIndex {
    root: PathBuf,
    files: HashMap<PathBuf, CachedFile>,
    sessions: HashMap<String, DesktopSession>,
}

impl DesktopIndex {
    /// Index of the files under `root` (`<root>\<account>\<org>\local_*.json`).
    /// Reads nothing until [`DesktopIndex::refresh`].
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            files: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    /// Re-reads the files whose size or modification time changed and drops
    /// the deleted ones. Returns true when anything changed. Fails only when
    /// the root folder cannot be listed; the previous sessions are then kept.
    pub fn refresh(&mut self) -> Result<bool, String> {
        let scan = scan_tree(&self.root)?;
        let mut dirty = false;

        let listed: HashSet<&Path> = scan.files.iter().map(|(path, _)| path.as_path()).collect();
        self.files.retain(|path, cached| {
            let keep = !scan.complete
                || listed.contains(path.as_path())
                || scan.unlisted.iter().any(|dir| path.starts_with(dir));
            if !keep && cached.session.is_some() {
                dirty = true;
            }
            keep
        });

        for (path, stamp) in &scan.files {
            // Unreadable metadata: keep what is cached and try again next time.
            let Some(stamp) = *stamp else { continue };
            if self.files.get(path).and_then(|cached| cached.parsed) == Some(stamp) {
                continue;
            }
            let session = read_session(path);
            let cached = self.files.entry(path.clone()).or_insert(CachedFile {
                parsed: None,
                session: None,
            });
            // On failure the previous value stays and the stamp is not
            // recorded, so the file is read again on the next refresh.
            if let Some(session) = session {
                cached.session = Some(session);
                cached.parsed = Some(stamp);
                // Even unchanged content can change which duplicate wins.
                dirty = true;
            }
        }

        if !dirty {
            return Ok(false);
        }
        // Report only what the caller can see: a re-read file whose fields
        // of interest did not change is not a change.
        let sessions = self.collect_sessions();
        let changed = sessions != self.sessions;
        self.sessions = sessions;
        Ok(changed)
    }

    /// Sessions by `host_session_id`.
    pub fn sessions(&self) -> &HashMap<String, DesktopSession> {
        &self.sessions
    }

    /// One session per id; when two files claim the same id, the most
    /// recently modified one wins.
    fn collect_sessions(&self) -> HashMap<String, DesktopSession> {
        let mut best: HashMap<&str, (Option<SystemTime>, &Path, &DesktopSession)> = HashMap::new();
        for (path, cached) in &self.files {
            let Some(session) = &cached.session else { continue };
            let candidate = (cached.parsed.and_then(|s| s.modified), path.as_path(), session);
            match best.entry(session.host_session_id.as_str()) {
                Entry::Vacant(slot) => {
                    slot.insert(candidate);
                }
                Entry::Occupied(mut slot) => {
                    let current = slot.get();
                    if (candidate.0, candidate.1) > (current.0, current.1) {
                        slot.insert(candidate);
                    }
                }
            }
        }
        best.into_iter()
            .map(|(id, (_, _, session))| (id.to_owned(), session.clone()))
            .collect()
    }
}

/// Result of listing the tree under the root.
struct Scan {
    /// Every `local_*.json` file found, with its stamp when it could be read.
    files: Vec<(PathBuf, Option<Stamp>)>,
    /// Folders that could not be listed: their cached files are kept.
    unlisted: Vec<PathBuf>,
    /// False when the root listing itself was incomplete: nothing is dropped.
    complete: bool,
}

/// Lists `<root>\<account>\<org>\local_*.json`, ignoring everything else.
fn scan_tree(root: &Path) -> Result<Scan, String> {
    let accounts = fs::read_dir(root).map_err(|e| root_error(root, &e))?;
    let mut scan = Scan {
        files: Vec::new(),
        unlisted: Vec::new(),
        complete: true,
    };
    for account in accounts {
        let Ok(account) = account else {
            scan.complete = false;
            continue;
        };
        let account = account.path();
        if !account.is_dir() {
            continue;
        }
        let Ok(orgs) = fs::read_dir(&account) else {
            scan.unlisted.push(account);
            continue;
        };
        for org in orgs {
            let Ok(org) = org else {
                scan.unlisted.push(account.clone());
                continue;
            };
            let org = org.path();
            if !org.is_dir() {
                continue;
            }
            scan_org(&org, &mut scan);
        }
    }
    Ok(scan)
}

/// Adds the session files of one `<account>\<org>` folder to `scan`.
fn scan_org(org: &Path, scan: &mut Scan) {
    let Ok(entries) = fs::read_dir(org) else {
        scan.unlisted.push(org.to_path_buf());
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            scan.unlisted.push(org.to_path_buf());
            continue;
        };
        if !is_session_file_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        // The listing's own copy of size and time costs no extra system call
        // on Windows (fs::metadata opens each file: ~10x slower). NTFS brings
        // it up to date when the writer closes the file, which Desktop does
        // after every save.
        match entry.metadata() {
            Ok(meta) if meta.is_file() => {
                let stamp = Stamp {
                    len: meta.len(),
                    modified: meta.modified().ok(),
                };
                scan.files.push((path, Some(stamp)));
            }
            Ok(_) => {}
            Err(_) => scan.files.push((path, None)),
        }
    }
}

/// `local_<id>.json`.
fn is_session_file_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|n| {
        n.len() > "local_.json".len() && n.starts_with("local_") && n.ends_with(".json")
    })
}

/// Reads and parses one file; None on any failure.
fn read_session(path: &Path) -> Option<DesktopSession> {
    let bytes = fs::read(path).ok()?;
    parse_session(std::str::from_utf8(&bytes).ok()?)
}

fn root_error(root: &Path, error: &std::io::Error) -> String {
    if error.kind() == ErrorKind::NotFound {
        format!(
            "Dossier des sessions Claude Desktop introuvable : {}",
            root.display()
        )
    } else {
        format!(
            "Lecture impossible du dossier des sessions Claude Desktop {} : {error}",
            root.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant, UNIX_EPOCH};

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/desktop/",
                $name
            ))
        };
    }

    const FULL: &str = fixture!("full.json");
    const MINIMAL: &str = fixture!("minimal.json");
    const WRONG_TYPES: &str = fixture!("wrong_types.json");
    const NO_SESSION_ID: &str = fixture!("no_session_id.json");
    const TRUNCATED: &str = fixture!("truncated.json");
    const PR_CARDS: &str = fixture!("pr_cards.json");
    const WAITING_ERROR: &str = fixture!("waiting_error.json");

    #[test]
    fn parses_full_file() {
        let s = parse_session(FULL).expect("full fixture parses");
        assert_eq!(s.host_session_id, "local_11111111-2222-4333-8444-555555555555");
        assert_eq!(s.cli_session_id.as_deref(), Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"));
        assert_eq!(s.title.as_deref(), Some("Fix the flaky upload test"));
        assert_eq!(s.cwd.as_deref(), Some("D:\\Données Utilisateur\\Projets\\demo app"));
        assert!(!s.is_archived);
        assert!(s.is_starred);
        assert_eq!(s.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(s.effort.as_deref(), Some("high"));
        assert_eq!(s.permission_mode.as_deref(), Some("auto"));
        assert_eq!(s.created_at, Some(1_792_600_000_000));
        assert_eq!(s.last_activity_at, Some(1_792_600_900_000));
        assert_eq!(s.last_focused_at, Some(1_792_600_500_000));
        assert_eq!(s.last_turn_edge_at, Some(1_792_600_890_000));
        assert_eq!(s.latest_user_frame_at, Some(1_792_600_100_000));
        assert_eq!(s.completed_turns, Some(7));
        assert_eq!(
            s.turn_wrap_up.as_deref(),
            Some("The upload test now waits for the server; all tests pass.")
        );
        assert_eq!(s.last_turn_report.as_deref(), Some("Upload test fixed, PR opened."));
        assert_eq!(s.branch.as_deref(), Some("claude/fix-upload-test"));
        assert_eq!(s.source_branch.as_deref(), Some("main"));
        assert_eq!(s.worktree_name.as_deref(), Some("fix-upload-test"));
        assert_eq!(
            s.worktree_path.as_deref(),
            Some("D:\\Données Utilisateur\\Projets\\demo app\\.claude\\worktrees\\fix-upload-test")
        );
        assert_eq!(
            s.pr,
            Some(DesktopPrBinding {
                number: Some(42),
                url: Some("https://github.com/example/demo-app/pull/42".into()),
                repository: Some("example/demo-app".into()),
                state: Some("OPEN".into()),
            })
        );
        assert_eq!(
            s.spawned_from,
            Some(SessionRef {
                host_session_id: "local_99999999-8888-4777-8666-555555555555".into(),
                title: Some("Release checklist".into()),
            })
        );
        assert_eq!(s.peer_receipts.len(), 2, "the malformed receipt is skipped");
        assert_eq!(
            s.peer_receipts[0],
            PeerReceipt {
                message_id: "msg-1".into(),
                target_session_id: "local_99999999-8888-4777-8666-555555555555".into(),
                sent_at: 1_792_600_200_000,
                delivery: Some("delivered".into()),
                delivered_at: Some(1_792_600_200_050),
                read_at: Some(1_792_600_260_000),
                replied_at: Some(1_792_600_300_000),
            }
        );
        assert_eq!(s.peer_receipts[1].message_id, "msg-2");
        assert_eq!(s.peer_receipts[1].read_at, None);
        assert_eq!(s.peer_receipts[1].replied_at, None);
        assert_eq!(s.context_exceeded_count, Some(1));
        // The mark was written while a turn was pending: kept.
        assert_eq!(
            s.prior_error,
            Some(ErrorMark {
                at: Some(1_792_600_600_000),
                label: Some("process_interrupted".into()),
            })
        );
        let crash = s.last_crash.expect("crash");
        assert_eq!(crash.at, Some(1_792_600_700_000));
        let error = crash.label.expect("crash label");
        assert!(error.starts_with("process_interrupted: The process was interrupted"));
        assert!(!error.contains('\n'));
        let message = error.trim_start_matches("process_interrupted: ");
        assert!(message.chars().count() <= ERROR_MESSAGE_MAX);
        assert!(message.ends_with('…'));
        assert_eq!(s.error, None);
        // The summary names no `lastAssistantUuid`: not fresh. The wrap-up
        // without `needsUser` keeps the classifier's category.
        assert_eq!(
            s.turn_summary,
            Some(TurnSummary {
                category: Some("done".into()),
                detail: Some("synthetic".into()),
                needs_action: None,
                fresh: false,
            })
        );
        assert!(!s.classifier_summary_enabled);
        assert!(s.queued_notes.is_empty());
    }

    #[test]
    fn parses_summary_errors_and_queued_notes() {
        let s = parse_session(WAITING_ERROR).expect("waiting/error fixture parses");
        assert!(s.classifier_summary_enabled);
        assert_eq!(
            s.turn_summary,
            Some(TurnSummary {
                category: Some("blocked".into()),
                detail: Some("PR #311 ouverte ; attend ta décision".into()),
                needs_action: Some("ta décision sur le ménage de la branche".into()),
                fresh: true,
            })
        );
        assert_eq!(
            s.error,
            Some(ErrorMark {
                at: Some(1_792_600_950_000),
                label: Some("auth_error: Your session credentials expired.".into()),
            })
        );
        // `hadPendingTurn: false`: Desktop stopped an idle process.
        assert_eq!(s.prior_error, None);
        assert_eq!(s.last_crash, None);
        assert_eq!(
            s.queued_notes,
            [SideNote {
                session_id: "local_cc5b463c-4454-4fdc-b373-a46663f78260".into(),
                title: Some("Fermer les fuites".into()),
                at: 1_792_600_900_000,
            }],
            "the malformed note is skipped"
        );
    }

    #[test]
    fn turn_summary_follows_desktop_merge_rules() {
        let summary = |extra: &str| {
            let json = format!(
                r#"{{"sessionId":"local_s","lastAssistantUuid":"u1"{extra}}}"#
            );
            parse_session(&json).expect("parses").turn_summary
        };
        let pts = |category: &str, needs: &str| {
            format!(
                concat!(
                    r#","postTurnSummaryFor":"u1","postTurnSummary":{{"#,
                    r#""status_category":"{}","status_detail":"d","#,
                    r#""needs_action":"{}","summarizes_uuid":"u1"}}"#
                ),
                category, needs
            )
        };
        let category = |extra: &str| summary(extra).and_then(|s| s.category);
        let needs = |extra: &str| summary(extra).and_then(|s| s.needs_action);

        assert_eq!(summary(""), None);
        // Desktop drops a malformed wrap-up.
        assert_eq!(summary(r#","turnWrapUp":{"description":5}"#), None);
        assert_eq!(
            summary(r#","turnWrapUp":{"description":"d","needsUser":3}"#),
            None
        );
        assert_eq!(category(&pts("blocked", "x")).as_deref(), Some("blocked"));
        assert_eq!(needs(&pts("blocked", "x")).as_deref(), Some("x"));
        assert_eq!(needs(&pts("completed", "")), None, "blank needs_action");
        // A wrap-up without needsUser overrides the classifier's "blocked".
        let quiet_wrap = r#","turnWrapUp":{"description":"Fait."}"#;
        let merged = format!("{}{quiet_wrap}", pts("blocked", "x"));
        assert_eq!(category(&merged).as_deref(), Some("review_ready"));
        assert_eq!(needs(&merged), None);
        let merged = format!("{}{quiet_wrap}", pts("need_input", "x"));
        assert_eq!(category(&merged).as_deref(), Some("review_ready"));
        let merged = format!("{}{quiet_wrap}", pts("completed", ""));
        assert_eq!(category(&merged).as_deref(), Some("completed"));
        // A wrap-up that needs the person makes it blocked, even alone.
        let asking_wrap = r#","turnWrapUp":{"description":"Fait.","needsUser":"On fusionne ?"}"#;
        let merged = format!("{}{asking_wrap}", pts("completed", ""));
        assert_eq!(category(&merged).as_deref(), Some("blocked"));
        assert_eq!(needs(&merged).as_deref(), Some("On fusionne ?"));
        let alone = summary(asking_wrap).expect("wrap-up alone");
        assert_eq!(alone.category.as_deref(), Some("blocked"));
        assert!(alone.fresh, "Desktop clears a wrap-up at the next message");
        // "failed" always wins.
        let merged = format!("{}{asking_wrap}", pts("failed", "relance"));
        assert_eq!(category(&merged).as_deref(), Some("failed"));
        assert_eq!(needs(&merged).as_deref(), Some("On fusionne ?"));
        let merged = format!("{}{quiet_wrap}", pts("failed", "relance"));
        assert_eq!(needs(&merged).as_deref(), Some("relance"));
        // Freshness: postTurnSummaryFor, else summarizes_uuid, must name the
        // latest assistant message.
        let stale = concat!(
            r#","postTurnSummaryFor":"u0","#,
            r#""postTurnSummary":{"status_category":"blocked","summarizes_uuid":"u1"}"#
        );
        assert!(!summary(stale).expect("stale").fresh);
        let no_for = r#","postTurnSummary":{"status_category":"blocked","summarizes_uuid":"u1"}"#;
        assert!(summary(no_for).expect("no for").fresh);
        let no_uuid = r#"{"sessionId":"local_s","postTurnSummary":{"status_category":"blocked"}}"#;
        assert!(!parse_session(no_uuid).expect("parses").turn_summary.expect("s").fresh);
    }

    #[test]
    fn error_marks_need_a_pending_turn() {
        let mark = |pending: &str, held: &str| {
            let json = format!(
                r#"{{"sessionId":"local_e","interruptedAwaitingPerson":{held},
                "priorErrorMark":{{"source":"result","at":50{pending}}}}}"#
            );
            parse_session(&json).expect("parses").prior_error
        };
        let kept = Some(ErrorMark {
            at: Some(50),
            label: Some("result".into()),
        });
        assert_eq!(mark(r#","hadPendingTurn":true"#, "null"), kept);
        assert_eq!(mark(r#","hadPendingTurn":false"#, "null"), None);
        assert_eq!(mark("", "null"), None);
        // Desktop holds an interrupted turn for the person (a boolean).
        assert_eq!(mark(r#","hadPendingTurn":false"#, "true"), kept);
        assert_eq!(mark(r#","hadPendingTurn":false"#, "false"), None);
        assert_eq!(mark("", "false"), None);

        // Desktop's own error needs both the message and its time.
        let error = |fields: &str| {
            parse_session(&format!(r#"{{"sessionId":"local_e"{fields}}}"#))
                .expect("parses")
                .error
        };
        assert_eq!(error(r#","error":"boom""#), None);
        assert_eq!(error(r#","errorAt":5"#), None);
        assert_eq!(error(r#","error":"  ","errorAt":5"#), None);
        assert_eq!(
            error(r#","error":"boom\nagain","errorAt":5"#),
            Some(ErrorMark {
                at: Some(5),
                label: Some("boom again".into()),
            })
        );
    }

    #[test]
    fn parses_minimal_file() {
        let s = parse_session(MINIMAL).expect("minimal fixture parses");
        assert_eq!(
            s,
            DesktopSession {
                host_session_id: "local_00000000-0000-4000-8000-000000000001".into(),
                ..Default::default()
            }
        );
    }

    #[test]
    fn wrong_types_are_dropped_field_by_field() {
        let s = parse_session(WRONG_TYPES).expect("wrong-types fixture parses");
        assert_eq!(s.host_session_id, "local_00000000-0000-4000-8000-000000000002");
        assert_eq!(s.title, None);
        assert_eq!(s.cwd, None);
        assert!(!s.is_archived, "\"yes\" is not a boolean");
        assert!(!s.is_starred);
        assert_eq!(s.created_at, Some(1_792_600_000_000), "numeric strings are accepted");
        assert_eq!(s.last_activity_at, Some(1_792_600_000_123), "floats are rounded");
        assert_eq!(s.last_focused_at, None);
        assert_eq!(s.completed_turns, None, "negative counts are dropped");
        assert_eq!(s.context_exceeded_count, None);
        assert_eq!(s.turn_wrap_up, None);
        assert_eq!(s.last_turn_report, None, "blank text is dropped");
        assert_eq!(s.pr, None);
        assert_eq!(s.spawned_from, None);
        assert!(s.peer_receipts.is_empty());
        assert!(s.queued_notes.is_empty());
        assert_eq!(s.turn_summary, None);
        assert_eq!(s.error, None);
        assert_eq!(s.prior_error, None);
        assert_eq!(s.last_crash, None);
        assert!(!s.classifier_summary_enabled);
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-5"));
    }

    #[test]
    fn parses_task_suggestions() {
        let json = r#"{"sessionId":"local_s","backgroundTaskSuggestions":[
            {"id":"task_0000000a","title":" Réparer le cache ","prompt":"…","tldr":"Le cache expire trop tôt.","createdAt":1000},
            {"id":"task_0000000b","title":"Une offre","prompt":"","createdAt":2000,"expiresAt":"9000",
             "offer":true,"accepted":true,"toolUseId":"toolu_1","asked":"…"},
            {"id":"task_0000000c","title":"Dans le cloud","createdAt":3000,"autoStart":"cloud"},
            {"id":"task_0000000d","createdAt":4000},
            {"id":"task_0000000e","title":"Sans date"},
            {"title":"Sans id","createdAt":5000},
            "pas un objet"
        ]}"#;
        let s = parse_session(json).expect("parses");
        assert_eq!(
            s.suggestions,
            [
                TaskSuggestion {
                    id: "task_0000000a".into(),
                    title: "Réparer le cache".into(),
                    tldr: Some("Le cache expire trop tôt.".into()),
                    created_at: 1000,
                    ..TaskSuggestion::default()
                },
                TaskSuggestion {
                    id: "task_0000000b".into(),
                    title: "Une offre".into(),
                    created_at: 2000,
                    expires_at: Some(9000),
                    offer: true,
                    accepted: true,
                    ..TaskSuggestion::default()
                },
                TaskSuggestion {
                    id: "task_0000000c".into(),
                    title: "Dans le cloud".into(),
                    created_at: 3000,
                    auto_start: true,
                    ..TaskSuggestion::default()
                },
            ]
        );
        let none = parse_session(r#"{"sessionId":"local_s","backgroundTaskSuggestions":{}}"#);
        assert!(none.expect("parses").suggestions.is_empty());
    }

    #[test]
    fn missing_session_id_gives_none() {
        assert_eq!(parse_session(NO_SESSION_ID), None);
        assert_eq!(parse_session(r#"{"sessionId": 12}"#), None);
        assert_eq!(parse_session(r#"{"sessionId": "  "}"#), None);
    }

    #[test]
    fn bad_documents_give_none() {
        assert_eq!(parse_session(TRUNCATED), None);
        assert_eq!(parse_session(""), None);
        assert_eq!(parse_session("null"), None);
        assert_eq!(parse_session(r#"["local_x"]"#), None);
        assert_eq!(parse_session(r#"{"sessionId":"local_x"} trailing"#), None);
    }

    #[test]
    fn tolerates_bom_and_duplicate_keys() {
        let s = parse_session("\u{feff}{\"sessionId\":\"local_a\"}").expect("bom");
        assert_eq!(s.host_session_id, "local_a");
        let s = parse_session(r#"{"sessionId":"local_a","title":"old","title":"new"}"#)
            .expect("duplicate keys");
        assert_eq!(s.title.as_deref(), Some("new"));
    }

    #[test]
    fn pr_comes_from_cards_when_not_bound() {
        let s = parse_session(PR_CARDS).expect("pr cards fixture parses");
        assert_eq!(
            s.pr,
            Some(DesktopPrBinding {
                number: Some(12),
                url: Some("https://github.com/example/demo-app/pull/12".into()),
                repository: Some("example/demo-app".into()),
                state: Some("OPEN".into()),
            }),
            "the open card wins over the later merged one"
        );

        let merged_only = r#"{"sessionId":"local_m","prs":[
            {"prNumber":3,"url":"u3","repo":"o/r","state":"MERGED"},
            {"prNumber":4,"url":"u4","repo":"o/r","state":"MERGED"},
            {"prNumber":5,"url":"u5","repo":"o/r","state":"OPEN","dismissed":true}]}"#;
        let s = parse_session(merged_only).expect("parses");
        assert_eq!(s.pr.and_then(|p| p.number), Some(4));

        let all_dismissed = r#"{"sessionId":"local_d","prs":[
            {"prNumber":3,"url":"u3","state":"MERGED","dismissed":true}]}"#;
        assert_eq!(parse_session(all_dismissed).expect("parses").pr, None);

        let bound = r#"{"sessionId":"local_b","prNumber":9,"prs":[
            {"prNumber":3,"url":"u3","state":"OPEN"}]}"#;
        assert_eq!(parse_session(bound).expect("parses").pr.and_then(|p| p.number), Some(9));
    }

    #[test]
    fn latest_crash_picks_the_latest_entry() {
        let crashes = r#"{"sessionId":"local_e",
            "queryCrashes":[{"at":90,"errorCategory":"crash","errorMessage":"late"},
                            {"at":10,"errorCategory":"crash","errorMessage":"boom"},
                            {"at":"x"}, 7]}"#;
        let s = parse_session(crashes).expect("parses");
        assert_eq!(
            s.last_crash,
            Some(ErrorMark {
                at: Some(90),
                label: Some("crash: late".into()),
            })
        );

        let message_only = r#"{"sessionId":"local_e",
            "queryCrashes":[{"at":10,"errorMessage":"  line one\n  line two "}]}"#;
        let s = parse_session(message_only).expect("parses");
        assert_eq!(
            s.last_crash.and_then(|c| c.label).as_deref(),
            Some("line one line two")
        );

        // Only the current CLI session's crashes count, as in Desktop.
        let other_cli = r#"{"sessionId":"local_e","cliSessionId":"now",
            "queryCrashes":[{"at":90,"cliSessionId":"before","errorCategory":"crash"},
                            {"at":10,"cliSessionId":"now","errorCategory":"oom"},
                            {"at":5,"errorCategory":"untagged"}]}"#;
        let s = parse_session(other_cli).expect("parses");
        assert_eq!(s.last_crash.and_then(|c| c.label).as_deref(), Some("oom"));
        let only_other = r#"{"sessionId":"local_e","cliSessionId":"now",
            "queryCrashes":[{"at":90,"cliSessionId":"before","errorCategory":"crash"}]}"#;
        assert_eq!(parse_session(only_other).expect("parses").last_crash, None);
    }

    #[test]
    fn one_line_cuts_on_characters() {
        assert_eq!(one_line("a\n b\t c", 120), "a b c");
        let long = "é".repeat(200);
        let cut = one_line(&long, 120);
        assert_eq!(cut.chars().count(), 120);
        assert!(cut.ends_with('…'));
    }

    // -- DesktopIndex ------------------------------------------------------

    /// A temporary folder removed on drop.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "monitor-core-desktop-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn org(&self) -> PathBuf {
            let org = self
                .0
                .join("aaaaaaaa-0000-4000-8000-000000000000")
                .join("bbbbbbbb-0000-4000-8000-000000000000");
            fs::create_dir_all(&org).expect("create org dir");
            org
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn session_json(id: &str, title: &str) -> String {
        format!(r#"{{"sessionId":"{id}","title":"{title}","isArchived":false}}"#)
    }

    fn set_mtime(path: &Path, secs: u64) {
        let file = fs::File::options().write(true).open(path).expect("open for mtime");
        file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .expect("set mtime");
    }

    #[test]
    fn missing_root_is_an_error() {
        let tree = TempTree::new("missing");
        let mut index = DesktopIndex::new(tree.0.join("nope"));
        let error = index.refresh().expect_err("missing root");
        assert!(error.contains("introuvable"), "{error}");

        let file = tree.0.join("a-file");
        fs::write(&file, "x").expect("write");
        assert!(DesktopIndex::new(file).refresh().is_err());
    }

    #[test]
    fn empty_root_has_no_sessions() {
        let tree = TempTree::new("empty");
        let mut index = DesktopIndex::new(tree.0.clone());
        assert!(index.sessions().is_empty());
        assert_eq!(index.refresh(), Ok(false));
        assert!(index.sessions().is_empty());
    }

    #[test]
    fn refresh_tracks_added_modified_and_deleted_files() {
        let tree = TempTree::new("lifecycle");
        let org = tree.org();
        let mut index = DesktopIndex::new(tree.0.clone());

        // Unrelated files and folders at every level are ignored.
        fs::write(tree.0.join("local_root.json"), session_json("local_root", "r")).unwrap();
        fs::write(
            tree.0.join("aaaaaaaa-0000-4000-8000-000000000000").join("local_acct.json"),
            session_json("local_acct", "a"),
        )
        .unwrap();
        fs::write(org.join("archived-sessions.idx"), "idx").unwrap();
        fs::write(org.join("deleted_cccccccc-0000-4000-8000-000000000000"), "{}").unwrap();
        fs::write(org.join("scheduled-tasks.json"), "[]").unwrap();
        fs::write(org.join("local_x.json.tmp"), session_json("local_tmp", "t")).unwrap();
        fs::create_dir_all(org.join("backlog")).unwrap();
        fs::write(org.join("backlog").join("local_deep.json"), session_json("local_deep", "d"))
            .unwrap();
        fs::create_dir_all(org.join("waiting-input")).unwrap();
        fs::create_dir_all(org.join("local_dir.json")).unwrap();
        assert_eq!(index.refresh(), Ok(false));
        assert!(index.sessions().is_empty());

        // Add.
        let one = org.join("local_one.json");
        fs::write(&one, session_json("local_one", "First")).unwrap();
        assert_eq!(index.refresh(), Ok(true));
        assert_eq!(index.sessions().len(), 1);
        assert_eq!(index.sessions()["local_one"].title.as_deref(), Some("First"));

        // No change.
        assert_eq!(index.refresh(), Ok(false));

        // Modify.
        fs::write(&one, session_json("local_one", "First, renamed")).unwrap();
        assert_eq!(index.refresh(), Ok(true));
        assert_eq!(index.sessions()["local_one"].title.as_deref(), Some("First, renamed"));

        // A change limited to fields the monitor ignores is not reported.
        let with_extra = concat!(
            r#"{"sessionId":"local_one","title":"First, renamed","isArchived":false,"#,
            r#""toolSurfaceSnapshot":{"a":[1,2,3]}}"#
        );
        fs::write(&one, with_extra).unwrap();
        assert_eq!(index.refresh(), Ok(false));

        // Second session in another org.
        let other_org = tree.0.join("aaaaaaaa-0000-4000-8000-000000000000").join("org-2");
        fs::create_dir_all(&other_org).unwrap();
        fs::write(other_org.join("local_two.json"), session_json("local_two", "Second")).unwrap();
        assert_eq!(index.refresh(), Ok(true));
        assert_eq!(index.sessions().len(), 2);

        // Delete.
        fs::remove_file(&one).unwrap();
        assert_eq!(index.refresh(), Ok(true));
        assert_eq!(index.sessions().len(), 1);
        assert!(index.sessions().contains_key("local_two"));

        // Deleting a whole account folder drops its sessions.
        fs::remove_dir_all(tree.0.join("aaaaaaaa-0000-4000-8000-000000000000")).unwrap();
        assert_eq!(index.refresh(), Ok(true));
        assert!(index.sessions().is_empty());
    }

    #[test]
    fn unchanged_stamp_is_not_read_again() {
        let tree = TempTree::new("stamp");
        let org = tree.org();
        let path = org.join("local_s.json");
        fs::write(&path, session_json("local_s", "AAAA")).unwrap();
        set_mtime(&path, 1_700_000_000);
        let mut index = DesktopIndex::new(tree.0.clone());
        assert_eq!(index.refresh(), Ok(true));

        // Same length, same modification time: not re-read.
        fs::write(&path, session_json("local_s", "BBBB")).unwrap();
        set_mtime(&path, 1_700_000_000);
        assert_eq!(index.refresh(), Ok(false));
        assert_eq!(index.sessions()["local_s"].title.as_deref(), Some("AAAA"));

        // Only the time changes: re-read.
        set_mtime(&path, 1_700_000_001);
        assert_eq!(index.refresh(), Ok(true));
        assert_eq!(index.sessions()["local_s"].title.as_deref(), Some("BBBB"));
    }

    #[test]
    fn failed_parse_keeps_previous_value_and_retries() {
        let tree = TempTree::new("retry");
        let org = tree.org();
        let path = org.join("local_r.json");
        fs::write(&path, session_json("local_r", "Good")).unwrap();
        let mut index = DesktopIndex::new(tree.0.clone());
        assert_eq!(index.refresh(), Ok(true));

        // Half-written file: the previous value stays.
        let broken = r#"{"sessionId":"local_r","title":"Bet"#;
        fs::write(&path, broken).unwrap();
        set_mtime(&path, 1_700_000_000);
        assert_eq!(index.refresh(), Ok(false));
        assert_eq!(index.sessions()["local_r"].title.as_deref(), Some("Good"));

        // Still broken, same stamp: retried, still nothing new.
        assert_eq!(index.refresh(), Ok(false));

        // Completed with the same length and time as the broken content: the
        // broken stamp was not recorded, so the file is read again.
        let fixed = r#"{"sessionId":"local_r","title":"B"}"#;
        assert_eq!(fixed.len(), broken.len());
        fs::write(&path, fixed).unwrap();
        set_mtime(&path, 1_700_000_000);
        assert_eq!(index.refresh(), Ok(true));
        assert_eq!(index.sessions()["local_r"].title.as_deref(), Some("B"));

        // A new file that never parsed is not listed, then appears once fixed.
        let late = org.join("local_late.json");
        fs::write(&late, "{\"sessionId\":").unwrap();
        assert_eq!(index.refresh(), Ok(false));
        assert!(!index.sessions().contains_key("local_late"));
        fs::write(&late, session_json("local_late", "Late")).unwrap();
        assert_eq!(index.refresh(), Ok(true));
        assert!(index.sessions().contains_key("local_late"));

        // A file without sessionId is treated like a failed parse.
        fs::write(&late, r#"{"title":"no id"}"#).unwrap();
        assert_eq!(index.refresh(), Ok(false));
        assert_eq!(index.sessions()["local_late"].title.as_deref(), Some("Late"));
    }

    #[test]
    fn duplicate_ids_keep_the_most_recent_file() {
        let tree = TempTree::new("dup");
        let org = tree.org();
        let old = org.join("local_old.json");
        let new = org.join("local_new.json");
        fs::write(&old, session_json("local_same", "Old")).unwrap();
        fs::write(&new, session_json("local_same", "New")).unwrap();
        set_mtime(&old, 1_700_000_100);
        set_mtime(&new, 1_700_000_200);
        let mut index = DesktopIndex::new(tree.0.clone());
        assert_eq!(index.refresh(), Ok(true));
        assert_eq!(index.sessions().len(), 1);
        assert_eq!(index.sessions()["local_same"].title.as_deref(), Some("New"));

        // The older file becomes the newer one.
        fs::write(&old, session_json("local_same", "Old, edited")).unwrap();
        set_mtime(&old, 1_700_000_300);
        assert_eq!(index.refresh(), Ok(true));
        assert_eq!(index.sessions()["local_same"].title.as_deref(), Some("Old, edited"));

        // Removing it brings the other one back.
        fs::remove_file(&old).unwrap();
        assert_eq!(index.refresh(), Ok(true));
        assert_eq!(index.sessions()["local_same"].title.as_deref(), Some("New"));

        // A file saved again with the same content can still take over.
        fs::write(&old, session_json("local_same", "Old")).unwrap();
        set_mtime(&old, 1_700_000_100);
        assert_eq!(index.refresh(), Ok(false), "the newer file still wins");
        set_mtime(&old, 1_700_000_400);
        assert_eq!(index.refresh(), Ok(true));
        assert_eq!(index.sessions()["local_same"].title.as_deref(), Some("Old"));
    }

    /// Read-only check against the real Desktop folder:
    /// `cargo test --release desktop::tests::live -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the real Claude Desktop folder (read-only)"]
    fn live_desktop_folder() {
        let root = crate::paths::desktop_sessions_root();
        let mut index = DesktopIndex::new(root.clone());
        let started = Instant::now();
        let first = index.refresh();
        let first_time = started.elapsed();
        let started = Instant::now();
        let second = index.refresh();
        let second_time = started.elapsed();
        let mut steady = Duration::ZERO;
        for _ in 0..20 {
            let started = Instant::now();
            let _ = index.refresh();
            steady += started.elapsed();
        }

        let bytes: u64 = index
            .files
            .keys()
            .filter_map(|p| fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        let sessions = index.sessions();
        let count =
            |f: &dyn Fn(&DesktopSession) -> bool| sessions.values().filter(|s| f(s)).count();
        let files = index.files.len();
        let unparsed = index.files.values().filter(|c| c.session.is_none()).count();
        println!("root: {}", root.display());
        println!("first refresh: {first:?} in {first_time:?} ({files} files, {bytes} bytes)");
        println!("second refresh: {second:?} in {second_time:?}");
        println!("average unchanged refresh: {:?}", steady / 20);
        println!("sessions: {}", sessions.len());
        println!("archived: {}", count(&|s| s.is_archived));
        println!("with cli_session_id: {}", count(&|s| s.cli_session_id.is_some()));
        println!("with pr: {}", count(&|s| s.pr.is_some()));
        println!("with peer_receipts: {}", count(&|s| !s.peer_receipts.is_empty()));
        println!("with turn_wrap_up: {}", count(&|s| s.turn_wrap_up.is_some()));
        println!("with spawned_from: {}", count(&|s| s.spawned_from.is_some()));
        println!("with error: {}", count(&|s| s.error.is_some()));
        println!("with prior_error (turn pending): {}", count(&|s| s.prior_error.is_some()));
        println!("with last_crash: {}", count(&|s| s.last_crash.is_some()));
        println!("with turn_summary: {}", count(&|s| s.turn_summary.is_some()));
        println!("with queued notes: {}", count(&|s| !s.queued_notes.is_empty()));
        println!("unparsed files: {unparsed}");
        match sessions.get("local_cc5b463c-4454-4fdc-b373-a46663f78260") {
            Some(s) => println!(
                "local_cc5b463c…: title {:?}, {} peer receipts",
                s.title,
                s.peer_receipts.len()
            ),
            None => println!("local_cc5b463c…: not found"),
        }
        assert!(first.is_ok());
        assert_eq!(second, Ok(false));
    }
}
