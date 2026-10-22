//! Incremental reading of `~/.claude/projects/<dir>/<cliSessionId>.jsonl`.
//!
//! Each transcript is read once from the start, then only the complete lines
//! appended since the previous call. Lines that cannot matter (API request
//! dumps, file-history snapshots, titles…) are skipped from their first bytes
//! without being parsed or even buffered.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::model::{
    ErrorMark, PendingQuestion, PlanItem, PlanProgress, ToolActivity, TranscriptInfo, TurnInfo,
    TurnOrigin,
};

/// Minimum delay between two searches for a transcript that was not found.
const RESCAN_INTERVAL: Duration = Duration::from_secs(10);
/// Maximum length, in characters, of a tool summary.
const SUMMARY_MAX: usize = 120;
/// Maximum length, in characters, of the latest assistant text.
const TEXT_MAX: usize = 200;
/// Maximum length, in characters, of a question or a wrap-up recap.
const QUESTION_MAX: usize = 300;
/// Maximum length, in characters, of a wrap-up recap.
const RECAP_MAX: usize = 400;
/// Characters of the closing message kept to find its final question.
const CLOSING_KEEP: usize = 4_000;
/// Longest error message kept, in characters.
const ERROR_MAX: usize = 120;
/// `wrap_up` (Desktop's end-of-turn tool), any MCP server prefix.
const WRAP_UP_SUFFIX: &str = "ccd_turn__wrap_up";

/// Which origin names a turn several prompts started together.
fn origin_rank(origin: TurnOrigin) -> u8 {
    match origin {
        TurnOrigin::Human => 2,
        TurnOrigin::Task => 1,
        TurnOrigin::Peer => 0,
    }
}
/// Tool calls without a result kept in memory; older ones are forgotten.
const MAX_OPEN_TOOLS: usize = 64;
/// Desktop keeps at most this many `update_plan` steps.
const UPDATE_PLAN_MAX_ITEMS: usize = 12;
/// Bytes read before deciding whether a line can be skipped.
const PEEK_LEN: usize = 64;
/// Read buffer size.
const READ_BUFFER: usize = 64 * 1024;

/// Keeps a read offset and the rebuilt state per transcript.
pub struct TranscriptTracker {
    projects_dir: PathBuf,
    files: HashMap<String, TrackedFile>,
    /// When each transcript that could not be found was last searched for.
    failed_lookups: HashMap<String, Instant>,
}

impl TranscriptTracker {
    /// Tracker for the transcripts under `projects_dir`, normally
    /// [`crate::paths::projects_dir`].
    pub fn new(projects_dir: PathBuf) -> Self {
        Self {
            projects_dir,
            files: HashMap::new(),
            failed_lookups: HashMap::new(),
        }
    }

    /// Reads the lines appended since the previous call and returns the
    /// session's current view. None when no transcript is found.
    pub fn update(&mut self, cli_session_id: &str) -> Option<TranscriptInfo> {
        if !is_valid_session_id(cli_session_id) {
            return None;
        }
        if let Some(tracked) = self.files.get_mut(cli_session_id) {
            match tracked.read_new_lines() {
                Ok(()) => return Some(tracked.info()),
                // The file moved or was deleted: look for it again below.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    self.files.remove(cli_session_id);
                }
                // Sharing violation or another transient error: keep the last view.
                Err(_) => return Some(tracked.info()),
            }
        }
        let path = self.locate(cli_session_id)?;
        let mut tracked = TrackedFile::new(path);
        let result = tracked.read_new_lines();
        let info = tracked.info();
        match result {
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            _ => {
                self.files.insert(cli_session_id.to_owned(), tracked);
                Some(info)
            }
        }
    }

    /// Forgets the transcripts of sessions that are no longer live.
    pub fn retain(&mut self, live_cli_ids: &HashSet<String>) {
        self.files.retain(|id, _| live_cli_ids.contains(id));
        self.failed_lookups.retain(|id, _| live_cli_ids.contains(id));
    }

    /// Finds `projects/*/<id>.jsonl`, at most once every [`RESCAN_INTERVAL`]
    /// while it cannot be found.
    fn locate(&mut self, id: &str) -> Option<PathBuf> {
        if let Some(at) = self.failed_lookups.get(id)
            && at.elapsed() < RESCAN_INTERVAL
        {
            return None;
        }
        match find_transcript(&self.projects_dir, id) {
            Some(path) => {
                self.failed_lookups.remove(id);
                Some(path)
            }
            None => {
                self.failed_lookups.insert(id.to_owned(), Instant::now());
                None
            }
        }
    }
}

/// Session ids are UUIDs; anything else could escape the projects folder.
fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Scans the project folders for `<id>.jsonl`, the most recently modified
/// one winning if several folders hold it.
fn find_transcript(projects_dir: &Path, id: &str) -> Option<PathBuf> {
    let file_name = format!("{id}.jsonl");
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(projects_dir).ok()?.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let candidate = entry.path().join(&file_name);
        let Ok(meta) = fs::metadata(&candidate) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| modified > *t) {
            best = Some((modified, candidate));
        }
    }
    best.map(|(_, path)| path)
}

/// One transcript being followed.
struct TrackedFile {
    path: PathBuf,
    /// Byte offset just after the last complete line consumed.
    offset: u64,
    state: TranscriptState,
}

impl TrackedFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            state: TranscriptState::default(),
        }
    }

    fn reset(&mut self) {
        self.offset = 0;
        self.state = TranscriptState::default();
    }

    /// Consumes the complete lines appended since the last call. A last line
    /// without its `\n` is left for the next call.
    fn read_new_lines(&mut self) -> io::Result<()> {
        let mut file = File::open(&self.path)?;
        let len = file.metadata()?.len();
        if len < self.offset {
            self.reset();
        }
        if len == self.offset {
            return Ok(());
        }
        if self.offset > 0 {
            // The byte before the offset must still end a line, else the file
            // was rewritten and everything must be read again.
            file.seek(SeekFrom::Start(self.offset - 1))?;
            let mut previous = [0u8; 1];
            file.read_exact(&mut previous)?;
            if previous[0] != b'\n' {
                self.reset();
                file.seek(SeekFrom::Start(0))?;
            }
        }
        let mut reader = BufReader::with_capacity(READ_BUFFER, file);
        let mut line = Vec::new();
        loop {
            match next_line(&mut reader, &mut line)? {
                NextLine::Complete(n) => {
                    self.offset += n;
                    self.state.apply_line(&line);
                }
                NextLine::Skipped(n) => self.offset += n,
                NextLine::Partial => return Ok(()),
            }
        }
    }

    fn info(&self) -> TranscriptInfo {
        self.state.info(&self.path)
    }
}

enum NextLine {
    /// A complete line, now in the buffer; holds its length with the `\n`.
    Complete(u64),
    /// A complete line not worth parsing; holds its length with the `\n`.
    Skipped(u64),
    /// End of file reached before the next `\n`.
    Partial,
}

/// Reads the next line into `line`. Lines that [`is_skippable`] are consumed
/// without being kept in memory, however long they are.
fn next_line<R: BufRead>(reader: &mut R, line: &mut Vec<u8>) -> io::Result<NextLine> {
    line.clear();
    let mut consumed = 0u64;
    let mut decided = false;
    let mut skipping = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(NextLine::Partial);
        }
        let (take, complete) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if !skipping {
            line.extend_from_slice(&available[..take]);
            if !decided && (complete || line.len() >= PEEK_LEN) {
                decided = true;
                if is_skippable(line) {
                    skipping = true;
                    line.clear();
                }
            }
        }
        reader.consume(take);
        consumed += take as u64;
        if complete {
            return Ok(if skipping {
                NextLine::Skipped(consumed)
            } else {
                NextLine::Complete(consumed)
            });
        }
    }
}

/// True for lines whose `type`, written first, is not one the tracker reads.
/// Conversation lines start with `{"parentUuid"` and are never skipped here.
fn is_skippable(prefix: &[u8]) -> bool {
    let Some(rest) = prefix.trim_ascii_start().strip_prefix(b"{\"type\":\"") else {
        return false;
    };
    let Some(end) = rest.iter().position(|&b| b == b'"') else {
        return false;
    };
    !is_conversation_type(&rest[..end])
}

fn is_conversation_type(kind: &[u8]) -> bool {
    matches!(kind, b"user" | b"assistant" | b"attachment" | b"system")
}

// ---------------------------------------------------------------------------
// State rebuilt from the lines
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct TranscriptState {
    last_event_at: Option<i64>,
    git_branch: Option<String>,
    last_assistant_text: Option<String>,
    last_tool: Option<ToolActivity>,
    /// Tool calls without a result yet, oldest first.
    open_tools: Vec<OpenTool>,
    /// API message id of the latest main-chain assistant line.
    last_message_id: Option<String>,
    plan: Option<Plan>,
    /// The latest plan-changing call and the plan before it, restored if
    /// that call's result is an error (e.g. `items` sent without `scope`).
    plan_undo: Option<(String, Option<Plan>)>,
    /// The latest turn.
    turn: Option<Turn>,
    /// Number of turns started so far.
    turn_count: u64,
    last_human_prompt_at: Option<i64>,
    notable_turn_ended_at: Option<i64>,
    pending_ask: Option<PendingQuestion>,
    /// The latest `wrap_up` call and what it replaced, restored if Desktop
    /// rejects the call.
    ask_undo: Option<AskUndo>,
    /// API error of a turn, with the number of that turn.
    api_error: Option<(ErrorMark, u64)>,
}

/// A tool call waiting for its result.
#[derive(Debug, Clone)]
struct OpenTool {
    id: String,
    /// API message the call belongs to; calls made in parallel share it.
    message_id: Option<String>,
    activity: ToolActivity,
}

/// The latest turn while it is being read.
#[derive(Debug, Clone, Default)]
struct Turn {
    started_at: Option<i64>,
    origin: TurnOrigin,
    ended: bool,
    ended_at: Option<i64>,
    /// Ended by a final API error rather than a stop marker.
    ended_by_error: bool,
    wrapped_up: bool,
    wrap_up_recap: Option<String>,
    /// Raw text written after the last tool call (the end is kept).
    closing_text: String,
    /// The assistant wrote something in this turn.
    answered: bool,
}

#[derive(Debug, Clone)]
struct AskUndo {
    id: String,
    pending_ask: Option<PendingQuestion>,
    wrapped_up: bool,
    wrap_up_recap: Option<String>,
}

/// What a main-chain user line (not a tool result) means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prompt {
    /// Starts a turn.
    Start(TurnOrigin),
    /// "[Request interrupted by user]": ends the running turn.
    Interrupt,
}

impl TranscriptState {
    fn apply_line(&mut self, bytes: &[u8]) {
        let Ok(line) = serde_json::from_slice::<Line>(bytes) else {
            return;
        };
        let Some(kind) = line.kind.clone() else {
            return;
        };
        if !is_conversation_type(kind.as_bytes()) {
            return;
        }
        let at = line.timestamp.as_deref().and_then(parse_iso8601_ms);
        if let Some(t) = at {
            self.last_event_at = Some(self.last_event_at.map_or(t, |prev| prev.max(t)));
        }
        if line.is_sidechain {
            return;
        }
        if let Some(branch) = line.git_branch.as_deref()
            && !branch.is_empty()
            && branch != "HEAD"
        {
            self.git_branch = Some(branch.to_owned());
        }
        match kind.as_str() {
            "assistant" => self.apply_assistant(line, at),
            "user" => self.apply_user(line, at),
            "system" => self.apply_system(&line, at),
            _ => {}
        }
    }

    fn apply_assistant(&mut self, line: Line, at: Option<i64>) {
        // The CLI calls the API again only once every result of the previous
        // message is in: calls of an older message cannot still be running.
        if let Some(id) = line.message_id.as_deref()
            && self.last_message_id.as_deref() != Some(id)
        {
            self.open_tools
                .retain(|t| t.message_id.as_deref().is_none_or(|m| m == id));
            self.last_message_id = Some(id.to_owned());
        }
        if line.is_api_error {
            self.on_api_error(&line, at);
            return;
        }
        self.on_assistant_progress();
        match line.content {
            Content::None => {}
            Content::Text(text) => self.on_text(&text),
            Content::Blocks(blocks) => {
                for block in blocks {
                    match block.kind.as_deref() {
                        Some("text") => {
                            if let Some(text) = block.text {
                                self.on_text(&text);
                            }
                        }
                        Some("tool_use") => self.on_tool_use(block, at),
                        _ => {}
                    }
                }
            }
        }
    }

    /// A final API error (rate limit, expired login…): the turn stops there.
    fn on_api_error(&mut self, line: &Line, at: Option<i64>) {
        let message = match &line.content {
            Content::Text(text) => Some(text.clone()),
            Content::Blocks(blocks) => blocks
                .iter()
                .filter(|b| b.kind.as_deref() == Some("text"))
                .find_map(|b| b.text.clone()),
            Content::None => None,
        };
        if let Some(message) = &message {
            self.set_text(message);
        }
        let message = message.map(|m| one_line(&m, ERROR_MAX)).filter(|m| !m.is_empty());
        let label = match (line.error.as_deref().map(str::trim), message) {
            (Some(kind), Some(message)) if !kind.is_empty() => Some(format!("{kind}: {message}")),
            (Some(kind), None) if !kind.is_empty() => Some(kind.to_owned()),
            (_, message) => message,
        };
        self.api_error = Some((ErrorMark { at, label }, self.turn_count));
        self.open_tools.clear();
        let turn = self.turn.get_or_insert_with(Turn::default);
        turn.answered = true;
        if !turn.ended {
            turn.ended = true;
            turn.ended_at = at;
            turn.ended_by_error = true;
        }
    }

    /// A normal assistant line: an earlier API error was overcome (the
    /// session answers again), and a turn that looked over is running again.
    fn on_assistant_progress(&mut self) {
        if let Some(turn) = self.turn.as_mut() {
            turn.answered = true;
        }
        self.api_error = None;
        if let Some(turn) = self.turn.as_mut()
            && turn.ended
        {
            turn.ended = false;
            turn.ended_at = None;
            turn.ended_by_error = false;
        }
    }

    fn apply_user(&mut self, line: Line, at: Option<i64>) {
        if let Content::Blocks(blocks) = &line.content
            && blocks.iter().any(|b| b.kind.as_deref() == Some("tool_result"))
        {
            for block in blocks {
                if block.kind.as_deref() == Some("tool_result")
                    && let Some(id) = block.tool_use_id.as_deref()
                {
                    self.on_tool_result(id, block.is_error);
                }
            }
            return;
        }
        match classify_prompt(&line) {
            Some(Prompt::Start(origin)) => self.start_turn(origin, at),
            Some(Prompt::Interrupt) => self.end_turn(at),
            None => {}
        }
    }

    fn apply_system(&mut self, line: &Line, at: Option<i64>) {
        if line.subtype.as_deref() == Some("stop_hook_summary") && !line.prevented_continuation {
            self.end_turn(at);
        }
    }

    fn start_turn(&mut self, origin: TurnOrigin, at: Option<i64>) {
        self.open_tools.clear();
        match self.turn.as_mut() {
            // Prompts delivered together (a task notification and a peer
            // message in the same instant) make one turn; the person's, or
            // else the session's own task, names it.
            Some(turn) if !turn.ended && !turn.answered => {
                if origin_rank(origin) > origin_rank(turn.origin) {
                    turn.origin = origin;
                }
            }
            _ => {
                self.turn_count += 1;
                self.turn = Some(Turn {
                    started_at: at,
                    origin,
                    ..Turn::default()
                });
            }
        }
        if origin == TurnOrigin::Human {
            self.last_human_prompt_at = at.or(self.last_human_prompt_at);
            self.pending_ask = None;
            self.ask_undo = None;
            self.api_error = None;
        }
    }

    /// A stop marker or an interruption: the running turn is over.
    fn end_turn(&mut self, at: Option<i64>) {
        self.open_tools.clear();
        let turn = self.turn.get_or_insert_with(Turn::default);
        turn.ended = true;
        turn.ended_at = at.or(turn.ended_at);
        turn.ended_by_error = false;
        // Another session's ping that only got a short answer needs nobody;
        // one that did work ends with a wrap-up, whose result is news.
        if turn.origin != TurnOrigin::Peer || turn.wrapped_up {
            self.notable_turn_ended_at = turn.ended_at.or(self.notable_turn_ended_at);
        }
    }

    fn on_text(&mut self, text: &str) {
        self.set_text(text);
        if let Some(turn) = self.turn.as_mut() {
            if !turn.closing_text.is_empty() {
                turn.closing_text.push('\n');
            }
            turn.closing_text.push_str(text);
            keep_tail(&mut turn.closing_text, CLOSING_KEEP);
        }
    }

    fn set_text(&mut self, text: &str) {
        let text = plain_text(text, TEXT_MAX);
        if !text.is_empty() {
            self.last_assistant_text = Some(text);
        }
    }

    fn on_tool_use(&mut self, block: Block, at: Option<i64>) {
        let Some(name) = block.name.filter(|n| !n.is_empty()) else {
            return;
        };
        if let Some(turn) = self.turn.as_mut() {
            turn.closing_text.clear();
        }
        let input = block.input.unwrap_or(Value::Null);
        let activity = ToolActivity {
            name: tool_display_name(&name).to_owned(),
            summary: summarize_tool(&name, &input),
            at,
        };
        self.last_tool = Some(activity.clone());
        let id = block.id.unwrap_or_default();
        if !id.is_empty() {
            self.open_tools.retain(|t| t.id != id);
            self.open_tools.push(OpenTool {
                id: id.clone(),
                message_id: self.last_message_id.clone(),
                activity,
            });
            if self.open_tools.len() > MAX_OPEN_TOOLS {
                self.open_tools.remove(0);
            }
        }
        if is_wrap_up(&name) {
            self.on_wrap_up(&id, &input, at);
        }
        self.apply_plan_tool(&id, &name, &input, at);
    }

    /// `wrap_up {recap, ask?, blocked?, actions?}`: records the recap and the
    /// question it leaves to the person. A wrap-up without question says
    /// nothing waits on the person.
    fn on_wrap_up(&mut self, id: &str, input: &Value, at: Option<i64>) {
        let turn = self.turn.get_or_insert_with(Turn::default);
        if !id.is_empty() {
            self.ask_undo = Some(AskUndo {
                id: id.to_owned(),
                pending_ask: self.pending_ask.clone(),
                wrapped_up: turn.wrapped_up,
                wrap_up_recap: turn.wrap_up_recap.clone(),
            });
        }
        turn.wrapped_up = true;
        turn.wrap_up_recap = first_text(input, &["recap", "outcome"])
            .map(|r| plain_text(r, RECAP_MAX))
            .filter(|r| !r.is_empty());
        self.pending_ask = wrap_up_question(input, at);
    }

    fn on_tool_result(&mut self, id: &str, is_error: bool) {
        self.open_tools.retain(|t| t.id != id);
        if let Some((undo_id, previous)) = self.plan_undo.take() {
            if undo_id == id {
                if is_error {
                    self.plan = previous;
                }
            } else {
                self.plan_undo = Some((undo_id, previous));
            }
        }
        if let Some(undo) = self.ask_undo.take() {
            if undo.id != id {
                self.ask_undo = Some(undo);
            } else if is_error {
                self.pending_ask = undo.pending_ask;
                if let Some(turn) = self.turn.as_mut() {
                    turn.wrapped_up = undo.wrapped_up;
                    turn.wrap_up_recap = undo.wrap_up_recap;
                }
            }
        }
    }

    fn apply_plan_tool(&mut self, id: &str, name: &str, input: &Value, at: Option<i64>) {
        let Some(op) = PlanOp::from_tool_name(name) else {
            return;
        };
        let before = self.plan.clone();
        let changed = match op {
            PlanOp::UpdatePlan => self.plan_update(input),
            PlanOp::WrapUp => self.plan_wrap_up(),
            PlanOp::TodoWrite => self.plan_todo_write(input),
            PlanOp::TaskCreate => self.plan_task_create(input),
            PlanOp::TaskUpdate => self.plan_task_update(input),
        };
        if !changed {
            return;
        }
        // A wrap-up changes no status: the list still belongs to the turn
        // that last changed it (and stays hidden after a newer prompt).
        if !matches!(op, PlanOp::WrapUp)
            && let Some(plan) = self.plan.as_mut()
            && at.is_some()
        {
            plan.updated_at = at;
        }
        self.plan_undo = (!id.is_empty()).then(|| (id.to_owned(), before));
    }

    /// `mcp__ccd_turn__update_plan`, as Desktop applies it: `items` replaces
    /// the list (exact "completed" / "in_progress" statuses, anything else
    /// pending; items without text skipped; at most 12), `done` completes
    /// items, `start` puts one item in progress, then `advance` completes the
    /// first item in progress and starts the next pending one. `start` leaves
    /// other items in progress as they are: Desktop itself answers "steps 4, 5
    /// of 7 are in progress" in that case. Any of them reopens a wrapped-up
    /// list, continuing from its real statuses.
    fn plan_update(&mut self, input: &Value) -> bool {
        let mut changed = false;
        if let Some(items) = input.get("items").and_then(Value::as_array) {
            self.plan = Some(Plan {
                items: items
                    .iter()
                    .filter_map(parse_update_plan_item)
                    .take(UPDATE_PLAN_MAX_ITEMS)
                    .collect(),
                ..Plan::default()
            });
            changed = true;
        }
        let Some(plan) = self.plan.as_mut() else {
            return changed;
        };
        if let Some(done) = input.get("done") {
            for index in one_based_indices(done) {
                if let Some(item) = plan.items.get_mut(index) {
                    item.status = Status::Completed;
                    changed = true;
                }
            }
        }
        if let Some(index) = input.get("start").and_then(one_based_index)
            && let Some(item) = plan.items.get_mut(index)
        {
            item.status = Status::InProgress;
            changed = true;
        }
        if input.get("advance").is_some_and(is_truthy) {
            plan.advance();
            changed = true;
        }
        if changed {
            plan.wrapped_up = false;
        }
        changed
    }

    /// `mcp__ccd_turn__wrap_up`: the turn is over. Like Desktop, it changes no
    /// status (an `advance` sent after it continues the list, "step 3 of 4");
    /// the flag is information only.
    fn plan_wrap_up(&mut self) -> bool {
        let Some(plan) = self.plan.as_mut() else {
            return false;
        };
        plan.wrapped_up = true;
        true
    }

    /// CLI `TodoWrite {todos: [{content, status, activeForm}]}`: full replacement.
    fn plan_todo_write(&mut self, input: &Value) -> bool {
        let Some(todos) = input.get("todos").and_then(Value::as_array) else {
            return false;
        };
        self.plan = Some(Plan {
            items: todos.iter().filter_map(parse_plan_item).collect(),
            ..Plan::default()
        });
        true
    }

    /// CLI `TaskCreate {subject}`: appends task `#n`, numbered from 1.
    fn plan_task_create(&mut self, input: &Value) -> bool {
        let Some(subject) = str_field(input, "subject").or_else(|| str_field(input, "description"))
        else {
            return false;
        };
        if !self.plan.as_ref().is_some_and(|p| p.from_tasks) {
            self.plan = Some(Plan {
                from_tasks: true,
                ..Plan::default()
            });
        }
        let Some(plan) = self.plan.as_mut() else {
            return false;
        };
        plan.next_task_id += 1;
        plan.items.push(Item {
            task_id: Some(plan.next_task_id.to_string()),
            content: subject.trim().to_owned(),
            status: Status::Pending,
        });
        plan.wrapped_up = false;
        true
    }

    /// CLI `TaskUpdate {taskId, status?, subject?}`; status "deleted" removes it.
    fn plan_task_update(&mut self, input: &Value) -> bool {
        let Some(plan) = self.plan.as_mut().filter(|p| p.from_tasks) else {
            return false;
        };
        let Some(task_id) = input.get("taskId").and_then(id_string) else {
            return false;
        };
        let Some(pos) = plan
            .items
            .iter()
            .position(|i| i.task_id.as_deref() == Some(task_id.as_str()))
        else {
            return false;
        };
        let status = str_field(input, "status");
        if status == Some("deleted") {
            plan.items.remove(pos);
            return true;
        }
        let item = &mut plan.items[pos];
        if let Some(status) = status {
            item.status = Status::parse(status);
        }
        if let Some(subject) = str_field(input, "subject") {
            item.content = subject.trim().to_owned();
        }
        plan.wrapped_up = false;
        true
    }

    fn info(&self, path: &Path) -> TranscriptInfo {
        TranscriptInfo {
            path: path.to_string_lossy().into_owned(),
            last_event_at: self.last_event_at,
            current_tool: self.open_tools.last().map(|t| t.activity.clone()),
            last_tool: self.last_tool.clone(),
            plan: self
                .plan
                .as_ref()
                .filter(|plan| !plan.predates(self.last_human_prompt_at))
                .map(Plan::progress),
            git_branch: self.git_branch.clone(),
            last_assistant_text: self.last_assistant_text.clone(),
            turn: self.turn.as_ref().map(|turn| TurnInfo {
                started_at: turn.started_at,
                origin: turn.origin,
                ended: turn.ended,
                ended_at: turn.ended_at,
                wrapped_up: turn.wrapped_up,
                wrap_up_recap: turn.wrap_up_recap.clone(),
                closing_question: closing_question(&turn.closing_text),
            }),
            last_human_prompt_at: self.last_human_prompt_at,
            notable_turn_ended_at: self.notable_turn_ended_at,
            pending_ask: self.pending_ask.clone(),
            api_error: self.api_error.as_ref().map(|(error, _)| error.clone()),
        }
    }
}

/// `mcp__<server>__ccd_turn__wrap_up` or `mcp__ccd_turn__wrap_up`.
fn is_wrap_up(name: &str) -> bool {
    name.starts_with("mcp__") && name.ends_with(WRAP_UP_SUFFIX)
}

/// What a main-chain user line that is not a tool result means.
///
/// - `origin.kind` (newer CLIs) decides: "human" is the person,
///   "task-notification" a background task of the session, anything else
///   (peer message, subagent hand-back…) another session.
/// - Without it: meta lines (skill bodies, image notes), compaction
///   summaries and local commands start nothing, except a meta
///   "[Cross-session idle notice]". Leading `<system-reminder>` blocks are
///   set aside and what follows decides: nothing, `<side-sessions-event>` or
///   `<cross-session-message>` is another session, `<task-notification>` a
///   background task; "[Request interrupted by user]" ends the turn; any
///   other text (typed, or the brief a parent gives its new session) is the
///   person.
fn classify_prompt(line: &Line) -> Option<Prompt> {
    match line.origin_kind.as_deref().map(str::trim) {
        Some("human") => return Some(Prompt::Start(TurnOrigin::Human)),
        Some("task-notification") => return Some(Prompt::Start(TurnOrigin::Task)),
        Some(kind) if !kind.is_empty() => return Some(Prompt::Start(TurnOrigin::Peer)),
        _ => {}
    }
    if line.is_compact_summary || line.is_visible_only {
        return None;
    }
    let (first_text, has_blocks) = match &line.content {
        Content::None => return None,
        Content::Text(text) => (text.as_str(), false),
        Content::Blocks(blocks) => (
            blocks
                .iter()
                .filter(|b| b.kind.as_deref() == Some("text"))
                .find_map(|b| b.text.as_deref())
                .unwrap_or(""),
            !blocks.is_empty(),
        ),
    };
    let text = first_text.trim_start();
    if line.is_meta {
        // Skill bodies, image notes, mid-turn reminders: part of the turn.
        // Older CLIs wake a session for another's idle notice this way.
        return text
            .starts_with("[Cross-session idle notice]")
            .then_some(Prompt::Start(TurnOrigin::Peer));
    }
    let had_reminder = text.starts_with(REMINDER_OPEN);
    let Some(text) = after_reminders(text) else {
        // A reminder that never closes: a wake-up note and nothing else.
        return Some(Prompt::Start(TurnOrigin::Peer));
    };
    if text.starts_with("<task-notification>") {
        Some(Prompt::Start(TurnOrigin::Task))
    } else if (had_reminder && text.is_empty())
        || text.starts_with("<cross-session-message")
        || text.starts_with("<side-sessions-event")
    {
        Some(Prompt::Start(TurnOrigin::Peer))
    } else if text.starts_with("<command-")
        || text.starts_with("<local-command-")
        || (text.is_empty() && !has_blocks)
    {
        None
    } else if text.starts_with("[Request interrupted by user") {
        Some(Prompt::Interrupt)
    } else {
        Some(Prompt::Start(TurnOrigin::Human))
    }
}

const REMINDER_OPEN: &str = "<system-reminder>";
const REMINDER_CLOSE: &str = "</system-reminder>";

/// `text` without its leading `<system-reminder>` blocks; None when one of
/// them never closes.
fn after_reminders(mut text: &str) -> Option<&str> {
    while let Some(rest) = text.strip_prefix(REMINDER_OPEN) {
        let end = rest.find(REMINDER_CLOSE)?;
        text = rest[end + REMINDER_CLOSE.len()..].trim_start();
    }
    Some(text)
}

/// French label of a `wrap_up` app action (a button without `label`).
fn app_action_label(action: &str) -> Option<&'static str> {
    Some(match action.trim() {
        "commit" => "Commiter",
        "create_pr" => "Ouvrir la PR",
        "merge" => "Fusionner",
        "fix_ci" => "Corriger la CI",
        "address_comments" => "Traiter les commentaires",
        "resolve_conflicts" => "Résoudre les conflits",
        "verify" => "Vérifier",
        "archive" => "Archiver",
        "new_session" => "Nouvelle session",
        _ => return None,
    })
}

/// The question a `wrap_up` leaves to the person: its `ask` (or `next_ask`,
/// `needs_user`), or, when it says `blocked`, the first sentence of its
/// recap. Desktop's buttons are the options: `actions[].label`, or the
/// French name of an app action (`{"action": "commit"}`).
fn wrap_up_question(input: &Value, at: Option<i64>) -> Option<PendingQuestion> {
    let blocked = match input.get("blocked") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.trim() == "true",
        _ => false,
    };
    let asked = first_text(input, &["ask", "next_ask", "needs_user"]);
    let text = match asked {
        Some(ask) => ask.to_owned(),
        None if blocked => first_sentence(first_text(input, &["recap", "outcome"])?).to_owned(),
        None => return None,
    };
    let text = plain_text(&text, QUESTION_MAX);
    if text.is_empty() {
        return None;
    }
    let options = input
        .get("actions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|action| match action {
            Value::String(label) => Some(label.as_str()),
            other => first_text(other, &["label"])
                .or_else(|| str_field(other, "action").and_then(app_action_label)),
        })
        .map(|label| plain_text(label, SUMMARY_MAX))
        .filter(|label| !label.is_empty())
        .collect();
    Some(PendingQuestion {
        text,
        options,
        source: "wrap_up".to_owned(),
        at,
    })
}

/// The first of `keys` holding a non-blank string.
fn first_text<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .filter_map(|k| str_field(value, k))
        .find(|s| !s.trim().is_empty())
}

/// Text up to the first ".", "!" or "?" followed by a space or the end.
fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if matches!(c, '.' | '!' | '?')
            && chars.peek().is_none_or(|(_, next)| next.is_whitespace())
        {
            return &text[..i + c.len_utf8()];
        }
    }
    text
}

/// Keeps the last `max` bytes of `text`, cut on a character boundary.
fn keep_tail(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text.drain(..start);
}

/// The final question of a closing message: its last sentence when the
/// message ends with a question mark (after trailing spaces, quotes,
/// closing brackets, Markdown markers and emoji). Plain text, one line.
fn closing_question(closing: &str) -> Option<String> {
    let last_line = closing
        .lines()
        .rev()
        .map(|line| plain_text(line, usize::MAX))
        .find(|line| !line.is_empty())?;
    let trimmed = last_line.trim_end_matches(is_trailing_decoration);
    if !trimmed.ends_with(['?', '？']) {
        return None;
    }
    let body = &trimmed[..trimmed.len() - trimmed.chars().next_back().map_or(0, char::len_utf8)];
    // Start of the last sentence: after the latest ". ", "! " or "? ".
    let start = body
        .char_indices()
        .rev()
        .find(|&(i, c)| {
            matches!(c, '.' | '!' | '?' | '…')
                && body[i + c.len_utf8()..].starts_with(char::is_whitespace)
        })
        .map_or(0, |(i, c)| i + c.len_utf8());
    let sentence = trimmed[start..].trim_start_matches(is_leading_decoration);
    // A very short tail ("Ok ?") reads better with its line.
    let question = if sentence.chars().count() < 12 {
        trimmed.trim_start_matches(is_leading_decoration)
    } else {
        sentence
    };
    let question = one_line(question, QUESTION_MAX);
    (!question.is_empty()).then_some(question)
}

/// Characters that may follow a final question mark without changing it.
fn is_trailing_decoration(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '«' | '»' | '“' | '”' | '‘' | '’' | ')' | ']' | '}' | '*' | '_' | '`'
                | '~'
        )
        || is_emoji(c)
}

/// Characters that may open a question without being part of it.
fn is_leading_decoration(c: char) -> bool {
    c.is_whitespace()
        || matches!(c, '"' | '\'' | '«' | '“' | '‘' | '(' | '[' | '{' | '*' | '_' | '`' | '~')
}

/// Emoji, pictographs and the joiners and modifiers that build them.
fn is_emoji(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0x2300..=0x23FF
            | 0xFE00..=0xFE0F | 0x200D | 0x20E3 | 0xE0020..=0xE007F
    )
}

#[derive(Debug, Clone, Copy)]
enum PlanOp {
    UpdatePlan,
    WrapUp,
    TodoWrite,
    TaskCreate,
    TaskUpdate,
}

impl PlanOp {
    fn from_tool_name(name: &str) -> Option<Self> {
        if name.starts_with("mcp__") && name.ends_with("ccd_turn__update_plan") {
            return Some(Self::UpdatePlan);
        }
        if is_wrap_up(name) {
            return Some(Self::WrapUp);
        }
        match name {
            "TodoWrite" => Some(Self::TodoWrite),
            "TaskCreate" => Some(Self::TaskCreate),
            "TaskUpdate" => Some(Self::TaskUpdate),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Plan {
    items: Vec<Item>,
    /// Built by `TaskCreate`, whose items are addressed by their number.
    from_tasks: bool,
    next_task_id: u32,
    updated_at: Option<i64>,
    wrapped_up: bool,
}

impl Plan {
    /// Completes the first item in progress and starts the next pending one
    /// (searching after it first, then from the top). With nothing in
    /// progress, starts the first pending item.
    fn advance(&mut self) {
        let from = match self.items.iter().position(|i| i.status == Status::InProgress) {
            Some(i) => {
                self.items[i].status = Status::Completed;
                i + 1
            }
            None => 0,
        };
        let next = self.items[from..]
            .iter()
            .position(|i| i.status == Status::Pending)
            .map(|p| p + from)
            .or_else(|| self.items.iter().position(|i| i.status == Status::Pending));
        if let Some(next) = next {
            self.items[next].status = Status::InProgress;
        }
    }

    /// Last changed before `prompt_at`, the person's latest prompt: Desktop
    /// shows the list under an older message, not under the current turn.
    fn predates(&self, prompt_at: Option<i64>) -> bool {
        matches!((self.updated_at, prompt_at), (Some(updated), Some(prompt)) if updated < prompt)
    }

    /// The list as Desktop shows it: statuses as they are, wrap-up or not.
    fn progress(&self) -> PlanProgress {
        let done = self
            .items
            .iter()
            .filter(|i| i.status == Status::Completed)
            .count();
        PlanProgress {
            items: self
                .items
                .iter()
                .map(|i| PlanItem {
                    content: i.content.clone(),
                    status: i.status.as_str().to_owned(),
                })
                .collect(),
            done: u32::try_from(done).unwrap_or(u32::MAX),
            total: u32::try_from(self.items.len()).unwrap_or(u32::MAX),
            current: self
                .items
                .iter()
                .find(|i| i.status == Status::InProgress)
                .map(|i| i.content.clone()),
            updated_at: self.updated_at,
            wrapped_up: self.wrapped_up,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Item {
    /// Number given by `TaskCreate`; None for the other sources.
    task_id: Option<String>,
    content: String,
    status: Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    InProgress,
    Completed,
}

impl Status {
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "completed" | "complete" | "done" => Self::Completed,
            "in_progress" | "in-progress" | "inprogress" | "active" | "running" => {
                Self::InProgress
            }
            _ => Self::Pending,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// `{content, status}` (or a bare string) to an item.
fn parse_plan_item(value: &Value) -> Option<Item> {
    let (content, status) = match value {
        Value::String(s) => (s.as_str(), Status::Pending),
        Value::Object(_) => (
            str_field(value, "content")
                .or_else(|| str_field(value, "subject"))
                .or_else(|| str_field(value, "title"))
                .unwrap_or(""),
            str_field(value, "status").map_or(Status::Pending, Status::parse),
        ),
        _ => return None,
    };
    Some(Item {
        task_id: None,
        content: content.trim().to_owned(),
        status,
    })
}

/// An `update_plan` item as Desktop reads it: text required, status exactly
/// "completed" or "in_progress", anything else pending.
fn parse_update_plan_item(value: &Value) -> Option<Item> {
    let (content, status) = match value {
        Value::String(s) => (s.as_str(), Status::Pending),
        Value::Object(_) => (
            str_field(value, "content").unwrap_or(""),
            match str_field(value, "status") {
                Some("completed") => Status::Completed,
                Some("in_progress") => Status::InProgress,
                _ => Status::Pending,
            },
        ),
        _ => return None,
    };
    let content = content.trim();
    (!content.is_empty()).then(|| Item {
        task_id: None,
        content: content.to_owned(),
        status,
    })
}

/// 1-based item numbers (a number, a numeric string or a list of them) to
/// 0-based indices.
fn one_based_indices(value: &Value) -> Vec<usize> {
    match value {
        Value::Array(values) => values.iter().filter_map(one_based_index).collect(),
        other => one_based_index(other).into_iter().collect(),
    }
}

fn one_based_index(value: &Value) -> Option<usize> {
    let n = match value {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && *f >= 0.0).map(|f| f as u64))?,
        Value::String(s) => s.trim().parse::<u64>().ok()?,
        _ => return None,
    };
    usize::try_from(n).ok()?.checked_sub(1)
}

fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().trim_start_matches('#').to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => s == "true",
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tool summaries
// ---------------------------------------------------------------------------

/// Short display name of a tool: `mcp__ccd_turn__update_plan` -> `update_plan`.
pub fn tool_display_name(name: &str) -> &str {
    name.strip_prefix("mcp__")
        .and_then(|rest| rest.find("__").map(|i| &rest[i + 2..]))
        .filter(|short| !short.is_empty())
        .unwrap_or(name)
}

/// One-line summary of a tool call: the command for Bash, the file name for
/// Read/Edit/Write, the pattern for Grep/Glob… At most 120 characters.
pub fn summarize_tool(name: &str, input: &serde_json::Value) -> String {
    let raw = match tool_display_name(name) {
        // Prose written in Markdown, unlike commands and patterns.
        "wrap_up" => return plain_text(&first_str(input, &["recap", "ask"]), SUMMARY_MAX),
        "Bash" | "PowerShell" => shell_summary(input),
        "Monitor" => first_str(input, &["description", "command"]),
        "Read" | "Edit" | "Write" | "MultiEdit" => {
            str_field(input, "file_path").map(file_name).unwrap_or_default().to_owned()
        }
        "NotebookEdit" | "NotebookRead" => {
            file_name(&first_str(input, &["notebook_path", "file_path"])).to_owned()
        }
        "Grep" | "Glob" => first_str(input, &["pattern"]),
        "WebFetch" => str_field(input, "url").map(url_host).unwrap_or_default().to_owned(),
        "WebSearch" | "ToolSearch" => first_str(input, &["query"]),
        "Agent" | "Task" => first_str(input, &["description", "prompt"]),
        "Workflow" => workflow_summary(input),
        "update_plan" => update_plan_summary(input),
        "TodoWrite" => todo_summary(input),
        "TaskCreate" => first_str(input, &["subject", "description"]),
        "TaskUpdate" => task_update_summary(input),
        "AskUserQuestion" => ask_summary(input),
        "SendMessage" => send_message_summary(input),
        "Skill" => first_str(input, &["skill"]),
        _ => generic_summary(input),
    };
    one_line(&raw, SUMMARY_MAX)
}

/// The command when it is one short line, else its description.
fn shell_summary(input: &Value) -> String {
    let command = str_field(input, "command").unwrap_or("");
    let description = str_field(input, "description")
        .map(str::trim)
        .filter(|d| !d.is_empty());
    let mut lines = command.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("");
    let multi_line = lines.next().is_some();
    let long = first.chars().count() > 80;
    match description {
        Some(description) if multi_line || long || first.is_empty() => description.to_owned(),
        _ if multi_line => format!("{first} …"),
        _ => first.to_owned(),
    }
}

/// `meta.name` of an inline script, else its description or script file.
fn workflow_summary(input: &Value) -> String {
    if let Some(name) = str_field(input, "script").and_then(script_meta_name) {
        return name.to_owned();
    }
    if let Some(description) = str_field(input, "description").filter(|d| !d.trim().is_empty()) {
        return description.to_owned();
    }
    if let Some(path) = str_field(input, "scriptPath") {
        let stem = file_name(path);
        let stem = stem.split('.').next().unwrap_or(stem);
        if !stem.is_empty() {
            return stem.to_owned();
        }
    }
    "workflow".to_owned()
}

/// Finds `name: '…'` in the `meta` object of a workflow script.
fn script_meta_name(script: &str) -> Option<&str> {
    let mut rest = &script[script.find("meta")? + 4..];
    loop {
        let at = rest.find("name")?;
        let preceded_by_word = rest[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let after = &rest[at + 4..];
        rest = after;
        if preceded_by_word {
            continue;
        }
        let after = after.strip_prefix(['"', '\'']).unwrap_or(after).trim_start();
        let Some(value) = after.strip_prefix(':') else {
            continue;
        };
        let value = value.trim_start();
        let quote = value.chars().next()?;
        if !matches!(quote, '\'' | '"' | '`') {
            continue;
        }
        let body = &value[1..];
        let name = &body[..body.find(quote)?];
        if !name.trim().is_empty() {
            return Some(name);
        }
    }
}

fn update_plan_summary(input: &Value) -> String {
    if let Some(items) = input.get("items").and_then(Value::as_array) {
        let parsed: Vec<Item> = items.iter().filter_map(parse_plan_item).collect();
        return parsed
            .iter()
            .find(|i| i.status == Status::InProgress)
            .or_else(|| parsed.iter().find(|i| i.status == Status::Pending))
            .map(|i| i.content.clone())
            .unwrap_or_else(|| format!("{} étapes", parsed.len()));
    }
    let mut parts = Vec::new();
    if let Some(done) = input.get("done") {
        let numbers: Vec<String> = one_based_indices(done)
            .iter()
            .map(|i| (i + 1).to_string())
            .collect();
        if !numbers.is_empty() {
            parts.push(format!("done {}", numbers.join(", ")));
        }
    }
    if input.get("advance").is_some_and(is_truthy) {
        parts.push("advance".to_owned());
    }
    if let Some(start) = input.get("start").and_then(one_based_index) {
        parts.push(format!("start {}", start + 1));
    }
    parts.join(" · ")
}

fn todo_summary(input: &Value) -> String {
    let Some(todos) = input.get("todos").and_then(Value::as_array) else {
        return String::new();
    };
    let items: Vec<Item> = todos.iter().filter_map(parse_plan_item).collect();
    items
        .iter()
        .find(|i| i.status == Status::InProgress)
        .or_else(|| items.iter().find(|i| i.status == Status::Pending))
        .map(|i| i.content.clone())
        .unwrap_or_default()
}

fn task_update_summary(input: &Value) -> String {
    let id = input.get("taskId").and_then(id_string).unwrap_or_default();
    match str_field(input, "status") {
        Some(status) => format!("#{id} {status}"),
        None => format!("#{id}"),
    }
}

fn ask_summary(input: &Value) -> String {
    input
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|q| q.first())
        .map(|q| first_str(q, &["question", "header"]))
        .unwrap_or_else(|| first_str(input, &["question"]))
}

fn send_message_summary(input: &Value) -> String {
    let to = first_str(input, &["to", "recipient"]);
    let what = first_str(input, &["summary", "message", "content"]);
    match (to.is_empty(), what.is_empty()) {
        (false, false) => format!("{to} : {what}"),
        (false, true) => to,
        _ => what,
    }
}

/// Keys that best describe an unknown tool call, in order of preference.
const PREFERRED_KEYS: &[&str] = &[
    "description",
    "action_summary",
    "title",
    "subject",
    "summary",
    "command",
    "query",
    "pattern",
    "url",
    "file_path",
    "path",
    "name",
    "skill",
    "action",
    "message",
    "prompt",
    "text",
];

/// A preferred string field, else the first short string field.
fn generic_summary(input: &Value) -> String {
    if let Value::String(s) = input {
        return s.clone();
    }
    let preferred = first_str(input, PREFERRED_KEYS);
    if !preferred.is_empty() {
        return preferred;
    }
    input
        .as_object()
        .and_then(|fields| {
            fields.values().filter_map(Value::as_str).find(|s| {
                let s = s.trim();
                !s.is_empty() && s.chars().count() <= SUMMARY_MAX
            })
        })
        .unwrap_or_default()
        .to_owned()
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The first of `keys` holding a non-blank string, or "".
fn first_str(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|k| str_field(value, k))
        .find(|s| !s.trim().is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// Last component of a Windows or POSIX path.
fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

/// Host of a URL, without scheme, credentials, port or path.
fn url_host(url: &str) -> &str {
    let url = url.trim();
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    if host.starts_with('[') {
        host.split_once(']').map_or(host, |(h, _)| &h[1..])
    } else {
        host.split(':').next().unwrap_or(host)
    }
}

/// Markdown prose to readable plain text on one line, cut to `max`
/// characters: links keep their label, inline code its content, emphasis
/// markers go, and so do headings, bullets, quotes, rules and fences.
pub fn plain_text(text: &str, max: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut in_fence = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            parts.push(line.to_owned());
            continue;
        }
        if is_rule(line) {
            continue;
        }
        parts.push(strip_inline(strip_block_markers(line)));
    }
    one_line(&parts.join(" ").replace('⏎', " "), max)
}

/// `---`, `***`, `___` and table separators such as `|---|:--:|`.
fn is_rule(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() >= 3 {
        let first = compact.chars().next().unwrap_or(' ');
        if matches!(first, '-' | '*' | '_') && compact.chars().all(|c| c == first) {
            return true;
        }
    }
    compact.contains('-') && compact.chars().all(|c| matches!(c, '|' | '-' | ':'))
}

/// Removes quote markers, a heading's hashes, a list bullet or number and a
/// task box at the start of a line.
fn strip_block_markers(mut line: &str) -> &str {
    loop {
        let before = line;
        line = line.trim_start();
        if let Some(rest) = line.strip_prefix('>') {
            line = rest;
        } else if line.starts_with('#') {
            let rest = line.trim_start_matches('#');
            let level = line.len() - rest.len();
            if level <= 6 && (rest.is_empty() || rest.starts_with(char::is_whitespace)) {
                line = rest;
            }
        } else if let Some(rest) = line
            .strip_prefix(['-', '*', '+', '•'])
            .filter(|rest| rest.starts_with(char::is_whitespace))
        {
            line = rest;
        } else if let Some(rest) = numbered_item(line) {
            line = rest;
        } else if let Some(rest) = ["[ ]", "[x]", "[X]"]
            .iter()
            .find_map(|box_| line.strip_prefix(box_))
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            line = rest;
        }
        if line == before {
            return line;
        }
    }
}

/// The rest of `12. item` or `3) item`.
fn numbered_item(line: &str) -> Option<&str> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    line[digits..]
        .strip_prefix(['.', ')'])
        .filter(|rest| rest.starts_with(char::is_whitespace))
}

/// Links to their label, inline code to its content, emphasis markers out.
fn strip_inline(line: &str) -> String {
    let linked = strip_links(line);
    let mut out = String::with_capacity(linked.len());
    // Odd segments are inside backticks: kept as they are.
    for (n, segment) in linked.split('`').enumerate() {
        if n % 2 == 1 {
            out.push_str(segment);
        } else {
            strip_emphasis(segment, &mut out);
        }
    }
    out
}

/// `[label](target)` and `![alt](source)` to their text.
fn strip_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let label_start = open + 1;
        let Some(close) = rest[label_start..].find(']').map(|i| label_start + i) else {
            break;
        };
        let after = &rest[close + 1..];
        let target_end = after
            .strip_prefix('(')
            .and_then(|target| target.find(')'))
            .map(|i| close + 1 + 1 + i);
        match target_end {
            Some(end) => {
                let image = rest[..open].ends_with('!');
                out.push_str(&rest[..if image { open - 1 } else { open }]);
                out.push_str(&rest[label_start..close]);
                rest = &rest[end + 1..];
            }
            None => {
                out.push_str(&rest[..label_start]);
                rest = &rest[label_start..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Drops `**`, `__`, `~~` and single `*` / `_` used as emphasis, keeping
/// `snake_case`, `2*3` and a lone ` * `.
fn strip_emphasis(text: &str, out: &mut String) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if matches!(c, '*' | '_' | '~') && next == Some(c) {
            i += 2;
            continue;
        }
        if matches!(c, '*' | '_') {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let word = |ch: Option<char>| ch.is_some_and(char::is_alphanumeric);
            let space = |ch: Option<char>| ch.is_none_or(char::is_whitespace);
            let literal = (word(prev) && word(next)) || (space(prev) && space(next));
            if !literal {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
}

/// Collapses whitespace to single spaces and cuts to `max` characters,
/// ending with "…" when cut.
fn one_line(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    'words: for word in text.split_whitespace() {
        if count > 0 {
            out.push(' ');
            count += 1;
        }
        for c in word.chars().filter(|c| !c.is_control()) {
            out.push(c);
            count += 1;
            if count > max {
                break 'words;
            }
        }
    }
    if count > max {
        let cut = out
            .char_indices()
            .nth(max.saturating_sub(1))
            .map_or(out.len(), |(i, _)| i);
        out.truncate(cut);
        out.truncate(out.trim_end().len());
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// Parses an ISO 8601 date-time such as `2026-10-22T13:57:34.548Z` to epoch
/// milliseconds. Accepts any number of fractional digits, `Z`, `±hh:mm`,
/// `±hhmm` or `±hh` offsets, and treats a missing offset as UTC.
pub fn parse_iso8601_ms(text: &str) -> Option<i64> {
    let b = text.trim().as_bytes();
    if b.len() < 19 {
        return None;
    }
    let year = digits(&b[0..4])?;
    let month = digits(&b[5..7])?;
    let day = digits(&b[8..10])?;
    let hour = digits(&b[11..13])?;
    let minute = digits(&b[14..16])?;
    let second = digits(&b[17..19])?;
    if b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut i = 19;
    let mut millis = 0i64;
    if i < b.len() && matches!(b[i], b'.' | b',') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            if i - start < 3 {
                millis = millis * 10 + i64::from(b[i] - b'0');
            }
            i += 1;
        }
        let read = i - start;
        if read == 0 {
            return None;
        }
        for _ in read..3 {
            millis *= 10;
        }
    }
    let offset_minutes = match b.get(i) {
        None => 0,
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let rest = &b[i + 1..];
            let (h, m) = match rest.len() {
                2 => (digits(rest)?, 0),
                4 => (digits(&rest[0..2])?, digits(&rest[2..4])?),
                5 if rest[2] == b':' => (digits(&rest[0..2])?, digits(&rest[3..5])?),
                _ => return None,
            };
            if h > 23 || m > 59 {
                return None;
            }
            let minutes = h * 60 + m;
            if sign == b'-' { -minutes } else { minutes }
        }
        Some(_) => return None,
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1_000 + millis)
}

fn digits(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        bytes
            .iter()
            .fold(0i64, |acc, b| acc * 10 + i64::from(b - b'0')),
    )
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ---------------------------------------------------------------------------
// Tolerant line decoding
// ---------------------------------------------------------------------------
//
// Hand-written visitors read only the handful of fields the tracker needs and
// skip everything else (tool results, attachments, thinking blocks) without
// allocating. A field of an unexpected type is ignored instead of failing the
// whole line.

/// The fields of a transcript line the tracker reads.
#[derive(Debug, Default)]
struct Line {
    kind: Option<String>,
    timestamp: Option<String>,
    is_sidechain: bool,
    git_branch: Option<String>,
    content: Content,
    /// `message.id`: the API message an assistant line belongs to.
    message_id: Option<String>,
    /// `origin.kind`: "human", "task-notification", "peer"…
    origin_kind: Option<String>,
    is_meta: bool,
    is_compact_summary: bool,
    is_visible_only: bool,
    /// `subtype` of a system line ("stop_hook_summary"…).
    subtype: Option<String>,
    prevented_continuation: bool,
    is_api_error: bool,
    /// `error` of an API error line ("rate_limit"…).
    error: Option<String>,
}

/// `message.content`: a plain string or a list of blocks.
#[derive(Debug, Default)]
enum Content {
    #[default]
    None,
    Text(String),
    Blocks(Vec<Block>),
}

/// One content block: text, tool_use or tool_result (others are ignored).
#[derive(Debug, Default)]
struct Block {
    kind: Option<String>,
    text: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<Value>,
    tool_use_id: Option<String>,
    is_error: bool,
}

/// Keys the tracker reads, at any nesting level.
enum Field {
    Type,
    Timestamp,
    IsSidechain,
    GitBranch,
    Message,
    Content,
    Text,
    Id,
    Name,
    Input,
    ToolUseId,
    IsError,
    Origin,
    IsMeta,
    IsCompactSummary,
    IsVisibleInTranscriptOnly,
    Subtype,
    PreventedContinuation,
    IsApiErrorMessage,
    Error,
    Other,
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldVisitor;
        impl Visitor<'_> for FieldVisitor {
            type Value = Field;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a field name")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Field, E> {
                Ok(match v {
                    "type" => Field::Type,
                    "timestamp" => Field::Timestamp,
                    "isSidechain" => Field::IsSidechain,
                    "gitBranch" => Field::GitBranch,
                    "message" => Field::Message,
                    "content" => Field::Content,
                    "text" => Field::Text,
                    "id" => Field::Id,
                    "name" => Field::Name,
                    "input" => Field::Input,
                    "tool_use_id" => Field::ToolUseId,
                    "is_error" => Field::IsError,
                    "origin" => Field::Origin,
                    "isMeta" => Field::IsMeta,
                    "isCompactSummary" => Field::IsCompactSummary,
                    "isVisibleInTranscriptOnly" => Field::IsVisibleInTranscriptOnly,
                    "subtype" => Field::Subtype,
                    "preventedContinuation" => Field::PreventedContinuation,
                    "isApiErrorMessage" => Field::IsApiErrorMessage,
                    "error" => Field::Error,
                    _ => Field::Other,
                })
            }
        }
        deserializer.deserialize_identifier(FieldVisitor)
    }
}

/// Visitor methods that accept and discard the kinds of value a visitor does
/// not care about, returning the default value.
macro_rules! discard {
    ($de:lifetime: $($kind:ident)*) => { $(discard!(@ $de $kind);)* };
    (@ $de:lifetime scalars) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
    };
    (@ $de:lifetime str) => {
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
    };
    (@ $de:lifetime seq) => {
        fn visit_seq<A: SeqAccess<$de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(Default::default())
        }
    };
    (@ $de:lifetime map) => {
        fn visit_map<A: MapAccess<$de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(Default::default())
        }
    };
}

fn into_string(value: Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s),
        _ => None,
    }
}

impl<'de> Deserialize<'de> for Line {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct LineVisitor;
        impl<'de> Visitor<'de> for LineVisitor {
            type Value = Line;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a transcript line object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Line, A::Error> {
                let mut line = Line::default();
                while let Some(key) = map.next_key::<Field>()? {
                    match key {
                        Field::Type => line.kind = into_string(map.next_value()?),
                        Field::Timestamp => line.timestamp = into_string(map.next_value()?),
                        Field::IsSidechain => {
                            line.is_sidechain = map.next_value::<Value>()?.as_bool() == Some(true);
                        }
                        Field::GitBranch => line.git_branch = into_string(map.next_value()?),
                        Field::Message => {
                            let message = map.next_value::<MessageContent>()?;
                            line.content = message.content;
                            line.message_id = message.id;
                        }
                        Field::Origin => {
                            line.origin_kind = map
                                .next_value::<Value>()?
                                .get("kind")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                        }
                        Field::IsMeta => line.is_meta = is_true(map.next_value()?),
                        Field::IsCompactSummary => {
                            line.is_compact_summary = is_true(map.next_value()?);
                        }
                        Field::IsVisibleInTranscriptOnly => {
                            line.is_visible_only = is_true(map.next_value()?);
                        }
                        Field::Subtype => line.subtype = into_string(map.next_value()?),
                        Field::PreventedContinuation => {
                            line.prevented_continuation = is_true(map.next_value()?);
                        }
                        Field::IsApiErrorMessage => line.is_api_error = is_true(map.next_value()?),
                        Field::Error => line.error = into_string(map.next_value()?),
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(line)
            }
        }
        deserializer.deserialize_map(LineVisitor)
    }
}

fn is_true(value: Value) -> bool {
    value.as_bool() == Some(true)
}

/// The `content` and `id` of a `message` object.
#[derive(Default)]
struct MessageContent {
    content: Content,
    id: Option<String>,
}

impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MessageVisitor;
        impl<'de> Visitor<'de> for MessageVisitor {
            type Value = MessageContent;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a message object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<MessageContent, A::Error> {
                let mut message = MessageContent::default();
                while let Some(key) = map.next_key::<Field>()? {
                    match key {
                        Field::Content => message.content = map.next_value()?,
                        Field::Id => message.id = into_string(map.next_value()?),
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(message)
            }
            discard!('de: scalars str seq);
        }
        deserializer.deserialize_any(MessageVisitor)
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ContentVisitor;
        impl<'de> Visitor<'de> for ContentVisitor {
            type Value = Content;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string or a list of blocks")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Content, E> {
                Ok(Content::Text(v.to_owned()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Content, E> {
                Ok(Content::Text(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Content, A::Error> {
                let mut blocks = Vec::new();
                while let Some(MaybeBlock(block)) = seq.next_element()? {
                    blocks.extend(block);
                }
                Ok(Content::Blocks(blocks))
            }
            discard!('de: scalars map);
        }
        deserializer.deserialize_any(ContentVisitor)
    }
}

/// A list element that is a block object, or anything else (dropped).
#[derive(Default)]
struct MaybeBlock(Option<Block>);

impl<'de> Deserialize<'de> for MaybeBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BlockVisitor;
        impl<'de> Visitor<'de> for BlockVisitor {
            type Value = MaybeBlock;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a content block")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<MaybeBlock, A::Error> {
                let mut block = Block::default();
                while let Some(key) = map.next_key::<Field>()? {
                    match key {
                        Field::Type => block.kind = into_string(map.next_value()?),
                        Field::Text => block.text = into_string(map.next_value()?),
                        Field::Id => block.id = into_string(map.next_value()?),
                        Field::Name => block.name = into_string(map.next_value()?),
                        Field::Input => block.input = Some(map.next_value()?),
                        Field::ToolUseId => block.tool_use_id = into_string(map.next_value()?),
                        Field::IsError => {
                            block.is_error = map.next_value::<Value>()?.as_bool() == Some(true);
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(MaybeBlock(Some(block)))
            }
            discard!('de: scalars str seq);
        }
        deserializer.deserialize_any(BlockVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/transcript");
    const SESSION: &str = "0f0e0d0c-1111-4222-8333-444455556666";
    const UPDATE_PLAN: &str = "mcp__ccd_turn__update_plan";
    const WRAP_UP: &str = "mcp__ccd_turn__wrap_up";

    /// `2026-10-22T10:00:<seconds>Z`.
    fn at(seconds: u32) -> String {
        format!("2026-10-22T10:00:{seconds:02}Z")
    }

    fn fixture(name: &str) -> String {
        fs::read_to_string(Path::new(FIXTURES).join(name)).expect("fixture")
    }

    fn state_from(text: &str) -> TranscriptState {
        let mut state = TranscriptState::default();
        for line in text.lines() {
            state.apply_line(line.as_bytes());
        }
        state
    }

    fn info_of(text: &str) -> TranscriptInfo {
        state_from(text).info(Path::new("x.jsonl"))
    }

    fn statuses(plan: &PlanProgress) -> Vec<&str> {
        plan.items.iter().map(|i| i.status.as_str()).collect()
    }

    fn assistant_tool(id: &str, name: &str, input: Value, ts: &str) -> String {
        json!({
            "parentUuid": "p", "isSidechain": false,
            "message": {"role": "assistant", "type": "message",
                "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]},
            "type": "assistant", "uuid": format!("a-{id}"), "timestamp": ts,
            "cwd": "D:\\Projets\\démo", "sessionId": SESSION, "gitBranch": "main"
        })
        .to_string()
    }

    fn tool_result(id: &str, is_error: bool, ts: &str) -> String {
        json!({
            "parentUuid": "p", "isSidechain": false, "type": "user",
            "message": {"role": "user", "content": [
                {"tool_use_id": id, "type": "tool_result", "is_error": is_error,
                 "content": [{"type": "text", "text": "ok"}]}]},
            "uuid": format!("u-{id}"), "timestamp": ts, "sessionId": SESSION, "gitBranch": "main"
        })
        .to_string()
    }

    /// A main-chain user prompt, with `origin.kind` when given.
    fn prompt(text: &str, origin: Option<&str>, ts: &str) -> String {
        let mut line = json!({
            "parentUuid": "p", "isSidechain": false, "type": "user",
            "message": {"role": "user", "content": text},
            "uuid": format!("u-{ts}"), "timestamp": ts, "sessionId": SESSION
        });
        if let Some(kind) = origin {
            line["origin"] = json!({"kind": kind});
        }
        line.to_string()
    }

    /// Desktop's end-of-turn marker.
    fn stop(ts: &str) -> String {
        json!({
            "parentUuid": "p", "isSidechain": false, "type": "system",
            "subtype": "stop_hook_summary", "hookCount": 2, "preventedContinuation": false,
            "timestamp": ts, "uuid": format!("s-{ts}"), "sessionId": SESSION
        })
        .to_string()
    }

    fn assistant_text(text: &str, ts: &str) -> String {
        json!({
            "parentUuid": "p", "isSidechain": false, "type": "assistant",
            "message": {"role": "assistant", "id": format!("msg-{ts}"),
                "content": [{"type": "text", "text": text}]},
            "uuid": format!("t-{ts}"), "timestamp": ts, "sessionId": SESSION
        })
        .to_string()
    }

    fn api_error(kind: &str, text: &str, ts: &str) -> String {
        json!({
            "parentUuid": "p", "isSidechain": false, "type": "assistant",
            "message": {"id": "synthetic", "model": "<synthetic>", "role": "assistant",
                "content": [{"type": "text", "text": text}]},
            "error": kind, "isApiErrorMessage": true,
            "uuid": format!("e-{ts}"), "timestamp": ts, "sessionId": SESSION
        })
        .to_string()
    }

    fn turn_of(lines: &[String]) -> TurnInfo {
        info_of(&lines.join("\n")).turn.expect("turn")
    }

    /// A unique, empty `projects` folder holding one project directory.
    struct TempProjects {
        root: PathBuf,
    }

    impl TempProjects {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "monitor-core-transcript-{tag}-{}-{}",
                std::process::id(),
                crate::paths::now_ms()
            ));
            fs::create_dir_all(root.join("D--Projets-d-mo")).expect("temp dir");
            Self { root }
        }

        fn transcript(&self) -> PathBuf {
            self.root.join("D--Projets-d-mo").join(format!("{SESSION}.jsonl"))
        }

        fn append(&self, text: &str) {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.transcript())
                .expect("open fixture");
            file.write_all(text.as_bytes()).expect("write fixture");
        }
    }

    impl Drop for TempProjects {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn iso8601_parsing() {
        assert_eq!(parse_iso8601_ms("2026-10-22T13:57:34.548Z"), Some(1_792_677_454_548));
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00.5Z"), Some(500));
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00.123456789Z"), Some(123));
        assert_eq!(parse_iso8601_ms("1969-12-31T23:59:59.999Z"), Some(-1));
        assert_eq!(parse_iso8601_ms("2024-02-29T23:59:59Z"), Some(1_709_251_199_000));
        assert_eq!(parse_iso8601_ms("2000-03-01T00:00:00+00:00"), Some(951_868_800_000));
        assert_eq!(parse_iso8601_ms("2000-03-01T02:00:00+02:00"), Some(951_868_800_000));
        assert_eq!(parse_iso8601_ms("2000-02-29T22:30:00-0130"), Some(951_868_800_000));
        assert_eq!(parse_iso8601_ms("2000-03-01T00:00:00"), Some(951_868_800_000));
        assert_eq!(parse_iso8601_ms(" 2026-10-22T13:57:34.548Z "), Some(1_792_677_454_548));
        for bad in [
            "",
            "2026-10-22",
            "2026-13-01T00:00:00Z",
            "2023-02-29T00:00:00Z",
            "2026-10-22T24:00:00Z",
            "2026-10-22T13:57:34.Z",
            "2026-10-22T13:57:34.548Zjunk",
            "2026-10-22T13:57:34+5",
            "2026/10/22T13:57:34Z",
            "2026-10-22T13:57:34.548Ü",
            "２０２６-10-22T13:57:34Z",
        ] {
            assert_eq!(parse_iso8601_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn display_names() {
        assert_eq!(tool_display_name("mcp__ccd_turn__update_plan"), "update_plan");
        assert_eq!(tool_display_name("mcp__Claude_Browser__computer"), "computer");
        assert_eq!(tool_display_name("Bash"), "Bash");
        assert_eq!(tool_display_name("mcp__broken"), "mcp__broken");
    }

    #[test]
    fn summaries() {
        let meta_script = "export const meta = {\n  name: 'build-mvp',\n  description: 'x'\n}";
        let cases = [
            ("Bash", json!({"command": "cargo test", "description": "Tests"}), "cargo test"),
            (
                "Bash",
                json!({"command": "cd x && python - <<'EOF'\nprint(1)\nEOF", "description": "N"}),
                "N",
            ),
            ("PowerShell", json!({"command": "Get-ChildItem\nDir"}), "Get-ChildItem …"),
            ("Read", json!({"file_path": "D:\\Données Utilisateur\\src\\main.rs"}), "main.rs"),
            ("Edit", json!({"file_path": "/home/a/b/lib.rs"}), "lib.rs"),
            ("NotebookEdit", json!({"notebook_path": "C:\\n\\a.ipynb"}), "a.ipynb"),
            ("Grep", json!({"pattern": "fn\\s+main", "path": "src"}), "fn\\s+main"),
            ("Glob", json!({"pattern": "**/*.rs"}), "**/*.rs"),
            ("WebFetch", json!({"url": "https://user@docs.rs:443/serde?x=1"}), "docs.rs"),
            ("WebSearch", json!({"query": "tauri 2 tray"}), "tauri 2 tray"),
            ("Agent", json!({"description": "Audit", "prompt": "long"}), "Audit"),
            ("Workflow", json!({ "script": meta_script }), "build-mvp"),
            ("Workflow", json!({"script": "meta = { rename: 1, \"name\": \"quoted\" }"}), "quoted"),
            ("Workflow", json!({"scriptPath": "C:\\w\\fix-ci.workflow.js"}), "fix-ci"),
            ("Workflow", json!({"script": "run()"}), "workflow"),
            (
                UPDATE_PLAN,
                json!({"items": [
                    {"content": "A", "status": "completed"},
                    {"content": "B", "status": "in_progress"}]}),
                "B",
            ),
            (UPDATE_PLAN, json!({"advance": true}), "advance"),
            (UPDATE_PLAN, json!({"done": [2, 3], "start": 4}), "done 2, 3 · start 4"),
            (WRAP_UP, json!({"recap": "Fini.\nTout marche.", "ask": "?"}), "Fini. Tout marche."),
            (
                "AskUserQuestion",
                json!({"questions": [{"question": "On continue ?", "header": "Suite"}]}),
                "On continue ?",
            ),
            ("SendMessage", json!({"to": "banc", "summary": "Relis"}), "banc : Relis"),
            ("TaskUpdate", json!({"taskId": "2", "status": "completed"}), "#2 completed"),
            ("mcp__Claude_Browser__computer", json!({"action": "screenshot"}), "screenshot"),
            ("mcp__ccd_pr__get_status", json!({}), ""),
            ("Unknown", json!({"zeta": "court", "alpha": 3}), "court"),
            ("Unknown", json!(null), ""),
            ("Unknown", json!({"command": "  a\t\tb \n c "}), "a b c"),
        ];
        for (name, input, expected) in cases {
            assert_eq!(summarize_tool(name, &input), expected, "{name} {input}");
        }
        let summary = summarize_tool("Bash", &json!({ "command": "x".repeat(200) }));
        assert_eq!(summary.chars().count(), 120);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn one_line_cuts_on_characters() {
        assert_eq!(one_line("é".repeat(10).as_str(), 5), "éééé…");
        assert_eq!(one_line("abc", 3), "abc");
        assert_eq!(one_line("ab cd", 4), "ab…");
        assert_eq!(one_line("  \n ", 10), "");
    }

    #[test]
    fn plan_sequence_from_fixture() {
        let info = info_of(&fixture("plan_sequence.jsonl"));
        let plan = info.plan.expect("plan");
        assert_eq!(plan.total, 5);
        assert_eq!(
            statuses(&plan),
            ["completed", "completed", "completed", "in_progress", "in_progress"]
        );
        assert_eq!(plan.done, 3);
        assert_eq!(plan.current.as_deref(), Some("Écrire le parseur"));
        assert!(!plan.wrapped_up);
        assert_eq!(plan.updated_at, parse_iso8601_ms("2026-10-22T14:00:06.000Z"));
        assert_eq!(info.git_branch.as_deref(), Some("feature/plan"));
        assert_eq!(info.last_event_at, parse_iso8601_ms("2026-10-22T14:00:07.000Z"));
        assert_eq!(info.last_assistant_text.as_deref(), Some("Je commence par le parseur."));
        let last = info.last_tool.expect("last tool");
        assert_eq!(last.name, "update_plan");
        assert_eq!(info.current_tool, None);
    }

    #[test]
    fn wrap_up_then_new_items() {
        let text = fixture("wrap_up.jsonl");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 9);
        // Wrapped up: statuses stay as Desktop shows them.
        let info = info_of(&lines[..7].join("\n"));
        let wrapped = info.plan.expect("plan");
        assert!(wrapped.wrapped_up);
        assert_eq!(statuses(&wrapped), ["completed", "in_progress", "pending"]);
        assert_eq!((wrapped.done, wrapped.total), (1, 3));
        assert_eq!(wrapped.current.as_deref(), Some("Deux"));
        let ask = info.pending_ask.expect("question");
        assert_eq!(ask.text, "On continue ?");
        assert_eq!(ask.options, ["Oui"]);
        assert_eq!(ask.source, "wrap_up");
        assert_eq!(ask.at, parse_iso8601_ms("2026-10-22T14:00:05.000Z"));
        let turn = info.turn.expect("turn");
        assert!(turn.wrapped_up);
        assert_eq!(turn.wrap_up_recap.as_deref(), Some("Deux choses faites sur trois."));

        // The person's next prompt answers the question; the old list now
        // sits under an older message.
        let info = info_of(&lines[..8].join("\n"));
        assert_eq!(info.pending_ask, None);
        assert_eq!(info.plan, None);
        let turn = info.turn.expect("turn");
        assert!(!turn.wrapped_up && !turn.ended);

        let reopened = info_of(&text).plan.expect("plan");
        assert!(!reopened.wrapped_up);
        assert_eq!((reopened.done, reopened.total), (0, 2));
        assert_eq!(reopened.current.as_deref(), Some("Nouvelle étape"));
    }

    #[test]
    fn advance_after_wrap_up_continues_the_list() {
        let items = json!({"items": [
            {"content": "Un", "status": "in_progress"},
            {"content": "Deux", "status": "pending"},
            {"content": "Trois", "status": "pending"},
            {"content": "Quatre", "status": "pending"}], "scope": "first_task"});
        let lines = [
            assistant_tool("t1", UPDATE_PLAN, items, &at(0)),
            assistant_tool("t2", UPDATE_PLAN, json!({"advance": true}), &at(1)),
            assistant_tool("t3", WRAP_UP, json!({"recap": "Pause"}), &at(2)),
            assistant_tool("t4", UPDATE_PLAN, json!({"advance": true}), &at(3)),
        ];
        // Like Desktop, wrap_up changes no status.
        let wrapped = info_of(&lines[..3].join("\n")).plan.expect("plan");
        assert!(wrapped.wrapped_up);
        assert_eq!(statuses(&wrapped), ["completed", "in_progress", "pending", "pending"]);
        assert_eq!((wrapped.done, wrapped.current.as_deref()), (1, Some("Deux")));
        // The wrap-up is not a change: the list still dates from step 2.
        assert_eq!(wrapped.updated_at, parse_iso8601_ms(&at(1)));

        let resumed = info_of(&lines.join("\n")).plan.expect("plan");
        assert!(!resumed.wrapped_up);
        assert_eq!(statuses(&resumed), ["completed", "completed", "in_progress", "pending"]);
        assert_eq!((resumed.done, resumed.current.as_deref()), (2, Some("Trois")));
    }

    #[test]
    fn rejected_items_call_is_undone() {
        let first = json!({"items": [{"content": "A", "status": "in_progress"}], "scope": "x"});
        let without_scope = json!({"items": [
            {"content": "B", "status": "in_progress"},
            {"content": "C", "status": "pending"}]});
        let lines = [
            assistant_tool("t1", UPDATE_PLAN, first, &at(0)),
            tool_result("t1", false, &at(1)),
            assistant_tool("t2", UPDATE_PLAN, without_scope, &at(2)),
        ];
        let state = state_from(&lines.join("\n"));
        assert_eq!(state.info(Path::new("x")).plan.expect("plan").total, 2);

        let mut all = lines.to_vec();
        all.push(tool_result("t2", true, &at(3)));
        let plan = info_of(&all.join("\n")).plan.expect("plan");
        assert_eq!(plan.total, 1);
        assert_eq!(plan.current.as_deref(), Some("A"));
        assert_eq!(plan.updated_at, parse_iso8601_ms(&at(0)));
    }

    #[test]
    fn advance_edge_cases() {
        let mut plan = Plan {
            items: ["a", "b", "c"]
                .iter()
                .map(|c| Item {
                    task_id: None,
                    content: (*c).to_owned(),
                    status: Status::Pending,
                })
                .collect(),
            ..Plan::default()
        };
        plan.advance();
        assert_eq!(plan.items[0].status, Status::InProgress);
        plan.items[2].status = Status::Completed;
        plan.advance();
        plan.advance();
        assert!(plan.items.iter().all(|i| i.status == Status::Completed));
        plan.advance();
        assert!(plan.items.iter().all(|i| i.status == Status::Completed));
        Plan::default().advance();
    }

    #[test]
    fn plan_ops_without_plan_or_out_of_range_are_ignored() {
        let lines = [
            assistant_tool("t1", UPDATE_PLAN, json!({"advance": true}), &at(0)),
            assistant_tool("t2", WRAP_UP, json!({"recap": "x"}), &at(1)),
        ];
        assert_eq!(info_of(&lines.join("\n")).plan, None);
        let items = json!({"items": [{"content": "A", "status": "pending"}], "scope": "x"});
        let out_of_range = json!({"done": [0, 7, "x", -1], "start": 9});
        let lines = [
            assistant_tool("t1", UPDATE_PLAN, items, &at(0)),
            assistant_tool("t2", UPDATE_PLAN, out_of_range, &at(1)),
            assistant_tool("t3", UPDATE_PLAN, json!({"done": "1"}), &at(2)),
        ];
        let plan = info_of(&lines.join("\n")).plan.expect("plan");
        assert_eq!(statuses(&plan), ["completed"]);
        assert_eq!(plan.updated_at, parse_iso8601_ms(&at(2)));
    }

    #[test]
    fn todo_write_replaces_the_list() {
        let plan = info_of(&fixture("todo_write.jsonl")).plan.expect("plan");
        assert_eq!(plan.total, 2);
        assert_eq!(statuses(&plan), ["completed", "in_progress"]);
        assert_eq!(plan.current.as_deref(), Some("Lancer les tests"));
    }

    #[test]
    fn task_create_and_update() {
        let calls = [
            ("TaskCreate", json!({"subject": "Un", "description": "d"})),
            ("TaskCreate", json!({"subject": "Deux"})),
            ("TaskCreate", json!({"subject": "Trois"})),
            ("TaskUpdate", json!({"taskId": "1", "status": "completed"})),
            ("TaskUpdate", json!({"taskId": 2, "status": "in_progress"})),
            ("TaskUpdate", json!({"taskId": "3", "status": "deleted"})),
            ("TaskUpdate", json!({"taskId": "9", "status": "completed"})),
        ];
        let lines: Vec<String> = calls
            .into_iter()
            .enumerate()
            .map(|(n, (name, input))| assistant_tool(&format!("t{n}"), name, input, &at(n as u32)))
            .collect();
        let plan = info_of(&lines.join("\n")).plan.expect("plan");
        let contents: Vec<&str> = plan.items.iter().map(|i| i.content.as_str()).collect();
        assert_eq!(contents, ["Un", "Deux"]);
        assert_eq!(statuses(&plan), ["completed", "in_progress"]);
        assert_eq!(plan.updated_at, parse_iso8601_ms(&at(5)));
    }

    #[test]
    fn parallel_tool_calls() {
        let text = fixture("parallel_tools.jsonl");
        let lines: Vec<&str> = text.lines().collect();
        // Two reads and a grep started in parallel, the second read finished.
        let info = info_of(&lines[..5].join("\n"));
        let current = info.current_tool.expect("current tool");
        assert_eq!((current.name.as_str(), current.summary.as_str()), ("Grep", "TODO"));
        assert_eq!(info.last_tool.expect("last").name, "Grep");
        // The grep finished: the first read is still open.
        let current = info_of(&lines[..6].join("\n")).current_tool.expect("current tool");
        assert_eq!((current.name.as_str(), current.summary.as_str()), ("Read", "a.rs"));
        assert_eq!(current.at, parse_iso8601_ms("2026-10-22T11:00:01.100Z"));
        // A new prompt: the CLI answered every call before taking it.
        assert_eq!(info_of(&lines[..7].join("\n")).current_tool, None);
        // A late result changes nothing.
        let info = info_of(&text);
        assert_eq!(info.current_tool, None);
        assert_eq!(info.last_tool.expect("last").summary, "TODO");
    }

    #[test]
    fn a_new_api_message_closes_calls_left_without_result() {
        let call = |msg: &str, id: &str, name: &str, ts: &str| {
            let mut line: Value =
                serde_json::from_str(&assistant_tool(id, name, json!({"command": id}), ts))
                    .expect("json");
            line["message"]["id"] = json!(msg);
            line.to_string()
        };
        let lines = [
            call("msg_a", "t1", "Write", &at(0)),
            call("msg_a", "t2", "Edit", &at(1)),
            tool_result("t2", false, &at(2)),
        ];
        // Same message: the Write is still running.
        let info = info_of(&lines.join("\n"));
        assert_eq!(info.current_tool.map(|t| t.name).as_deref(), Some("Write"));
        // The next message: the Write never got its result, it is not running.
        let mut more = lines.to_vec();
        more.push(call("msg_b", "t3", "Bash", &at(30)));
        more.push(tool_result("t3", false, &at(31)));
        let info = info_of(&more.join("\n"));
        assert_eq!(info.current_tool, None);
        assert_eq!(info.last_tool.map(|t| t.name).as_deref(), Some("Bash"));
        // A stop marker closes everything too.
        let mut ended = lines.to_vec();
        ended.push(stop(&at(40)));
        assert_eq!(info_of(&ended.join("\n")).current_tool, None);
    }

    // --- turns -------------------------------------------------------------

    #[test]
    fn prompts_are_told_apart() {
        let kind = |line: String| {
            let line: Line = serde_json::from_str(&line).expect("line");
            classify_prompt(&line)
        };
        let human = Some(Prompt::Start(TurnOrigin::Human));
        let task = Some(Prompt::Start(TurnOrigin::Task));
        let peer = Some(Prompt::Start(TurnOrigin::Peer));
        let ts = at(0);
        let cases = [
            ("Pousse la branche", Some("human"), human),
            ("<task-notification>", Some("task-notification"), task),
            ("Another Claude session sent a message", Some("peer"), peer),
            // Without origin (older CLIs, Desktop reminders).
            ("Fais trois choses", None, human),
            ("Background (from the session that started this one): x", None, human),
            ("<system-reminder>\nThe session X you started finished", None, peer),
            (" <task-notification>\n<task-id>b1</task-id>", None, task),
            ("[Request interrupted by user]", None, Some(Prompt::Interrupt)),
            ("<command-name>/compact</command-name>", None, None),
            ("<local-command-stdout>Compacted</local-command-stdout>", None, None),
            ("", None, None),
            // The origin wins over the text: the person typed under a reminder.
            ("<system-reminder>x</system-reminder> ok", Some("human"), human),
            // Reminders set aside, what follows decides.
            ("<system-reminder>a</system-reminder>\n\n<system-reminder>b</system-reminder>", None, peer),
            ("<system-reminder>a</system-reminder>\n<side-sessions-event>x", None, peer),
            ("<system-reminder>a</system-reminder><cross-session-message from=\"l\">", None, peer),
            ("<system-reminder>a</system-reminder>\n<task-notification>", None, task),
            (
                "<system-reminder>You are operating in a git worktree</system-reminder>\n\n\
                 Background (from the session that started this one): fais #7",
                None,
                human,
            ),
            ("<system-reminder>Linked sessions</system-reminder>Task (from the session…", None, human),
            ("<side-sessions-event>None of them</side-sessions-event>", None, peer),
        ];
        for (text, origin, expected) in cases {
            assert_eq!(kind(prompt(text, origin, &ts)), expected, "{text:?} {origin:?}");
        }
        // Meta lines (skill bodies), compaction summaries: nothing starts.
        let flagged = |text: &str, flag: &str, origin: Option<&str>| {
            let mut line: Value = serde_json::from_str(&prompt(text, origin, &ts)).expect("json");
            line[flag] = json!(true);
            kind(line.to_string())
        };
        assert_eq!(flagged("Base directory for this skill", "isMeta", None), None);
        assert_eq!(flagged("<system-reminder>cwd changed", "isMeta", None), None);
        assert_eq!(flagged("texte", "isCompactSummary", None), None);
        assert_eq!(flagged("texte", "isVisibleInTranscriptOnly", None), None);
        // A peer message is meta but carries its origin.
        assert_eq!(flagged("texte", "isMeta", Some("peer")), peer);
        // Older CLIs: another session's idle notice wakes this one.
        let notice = "[Cross-session idle notice] \"uds:x\" is idle now";
        assert_eq!(flagged(notice, "isMeta", None), peer);
        // An image-only prompt.
        let image = json!({"type": "user", "isSidechain": false, "timestamp": ts,
            "message": {"content": [{"type": "image", "source": {}}]}});
        assert_eq!(kind(image.to_string()), human);
    }

    #[test]
    fn turn_start_and_end() {
        let mut lines = vec![prompt("Fais-le", Some("human"), &at(0))];
        let turn = turn_of(&lines);
        assert_eq!(
            (turn.origin, turn.started_at, turn.ended),
            (TurnOrigin::Human, parse_iso8601_ms(&at(0)), false)
        );
        lines.push(assistant_text("Fait.", &at(5)));
        lines.push(stop(&at(6)));
        let info = info_of(&lines.join("\n"));
        let turn = info.turn.expect("turn");
        assert!(turn.ended);
        assert_eq!(turn.ended_at, parse_iso8601_ms(&at(6)));
        assert_eq!(info.last_human_prompt_at, parse_iso8601_ms(&at(0)));
        assert_eq!(info.notable_turn_ended_at, parse_iso8601_ms(&at(6)));

        // A stop hook that prevented the stop does not end the turn.
        let mut blocked: Value = serde_json::from_str(&stop(&at(6))).expect("json");
        blocked["preventedContinuation"] = json!(true);
        let held = [lines[0].clone(), blocked.to_string()];
        assert!(!turn_of(&held).ended);

        // A child's ping: the person's turn stays the notable one.
        lines.push(prompt("<system-reminder>\nThe session X finished", None, &at(20)));
        lines.push(assistant_text("Rien à faire.", &at(21)));
        lines.push(stop(&at(22)));
        let info = info_of(&lines.join("\n"));
        let turn = info.turn.expect("turn");
        assert_eq!((turn.origin, turn.ended), (TurnOrigin::Peer, true));
        assert_eq!(info.notable_turn_ended_at, parse_iso8601_ms(&at(6)));
        assert_eq!(info.last_human_prompt_at, parse_iso8601_ms(&at(0)));

        // The session's own background task finishing is notable, even when
        // a peer message arrives in the same instant: one turn, the task's.
        lines.push(prompt("<task-notification>", Some("task-notification"), &at(30)));
        lines.push(prompt("[Subagent hand-back]", Some("peer"), &at(30)));
        let turn = turn_of(&lines);
        assert_eq!((turn.origin, turn.started_at), (TurnOrigin::Task, parse_iso8601_ms(&at(30))));
        lines.push(stop(&at(32)));
        assert_eq!(info_of(&lines.join("\n")).notable_turn_ended_at, parse_iso8601_ms(&at(32)));
        // Once answered, the next prompt starts a new turn.
        let mut answered = lines[..lines.len() - 1].to_vec();
        answered.push(assistant_text("Vu.", &at(31)));
        answered.push(prompt("<system-reminder>\nx", None, &at(31)));
        assert_eq!(turn_of(&answered).origin, TurnOrigin::Peer);

        // An interruption ends the turn.
        lines.push(prompt("Encore", Some("human"), &at(40)));
        lines.push(assistant_tool("t9", "Bash", json!({"command": "sleep 100"}), &at(41)));
        lines.push(prompt("[Request interrupted by user]", None, &at(42)));
        let info = info_of(&lines.join("\n"));
        let turn = info.turn.expect("turn");
        assert!(turn.ended);
        assert_eq!(turn.ended_at, parse_iso8601_ms(&at(42)));
        assert_eq!(info.current_tool, None);

        // Assistant activity after the end: the turn runs again.
        lines.push(assistant_text("Je reprends.", &at(50)));
        assert!(!turn_of(&lines).ended);
    }

    #[test]
    fn wrap_up_questions() {
        let wrap = |input: Value, ts: &str| assistant_tool(&format!("w{ts}"), WRAP_UP, input, ts);
        let start = prompt("Implémente #6", Some("human"), &at(0));
        let asking = json!({
            "recap": "#6 est fait sur `feat/6-lot`. Rien n'est poussé.",
            "ask": "Pousser **feat/6-lot** et ouvrir la PR ?",
            "actions": [{"label": "Pousser et ouvrir", "message": "Pousse."},
                        {"label": "Pas encore", "message": "Attends."}, {"x": 1}]});
        let lines = vec![start.clone(), wrap(asking.clone(), &at(5)), stop(&at(6))];
        let info = info_of(&lines.join("\n"));
        let ask = info.pending_ask.clone().expect("question");
        assert_eq!(ask.text, "Pousser feat/6-lot et ouvrir la PR ?");
        assert_eq!(ask.options, ["Pousser et ouvrir", "Pas encore"]);
        assert_eq!((ask.source.as_str(), ask.at), ("wrap_up", parse_iso8601_ms(&at(5))));
        assert_eq!(
            info.turn.and_then(|t| t.wrap_up_recap).as_deref(),
            Some("#6 est fait sur feat/6-lot. Rien n'est poussé.")
        );
        // The activity summary is plain text too.
        assert_eq!(
            info.last_tool.map(|t| t.summary).as_deref(),
            Some("#6 est fait sur feat/6-lot. Rien n'est poussé.")
        );

        // blocked without ask: the recap's first sentence.
        let blocked = [
            start.clone(),
            wrap(
                json!({"recap": "J'attends ta relecture. Le reste est prêt.", "blocked": true}),
                &at(5),
            ),
        ];
        let ask = info_of(&blocked.join("\n")).pending_ask.expect("question");
        assert_eq!(ask.text, "J'attends ta relecture.");
        assert!(ask.options.is_empty());
        let blocked = [start.clone(), wrap(json!({"recap": "Bloqué", "blocked": "true"}), &at(5))];
        assert!(info_of(&blocked.join("\n")).pending_ask.is_some());

        // No ask, not blocked: nothing waits; blank ask is no ask.
        let quiet = [start.clone(), wrap(json!({"recap": "Fini.", "ask": "  "}), &at(5))];
        let info = info_of(&quiet.join("\n"));
        assert_eq!(info.pending_ask, None);
        assert!(info.turn.expect("turn").wrapped_up);

        // A turn another session or a task started keeps the question…
        let mut kept = lines.clone();
        kept.push(prompt("<system-reminder>\nThe session X finished", None, &at(20)));
        kept.push(assistant_text("Rien à faire.", &at(21)));
        kept.push(stop(&at(22)));
        kept.push(prompt("<task-notification>", Some("task-notification"), &at(30)));
        kept.push(stop(&at(31)));
        assert_eq!(info_of(&kept.join("\n")).pending_ask, Some(ask_of(&asking, 5)));
        // …unless it wraps up without question…
        let mut cleared = kept.clone();
        cleared.push(prompt("<system-reminder>\nx", None, &at(40)));
        cleared.push(wrap(json!({"recap": "La PR est fusionnée."}), &at(41)));
        assert_eq!(info_of(&cleared.join("\n")).pending_ask, None);
        // …or asks something else.
        let mut replaced = kept.clone();
        replaced.push(prompt("<system-reminder>\nx", None, &at(40)));
        replaced.push(wrap(json!({"recap": "r", "ask": "On fusionne ?"}), &at(41)));
        let ask = info_of(&replaced.join("\n")).pending_ask.expect("question");
        assert_eq!(ask.text, "On fusionne ?");
        // The person's next prompt answers it.
        let mut answered = kept;
        answered.push(prompt("Pousse feat/6-lot et ouvre la PR de #6.", Some("human"), &at(50)));
        assert_eq!(info_of(&answered.join("\n")).pending_ask, None);

        // Desktop rejects the call (a subagent's, say): nothing changes.
        let rejected = [
            start,
            wrap(json!({"recap": "r", "ask": "On y va ?"}), &at(5)),
            tool_result(&format!("w{}", at(5)), true, &at(6)),
        ];
        let info = info_of(&rejected.join("\n"));
        assert_eq!(info.pending_ask, None);
        assert!(!info.turn.expect("turn").wrapped_up);
    }

    fn ask_of(input: &Value, second: u32) -> PendingQuestion {
        wrap_up_question(input, parse_iso8601_ms(&at(second))).expect("question")
    }

    #[test]
    fn sidechain_wrap_up_is_ignored() {
        let mut side: Value = serde_json::from_str(&assistant_tool(
            "w1",
            WRAP_UP,
            json!({"recap": "r", "ask": "Sous-agent ?"}),
            &at(5),
        ))
        .expect("json");
        side["isSidechain"] = json!(true);
        let lines = [prompt("Go", Some("human"), &at(0)), side.to_string()];
        let info = info_of(&lines.join("\n"));
        assert_eq!(info.pending_ask, None);
        assert!(!info.turn.expect("turn").wrapped_up);
    }

    #[test]
    fn closing_questions() {
        let cases = [
            ("Fini. Je pousse la branche ?", Some("Je pousse la branche ?")),
            ("Je pousse ?  \n", Some("Je pousse ?")),
            ("Tu veux que je continue ? \u{1F642}", Some("Tu veux que je continue ?")),
            ("« On fusionne ? »", Some("On fusionne ?")),
            ("(ou bien on attend ?)", Some("ou bien on attend ?")),
            (
                "**Par quoi voulez-vous que je commence ?**",
                Some("Par quoi voulez-vous que je commence ?"),
            ),
            (
                "Voici le bilan.\n\n- point 1\n- point 2\n\nJe commite et j'ouvre la `PR` ?",
                Some("Je commite et j'ouvre la PR ?"),
            ),
            // A short tail reads with its line.
            ("Tout passe. Ok ?", Some("Tout passe. Ok ?")),
            ("Fini, tout passe.", None),
            ("Une question ? Non, c'est réglé.", None),
            ("Vraiment ?!", None),
            ("", None),
            ("  \n ", None),
        ];
        for (closing, expected) in cases {
            assert_eq!(closing_question(closing).as_deref(), expected, "{closing:?}");
        }
    }

    #[test]
    fn closing_question_comes_from_the_text_after_the_last_tool_call() {
        let lines = vec![
            prompt("Go", Some("human"), &at(0)),
            assistant_text("Je regarde ?", &at(1)),
            assistant_tool("t1", "Bash", json!({"command": "ls"}), &at(2)),
            tool_result("t1", false, &at(3)),
            assistant_text("Tout est vert.", &at(4)),
            stop(&at(5)),
        ];
        assert_eq!(turn_of(&lines).closing_question, None);
        let mut asking = lines[..4].to_vec();
        asking.push(assistant_text("Tout est vert.", &at(4)));
        asking.push(assistant_text("Je fusionne ?", &at(5)));
        asking.push(stop(&at(6)));
        assert_eq!(turn_of(&asking).closing_question.as_deref(), Some("Je fusionne ?"));
        // A new turn starts with no closing text.
        asking.push(prompt("Oui", Some("human"), &at(10)));
        assert_eq!(turn_of(&asking).closing_question, None);
    }

    #[test]
    fn api_errors_end_the_turn_until_the_person_writes() {
        let mut lines = vec![
            prompt("Go", Some("human"), &at(0)),
            api_error("rate_limit", "You've hit your session limit · resets 8am", &at(3)),
        ];
        let info = info_of(&lines.join("\n"));
        let error = info.api_error.expect("error");
        assert_eq!(error.at, parse_iso8601_ms(&at(3)));
        assert_eq!(
            error.label.as_deref(),
            Some("rate_limit: You've hit your session limit · resets 8am")
        );
        let turn = info.turn.expect("turn");
        assert!(turn.ended);
        assert_eq!(turn.ended_at, parse_iso8601_ms(&at(3)));
        assert_eq!(
            info.last_assistant_text.as_deref(),
            Some("You've hit your session limit · resets 8am")
        );

        // Overcome in the same turn: gone, and the turn runs again.
        let mut retried = lines.clone();
        retried.push(assistant_text("Je reprends.", &at(10)));
        let info = info_of(&retried.join("\n"));
        assert_eq!(info.api_error, None);
        assert!(!info.turn.expect("turn").ended);

        // A turn another session starts does not clear it by itself…
        let mut pinged = lines.clone();
        pinged.push(prompt("<system-reminder>\nping", None, &at(20)));
        assert!(info_of(&pinged.join("\n")).api_error.is_some());
        // …but the session answering again in it does…
        pinged.push(assistant_text("Rien.", &at(21)));
        pinged.push(stop(&at(22)));
        assert_eq!(info_of(&pinged.join("\n")).api_error, None);
        // …and so does the person's next prompt.
        lines.push(prompt("Réessaie", Some("human"), &at(30)));
        assert_eq!(info_of(&lines.join("\n")).api_error, None);
    }

    #[test]
    fn plan_of_a_previous_turn_is_hidden() {
        let items = json!({"items": [
            {"content": "Un", "status": "completed"},
            {"content": "Deux", "status": "in_progress"}], "scope": "first_task"});
        let mut lines = vec![
            prompt("Fais deux choses", Some("human"), &at(0)),
            assistant_tool("t1", UPDATE_PLAN, items, &at(1)),
            assistant_tool("t2", WRAP_UP, json!({"recap": "Fini."}), &at(2)),
            stop(&at(3)),
        ];
        assert!(info_of(&lines.join("\n")).plan.is_some());
        // Turns the person did not start keep it.
        lines.push(prompt("<task-notification>", Some("task-notification"), &at(10)));
        lines.push(stop(&at(11)));
        assert!(info_of(&lines.join("\n")).plan.is_some());
        // The person's next prompt: the list belongs to an older message.
        lines.push(prompt("Et maintenant ?", Some("human"), &at(20)));
        lines.push(assistant_text("Rien de plus.", &at(21)));
        assert_eq!(info_of(&lines.join("\n")).plan, None);
        // Touched again in this turn: shown, statuses continued.
        lines.push(assistant_tool("t3", UPDATE_PLAN, json!({"advance": true}), &at(22)));
        let plan = info_of(&lines.join("\n")).plan.expect("plan");
        assert_eq!(statuses(&plan), ["completed", "completed"]);
        assert!(!plan.wrapped_up);
    }

    #[test]
    fn a_wrap_up_does_not_bring_back_an_older_plan() {
        let items = json!({"items": [
            {"content": "Un", "status": "completed"},
            {"content": "Deux", "status": "in_progress"},
            {"content": "Trois", "status": "pending"}], "scope": "first_task"});
        let mut lines = vec![
            prompt("Fais trois choses", Some("human"), &at(0)),
            assistant_tool("t1", UPDATE_PLAN, items, &at(1)),
            assistant_tool("t2", WRAP_UP, json!({"recap": "Pause."}), &at(2)),
            stop(&at(3)),
            prompt("Autre chose ?", Some("human"), &at(10)),
            assistant_text("Voilà.", &at(11)),
            assistant_tool("t3", WRAP_UP, json!({"recap": "Répondu."}), &at(12)),
            stop(&at(13)),
        ];
        // Turn 2 ended with a wrap-up but never touched the list.
        assert_eq!(info_of(&lines.join("\n")).plan, None);
        // An advance in a later turn brings it back, continued.
        lines.push(prompt("Reprends", Some("human"), &at(20)));
        lines.push(assistant_tool("t4", UPDATE_PLAN, json!({"advance": true}), &at(21)));
        let plan = info_of(&lines.join("\n")).plan.expect("plan");
        assert_eq!(statuses(&plan), ["completed", "completed", "in_progress"]);
        assert_eq!(plan.updated_at, parse_iso8601_ms(&at(21)));
    }

    #[test]
    fn update_plan_reads_items_like_desktop() {
        let mut items: Vec<Value> = vec![
            json!({"content": "Un", "status": "done"}),
            json!({"content": "  ", "status": "completed"}),
            json!({"content": "Deux", "status": "running"}),
            json!({"content": "Trois", "status": "completed"}),
            json!({"content": "Quatre", "status": "in_progress"}),
        ];
        items.extend((5..=20).map(|n| json!({"content": format!("Étape {n}")})));
        let lines = [assistant_tool("t1", UPDATE_PLAN, json!({"items": items}), &at(0))];
        let plan = info_of(&lines.join("\n")).plan.expect("plan");
        // Exact statuses only, blank items skipped, at most 12 steps.
        assert_eq!(plan.total, 12);
        assert_eq!(&statuses(&plan)[..4], ["pending", "pending", "completed", "in_progress"]);
        assert_eq!(plan.items[1].content, "Deux");

        // `start` is applied before `advance`, as Desktop does.
        let three = json!({"items": [
            {"content": "A", "status": "pending"},
            {"content": "B", "status": "pending"},
            {"content": "C", "status": "pending"}]});
        let lines = [
            assistant_tool("t1", UPDATE_PLAN, three, &at(0)),
            assistant_tool("t2", UPDATE_PLAN, json!({"start": 2, "advance": true}), &at(1)),
        ];
        let plan = info_of(&lines.join("\n")).plan.expect("plan");
        assert_eq!(statuses(&plan), ["pending", "completed", "in_progress"]);
        // TodoWrite keeps its lenient statuses.
        let todos = json!({"todos": [{"content": "x", "status": "done"}]});
        let lines = [assistant_tool("t1", "TodoWrite", todos, &at(0))];
        assert_eq!(statuses(&info_of(&lines.join("\n")).plan.expect("plan")), ["completed"]);
    }

    #[test]
    fn a_peer_turn_that_did_work_is_news() {
        let base = vec![
            prompt("Go", Some("human"), &at(0)),
            assistant_text("Fait.", &at(1)),
            stop(&at(2)),
            prompt("<system-reminder>\nLa session X a fini", None, &at(10)),
        ];
        // A ping answered in a few words: not news.
        let mut ping = base.clone();
        ping.push(assistant_text("Noté.", &at(11)));
        ping.push(stop(&at(12)));
        assert_eq!(info_of(&ping.join("\n")).notable_turn_ended_at, parse_iso8601_ms(&at(2)));
        // Work ended with a wrap-up recap: news, even without a question.
        let mut work = base;
        work.push(assistant_tool("t1", WRAP_UP, json!({"recap": "PR #74 ouverte."}), &at(11)));
        work.push(stop(&at(12)));
        let info = info_of(&work.join("\n"));
        assert_eq!(info.notable_turn_ended_at, parse_iso8601_ms(&at(12)));
        assert_eq!(info.pending_ask, None);
    }

    #[test]
    fn app_actions_become_options() {
        let input = json!({"recap": "Prêt.", "ask": "Je commite ?", "actions": [
            {"action": "commit"},
            {"label": "Pas encore", "message": "non"},
            {"action": "create_pr", "label": "Ouvrir"},
            {"action": "teleport"}]});
        let lines = [assistant_tool("t1", WRAP_UP, input, &at(0))];
        let ask = info_of(&lines.join("\n")).pending_ask.expect("question");
        assert_eq!(ask.options, ["Commiter", "Pas encore", "Ouvrir"]);
    }

    #[test]
    fn plain_text_strips_markdown() {
        let cases = [
            (
                "Je ne touche pas à `feat/6-lot`. C'est [Implémenter #6](#local_6093b050) qui…",
                "Je ne touche pas à feat/6-lot. C'est Implémenter #6 qui…",
            ),
            (
                "**Si une coque n'est pas à jour :** je prépare",
                "Si une coque n'est pas à jour : je prépare",
            ),
            (
                "## Bilan\n\n- **Fix** : ok\n* deux\n+ trois\n1. un\n2) deux",
                "Bilan Fix : ok deux trois un deux",
            ),
            ("> cité\n> > encore", "cité encore"),
            ("- [x] fait\n- [ ] à faire", "fait à faire"),
            ("_italique_ et *aussi* et ~~barré~~", "italique et aussi et barré"),
            ("snake_case, 2*3 et a * b gardés", "snake_case, 2*3 et a * b gardés"),
            ("`**/*.rs` reste tel quel", "**/*.rs reste tel quel"),
            ("![capture](a.png) puis [lien](https://x.y/a_(b))", "capture puis lien)"),
            ("[pas un lien] et [x]", "[pas un lien] et [x]"),
            (
                "avant\n---\naprès\n| a | b |\n|---|:-:|\n| 1 | 2 |",
                "avant après | a | b | | 1 | 2 |",
            ),
            ("```rust\nlet x = 1;\n```\nfin", "let x = 1; fin"),
            ("un⏎deux", "un deux"),
            ("#6 est fait", "#6 est fait"),
            ("-5 degrés", "-5 degrés"),
            ("", ""),
        ];
        for (markdown, expected) in cases {
            assert_eq!(plain_text(markdown, 400), expected, "{markdown:?}");
        }
        assert_eq!(plain_text("[Implémenter #6](#local_x) a fini", 12), "Implémenter…");
    }

    #[test]
    fn sidechain_lines_are_ignored() {
        let info = info_of(&fixture("sidechain.jsonl"));
        let plan = info.plan.expect("plan");
        assert_eq!(statuses(&plan), ["in_progress", "pending"]);
        assert_eq!(info.current_tool, None);
        assert_eq!(info.last_tool.expect("last").name, "update_plan");
        assert_eq!(info.last_assistant_text.as_deref(), Some("Texte principal"));
        assert_eq!(info.git_branch.as_deref(), Some("main"));
        assert_eq!(info.last_event_at, parse_iso8601_ms("2026-10-22T12:00:09Z"));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let text = fixture("malformed.jsonl");
        let lines: Vec<&str> = text.lines().collect();
        // Before the second prompt, the Bash call is still open.
        let current = info_of(&lines[..14].join("\n")).current_tool.expect("current tool");
        assert_eq!(current.name, "Bash");
        assert_eq!(current.summary, "ls");
        assert_eq!(current.at, None);
        let info = info_of(&text);
        assert_eq!(info.last_assistant_text.as_deref(), Some("Toujours là"));
        assert_eq!(info.current_tool, None, "the new prompt closed it");
        assert_eq!(info.git_branch.as_deref(), Some("dev"));
        assert_eq!(info.last_event_at, parse_iso8601_ms("2026-10-22T09:00:05Z"));
        let turn = info.turn.expect("turn");
        assert_eq!(turn.origin, TurnOrigin::Human);
        assert_eq!(turn.started_at, parse_iso8601_ms("2026-10-22T09:00:01Z"));
        assert!(!turn.ended);
    }

    #[test]
    fn skippable_prefixes() {
        assert!(is_skippable(b"{\"type\":\"api-request-blob\",\"sessionId\""));
        assert!(is_skippable(b"{\"type\":\"file-history-snapshot\""));
        assert!(!is_skippable(b"{\"type\":\"assistant\""));
        assert!(!is_skippable(b"{\"parentUuid\":null,\"isSidechain\":false"));
        assert!(!is_skippable(b"{\"type\":\"some-very-long-unterminated"));
        assert!(!is_skippable(b""));
    }

    #[test]
    fn next_line_skips_huge_lines_without_buffering() {
        let mut data = Vec::new();
        data.extend_from_slice(b"{\"type\":\"api-request-blob\",\"message\":\"");
        data.extend(std::iter::repeat_n(b'x', 1 << 20));
        data.extend_from_slice(b"\"}\n{\"type\":\"user\"}\n{\"type\":\"assist");
        let mut reader = BufReader::with_capacity(4096, data.as_slice());
        let mut line = Vec::new();
        let first = next_line(&mut reader, &mut line).expect("read");
        assert!(matches!(first, NextLine::Skipped(n) if n == (1 << 20) + 41));
        assert!(line.capacity() < 8192);
        assert!(matches!(next_line(&mut reader, &mut line), Ok(NextLine::Complete(16))));
        assert_eq!(line, b"{\"type\":\"user\"}\n");
        assert!(matches!(next_line(&mut reader, &mut line), Ok(NextLine::Partial)));
    }

    #[test]
    fn incremental_reads_partial_line_and_truncation() {
        let projects = TempProjects::new("incremental");
        let mut tracker = TranscriptTracker::new(projects.root.clone());
        assert_eq!(tracker.update(SESSION), None);

        let first = assistant_tool("t1", "Bash", json!({"command": "ls"}), "2026-10-22T10:00:00Z");
        projects.append(&format!("{first}\n"));
        // Found only after the rescan delay: forget the failed lookup.
        tracker.failed_lookups.clear();
        let info = tracker.update(SESSION).expect("found");
        assert_eq!(info.current_tool.expect("open").summary, "ls");
        assert!(info.path.ends_with(&format!("{SESSION}.jsonl")));

        // Half a line: not consumed yet.
        let result = tool_result("t1", false, "2026-10-22T10:00:01Z");
        let (head, tail) = result.split_at(result.len() / 2);
        projects.append(head);
        let info = tracker.update(SESSION).expect("found");
        assert!(info.current_tool.is_some());
        assert_eq!(info.last_event_at, parse_iso8601_ms("2026-10-22T10:00:00Z"));

        // The rest of the line arrives.
        projects.append(&format!("{tail}\n"));
        let info = tracker.update(SESSION).expect("found");
        assert_eq!(info.current_tool, None);
        assert_eq!(info.last_event_at, parse_iso8601_ms("2026-10-22T10:00:01Z"));
        let offset = tracker.files[SESSION].offset;
        assert_eq!(offset, fs::metadata(projects.transcript()).expect("meta").len());

        // Nothing new: same view.
        assert_eq!(tracker.update(SESSION).expect("found").last_event_at, info.last_event_at);

        // The file shrinks: everything is read again from the start.
        let shorter = assistant_tool("t9", "Grep", json!({"pattern": "neuf"}), &at(0));
        fs::write(projects.transcript(), format!("{shorter}\n")).expect("rewrite");
        let info = tracker.update(SESSION).expect("found");
        assert_eq!(info.current_tool.expect("open").summary, "neuf");
        assert_eq!(info.last_event_at, parse_iso8601_ms(&at(0)));

        // Rewritten with a longer, older content: detected because the byte
        // before the offset no longer ends a line.
        let older = "2026-10-22T09:59:00Z";
        let longer = assistant_tool("t10", "Glob", json!({"pattern": "*.toml"}), older);
        fs::write(projects.transcript(), format!("{longer}\n{longer}\n")).expect("rewrite");
        let info = tracker.update(SESSION).expect("found");
        assert_eq!(info.current_tool.expect("open").summary, "*.toml");
        assert_eq!(info.last_event_at, parse_iso8601_ms(older));
        assert_eq!(tracker.files[SESSION].state.open_tools.len(), 1);

        // Deleted: gone, and not searched again right away.
        fs::remove_file(projects.transcript()).expect("remove");
        assert_eq!(tracker.update(SESSION), None);
        assert!(tracker.files.is_empty());
        projects.append(&format!("{first}\n"));
        assert_eq!(tracker.update(SESSION), None);
    }

    #[test]
    fn retain_and_invalid_ids() {
        let projects = TempProjects::new("retain");
        projects.append(&format!(
            "{}\n",
            assistant_tool("t1", "Bash", json!({"command": "ls"}), "2026-10-22T10:00:00Z")
        ));
        let mut tracker = TranscriptTracker::new(projects.root.clone());
        assert!(tracker.update(SESSION).is_some());
        assert_eq!(tracker.update("../secret"), None);
        assert_eq!(tracker.update(""), None);
        assert_eq!(tracker.update("missing-id"), None);
        assert_eq!(tracker.failed_lookups.len(), 1);
        tracker.retain(&HashSet::from([SESSION.to_owned()]));
        assert_eq!(tracker.files.len(), 1);
        assert!(tracker.failed_lookups.is_empty());
        tracker.retain(&HashSet::new());
        assert!(tracker.files.is_empty());
    }

    /// Read-only check against the live machine:
    /// `cargo test --release live_check -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the live ~/.claude folder"]
    fn live_check() {
        let mut sessions = Vec::new();
        for entry in fs::read_dir(crate::paths::sessions_dir()).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(value) = serde_json::from_slice::<Value>(&bytes) else { continue };
            if let Some(id) = str_field(&value, "sessionId") {
                let name = str_field(&value, "name").unwrap_or("").to_owned();
                sessions.push((id.to_owned(), name));
            }
        }
        let projects = crate::paths::projects_dir();
        let mut largest: Option<(u64, String)> = None;
        for (id, name) in &sessions {
            let Some(path) = find_transcript(&projects, id) else { continue };
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            println!("{id} {size:>10} B  {name}");
            if largest.as_ref().is_none_or(|(s, _)| size > *s) {
                largest = Some((size, id.clone()));
            }
        }
        if let Some((id, _)) = sessions.iter().find(|(_, n)| n == "Claude Desktop monitoring app") {
            let mut tracker = TranscriptTracker::new(projects.clone());
            let start = Instant::now();
            let info = tracker.update(id);
            println!("monitoring session read in {:?}", start.elapsed());
            println!("{}", serde_json::to_string_pretty(&info).unwrap_or_default());
            if let Some(plan) = tracker.files.get(id).and_then(|f| f.state.plan.as_ref()) {
                let kept: Vec<&str> = plan.items.iter().map(|i| i.status.as_str()).collect();
                println!("statuses kept behind wrapped_up={}: {kept:?}", plan.wrapped_up);
            }
            let start = Instant::now();
            tracker.update(id);
            println!("second update (nothing new) in {:?}", start.elapsed());
        }
        if let Some((size, id)) = largest {
            let mut tracker = TranscriptTracker::new(projects);
            let start = Instant::now();
            let info = tracker.update(&id);
            println!("largest live transcript {id} ({size} B) first read in {:?}", start.elapsed());
            if let Some(info) = info {
                println!(
                    "  plan={:?} current={:?} last={:?} branch={:?}",
                    info.plan.map(|p| (p.done, p.total, p.current, p.wrapped_up)),
                    info.current_tool.map(|t| (t.name, t.summary)),
                    info.last_tool.map(|t| (t.name, t.summary)),
                    info.git_branch
                );
            }
        }
    }

    /// Replays real transcripts and prints, at each end of turn, the state
    /// the monitor derives with an idle registry and no Desktop file.
    /// Read-only: `CSM_REPLAY=<cli id>[,<cli id>…] cargo test live_turn_replay
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the live ~/.claude folder"]
    fn live_turn_replay() {
        use crate::aggregate::{StateInput, derive_state};
        let ids = std::env::var("CSM_REPLAY").unwrap_or_default();
        let projects = crate::paths::projects_dir();
        for id in ids.split(',').map(str::trim).filter(|id| !id.is_empty()) {
            let Some(path) = find_transcript(&projects, id) else {
                println!("{id}: not found");
                continue;
            };
            let Ok(text) = fs::read_to_string(&path) else { continue };
            println!("== {id}");
            let mut state = TranscriptState::default();
            let mut was_ended = true;
            for line in text.lines() {
                state.apply_line(line.as_bytes());
                let ended = state.turn.as_ref().is_some_and(|t| t.ended);
                if ended && !was_ended {
                    let info = state.info(&path);
                    let turn = info.turn.clone().unwrap_or_default();
                    let end = turn.ended_at.unwrap_or_default();
                    let derived = derive_state(&StateInput {
                        now_ms: end + 60_000,
                        status: Some("idle"),
                        transcript: Some(&info),
                        ..StateInput::default()
                    });
                    let question = derived.question.map(|q| (q.source, q.text, q.options));
                    println!(
                        "{} {:?} -> {:?} {:?} {:?}",
                        line_time(end),
                        turn.origin,
                        derived.state,
                        derived.waiting_for,
                        question
                    );
                }
                was_ended = ended;
            }
        }
    }

    /// `HH:MM:SS` (UTC) of an epoch time in milliseconds.
    fn line_time(ms: i64) -> String {
        let secs = ms.div_euclid(1000).rem_euclid(86_400);
        format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    }

    /// Replays every transcript modified in the last 7 days and counts how
    /// turns end. Read-only:
    /// `cargo test live_turn_survey -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the live ~/.claude folder"]
    fn live_turn_survey() {
        let since = SystemTime::now() - Duration::from_secs(7 * 86_400);
        let mut counts: HashMap<String, u32> = HashMap::new();
        let mut bump = |key: String| *counts.entry(key).or_default() += 1;
        let projects = fs::read_dir(crate::paths::projects_dir()).into_iter().flatten();
        for project in projects.flatten() {
            for file in fs::read_dir(project.path()).into_iter().flatten().flatten() {
                let path = file.path();
                let recent = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t >= since);
                if path.extension().is_none_or(|e| e != "jsonl") || !recent {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else { continue };
                let mut state = TranscriptState::default();
                let mut was_ended = true;
                let mut turns_seen = 0;
                for line in text.lines() {
                    state.apply_line(line.as_bytes());
                    let ended = state.turn.as_ref().is_some_and(|t| t.ended);
                    let same_turn = state.turn_count == turns_seen;
                    if was_ended && !ended && same_turn && state.turn.is_some() {
                        bump("reopened after its end (no prompt)".to_owned());
                        let name = file.file_name().to_string_lossy().into_owned();
                        println!("reopened: {name} at {:?}", state.last_event_at.map(line_time));
                    }
                    if ended && !was_ended {
                        let info = state.info(&path);
                        let turn = info.turn.unwrap_or_default();
                        let end = if info.pending_ask.is_some() {
                            "decision"
                        } else if info.api_error.is_some() {
                            "api error"
                        } else if !turn.wrapped_up && turn.closing_question.is_some() {
                            "probable question"
                        } else {
                            "nothing pending"
                        };
                        bump(format!("{:?} turn ended: {end}", turn.origin));
                        if !state.open_tools.is_empty() {
                            bump("open tools at the end".to_owned());
                        }
                    }
                    was_ended = ended;
                    turns_seen = state.turn_count;
                }
            }
        }
        let mut lines: Vec<_> = counts.into_iter().collect();
        lines.sort();
        for (key, count) in lines {
            println!("{count:6} {key}");
        }
    }

    /// Steps in progress and step count announced by Desktop's answer to
    /// `update_plan`: "Plan updated: steps 4, 5 of 7 are in progress…".
    fn announced_plan(text: &str) -> Option<(Vec<usize>, usize)> {
        let rest = text.strip_prefix("Plan updated: ")?;
        let rest = rest.strip_prefix("steps ").or_else(|| rest.strip_prefix("step "))?;
        let (steps, rest) = rest.split_once(" of ")?;
        let total = rest.split(' ').next()?.parse().ok()?;
        let steps = steps.split(", ").map(str::parse).collect::<Result<_, _>>().ok()?;
        Some((steps, total))
    }

    /// Text of a tool result's `content`: a string or a list of text parts.
    fn result_text(block: &Value) -> String {
        match block.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => {
                parts.iter().filter_map(|p| str_field(p, "text")).collect()
            }
            _ => String::new(),
        }
    }

    /// Answers checked, mismatching and not understood in one replay.
    #[derive(Default)]
    struct Tally {
        checked: u32,
        mismatches: u32,
        unparsed: u32,
    }

    /// Replays one transcript, comparing the rebuilt plan with each of
    /// Desktop's answers to `update_plan`.
    fn check_plan_answers(name: &str, text: &str, tally: &mut Tally) {
        let mut state = TranscriptState::default();
        let mut plan_calls = HashSet::new();
        for (number, line) in text.lines().enumerate() {
            state.apply_line(line.as_bytes());
            let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
            if let Err(e) = serde_json::from_str::<Line>(line) {
                println!("{name} line {number}: parses as Value but not as Line: {e}");
            }
            // `api-request-blob` lines replay older messages: only real ones count.
            if !matches!(str_field(&value, "type"), Some("user" | "assistant")) {
                continue;
            }
            let blocks = value.pointer("/message/content").and_then(Value::as_array);
            for block in blocks.into_iter().flatten() {
                if str_field(block, "name") == Some(UPDATE_PLAN) {
                    plan_calls.extend(str_field(block, "id").map(str::to_owned));
                }
                let Some(id) = str_field(block, "tool_use_id") else { continue };
                if !plan_calls.remove(id) || block.get("is_error") == Some(&Value::Bool(true)) {
                    continue;
                }
                let answer = result_text(block);
                let Some((steps, total)) = announced_plan(&answer) else {
                    if !answer.starts_with("Plan shown") {
                        tally.unparsed += 1;
                        println!("{name} line {number}: unparsed {}", one_line(&answer, 80));
                    }
                    continue;
                };
                tally.checked += 1;
                let plan = state.plan.as_ref().map(Plan::progress).unwrap_or_default();
                let rebuilt: Vec<usize> = (1..)
                    .zip(&plan.items)
                    .filter(|(_, item)| item.status == "in_progress")
                    .map(|(n, _)| n)
                    .collect();
                if rebuilt != steps || plan.total as usize != total {
                    tally.mismatches += 1;
                    println!(
                        "{name}:{number}: Desktop says {steps:?} of {total}, rebuilt {:?} of {}",
                        rebuilt, plan.total
                    );
                }
            }
        }
    }

    /// Replays every real transcript modified in the last 30 days and checks
    /// the rebuilt plan against Desktop's own answers. Read-only:
    /// `cargo test --release live_plan_consistency -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the live ~/.claude folder"]
    fn live_plan_consistency() {
        let since = SystemTime::now() - Duration::from_secs(30 * 86_400);
        let mut tally = Tally::default();
        let projects = fs::read_dir(crate::paths::projects_dir()).into_iter().flatten();
        for project in projects.flatten() {
            for file in fs::read_dir(project.path()).into_iter().flatten().flatten() {
                let path = file.path();
                let recent = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t >= since);
                if path.extension().is_none_or(|e| e != "jsonl") || !recent {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else { continue };
                let name = file.file_name().to_string_lossy().into_owned();
                check_plan_answers(&name, &text, &mut tally);
            }
        }
        println!(
            "checked {} answers, {} mismatches, {} unparsed",
            tally.checked, tally.mismatches, tally.unparsed
        );
        assert_eq!(tally.mismatches, 0);
    }
}
