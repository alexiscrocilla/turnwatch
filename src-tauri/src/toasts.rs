//! Windows toasts reminding the user of sessions still waiting for them.
//!
//! Claude Desktop already notifies when a session ends its turn or starts
//! waiting, so the monitor does not repeat it: its toasts are reminders. A
//! wait, or a turn ended on an error, is announced only once it has lasted
//! [`REMINDER_DELAY_MS`] (counted from the start of the episode) and is
//! still the same episode (same key, state and `state_since`), i.e. still
//! unanswered. Until then it is held back; an episode that ends first
//! (answered, replaced by another, session gone) is never announced.
//!
//! Never announced:
//! - a "probable question" wait (the last message merely ends with `?`);
//! - a session while it is on screen (shown in a Claude Desktop pane while
//!   Desktop is in front): it is announced once it leaves the screen still
//!   in the same episode and the delay has passed;
//! - an episode already announced (once per episode, even after a flap or
//!   when the session drops out of the list and comes back), nor one already
//!   there when the monitor started or the session came back.
//!
//! Several reminders due in the same [`Notifier::select`] or
//! [`Notifier::tick`] are coalesced into one toast ("N sessions t'attendent
//! toujours", their titles in the body) whose click shows the monitor's
//! window ([`SUMMARY_KEY`]). A single one names its session and opens it.
//!
//! Finished turns are only announced when `notify_on_finished` is on (off by
//! default), right away, and never for a session on screen.
//!
//! On Windows the toasts are shown with the app's own AppUserModelID (see
//! `app_identity`), so they carry its name and icon. A toast about a Claude
//! Desktop session opens it through Desktop's `claude://` link (protocol
//! activation), which also works from the Notification Center once the
//! banner is gone and even after the monitor quit. When that path is not
//! available the notification plugin shows the toast instead.

use std::collections::HashMap;

use monitor_core::model::{SessionState, SessionView, Settings, Snapshot, Transition};
use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::NotificationExt;

use crate::quota_alerts::QuotaAlerts;

/// A wait seen when its session is first listed (it just appeared or came
/// back) is announced only when it started this recently.
const FRESH_WAIT_MS: i64 = 15_000;
/// Reaching "unseen" this soon after leaving "working" counts as finishing a
/// turn, even through another state in between: the registry can go idle a
/// few seconds before Desktop records the end of the turn.
const FINISH_WINDOW_MS: i64 = 60_000;
/// Announced episodes are remembered this long, and at most this many.
const EPISODE_MEMORY_MS: i64 = 12 * 60 * 60 * 1000;
const EPISODE_MEMORY_MAX: usize = 1024;
/// Longest toast body, in characters.
const BODY_MAX_CHARS: usize = 300;
/// A wait or error is announced once it has lasted this long unanswered.
pub const REMINDER_DELAY_MS: i64 = 5 * 60 * 1000;
/// `waiting_for` of a wait only guessed from a trailing question mark.
const PROBABLE_QUESTION: &str = "probable question";
/// Keys of the toasts about the monitor itself (several sessions, the quota,
/// a setting) start with this: a click shows the monitor. Each kind has its
/// own key, because Windows replaces a toast that has the same key.
pub const MONITOR_PREFIX: &str = "monitor:";
/// Key of a toast coalescing several sessions.
pub const SUMMARY_KEY: &str = "monitor:summary";

/// One toast to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// Session key, used to open the session when the toast is clicked;
    /// a key starting with [`MONITOR_PREFIX`] for a toast about the monitor.
    pub key: String,
    pub title: String,
    pub body: String,
}

impl Toast {
    /// A toast about the monitor, not one session: its click shows the monitor.
    pub fn is_summary(&self) -> bool {
        shows_monitor(&self.key)
    }
}

/// A click on the toast with this key shows the monitor.
pub fn shows_monitor(key: &str) -> bool {
    key.starts_with(MONITOR_PREFIX)
}

/// What a toast announces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Waiting,
    Error,
    Finished,
}

/// Turns snapshot transitions into toasts.
#[derive(Debug, Default)]
pub struct Notifier {
    seen_first_snapshot: bool,
    /// Episodes (key, state, state_since) already announced or not to be
    /// announced, with the time they were recorded.
    announced: HashMap<(String, SessionState, i64), i64>,
    /// Session key -> when it was last seen leaving Working.
    left_working: HashMap<String, i64>,
    /// Waits and errors held back until they have lasted long enough.
    deferred: HashMap<String, Deferred>,
    /// Plan usage thresholds already announced.
    quota: QuotaAlerts,
}

/// An episode held back until it is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Deferred {
    kind: Kind,
    state: SessionState,
    /// Its `state_since`; None when the session gave none.
    since: Option<i64>,
    /// When it was held back.
    at: i64,
}

impl Deferred {
    /// When the episode started: its `state_since`, else when it was seen.
    fn start(&self) -> i64 {
        self.since.unwrap_or(self.at)
    }
}

impl Notifier {
    /// Toasts for one snapshot's transitions. `snapshot` is the snapshot the
    /// transitions lead to; `now_ms` is the wall clock (epoch ms).
    pub fn select(
        &mut self,
        transitions: &[Transition],
        snapshot: &Snapshot,
        settings: &Settings,
        now_ms: i64,
    ) -> Vec<Toast> {
        self.forget_old(now_ms);
        for transition in transitions {
            if transition.from == Some(SessionState::Working) {
                self.left_working.insert(transition.key.clone(), now_ms);
            }
        }
        // Quiet on the first sight of the quota, like the sessions below.
        let mut toasts = self.quota.check(snapshot.quota.as_ref(), settings, now_ms);
        if !self.seen_first_snapshot {
            self.seen_first_snapshot = true;
            self.record_listed(snapshot, now_ms);
            return toasts;
        }
        for transition in transitions {
            let Some(kind) = self.kind(transition, settings, now_ms) else {
                continue;
            };
            if kind == Kind::Finished {
                self.left_working.remove(&transition.key);
                // A finished turn seen on screen was seen.
                if self.first_announcement(transition, now_ms) && !transition.on_screen {
                    let session = snapshot.sessions.iter().find(|s| s.key == transition.key);
                    toasts.push(toast(kind, transition, session));
                }
                continue;
            }
            // Held back, not recorded: announced if it lasts unanswered.
            if !self.is_announced(transition) {
                self.deferred.insert(
                    transition.key.clone(),
                    Deferred {
                        kind,
                        state: transition.to,
                        since: transition.state_since,
                        at: now_ms,
                    },
                );
            }
        }
        toasts.extend(self.release_deferred(snapshot, settings, now_ms));
        self.record_listed(snapshot, now_ms);
        toasts
    }

    /// Toasts for the held-back episodes that are due, without new
    /// transitions: call it every few seconds with the latest snapshot, so a
    /// wait is announced soon after it has lasted [`REMINDER_DELAY_MS`].
    pub fn tick(&mut self, snapshot: &Snapshot, settings: &Settings, now_ms: i64) -> Vec<Toast> {
        if !self.seen_first_snapshot {
            return Vec::new();
        }
        self.release_deferred(snapshot, settings, now_ms)
    }

    /// The toast when Desktop stopped sampling the quota while sessions
    /// work (see `QuotaAlerts::check_stop`): call it every few seconds.
    pub fn quota_stop(
        &mut self,
        snapshot: &Snapshot,
        settings: &Settings,
        now_ms: i64,
        idle_ms: Option<i64>,
    ) -> Option<Toast> {
        self.quota.check_stop(snapshot, settings, now_ms, idle_ms)
    }

    /// Announces held-back episodes whose session is still in the same
    /// state since the same time, off screen, not a probable question, and
    /// started at least [`REMINDER_DELAY_MS`] ago; several are coalesced
    /// into one toast. Episodes that ended (answered, session gone) or whose
    /// setting was turned off are dropped. An unready snapshot (no sessions
    /// read yet) decides nothing.
    fn release_deferred(
        &mut self,
        snapshot: &Snapshot,
        settings: &Settings,
        now_ms: i64,
    ) -> Vec<Toast> {
        if self.deferred.is_empty() || !snapshot.ready {
            return Vec::new();
        }
        let mut due: Vec<(usize, Toast, String)> = Vec::new();
        let deferred = std::mem::take(&mut self.deferred);
        for (key, held) in deferred {
            let Some((index, session)) = snapshot
                .sessions
                .iter()
                .enumerate()
                .find(|(_, s)| s.key == key)
            else {
                continue;
            };
            if session.state != held.state || session.state_since != held.since {
                continue;
            }
            let enabled = match held.kind {
                Kind::Waiting => settings.notify_on_waiting,
                Kind::Error => settings.notify_on_error,
                Kind::Finished => settings.notify_on_finished,
            };
            if !enabled {
                continue;
            }
            let probable = held.kind == Kind::Waiting
                && is_probable_question(session.waiting_for.as_deref());
            let early = now_ms.saturating_sub(held.start()) < REMINDER_DELAY_MS;
            if probable || early || session.on_screen {
                self.deferred.insert(key, held);
                continue;
            }
            let transition = Transition {
                key: session.key.clone(),
                title: session.title.clone(),
                from: None,
                to: session.state,
                waiting_for: session.waiting_for.clone(),
                state_since: session.state_since,
                on_screen: false,
            };
            if self.first_announcement(&transition, now_ms) {
                let name = display_name(&transition).to_owned();
                due.push((index, toast(held.kind, &transition, Some(session)), name));
            }
        }
        due.sort_by_key(|(index, _, _)| *index);
        coalesce(due.into_iter().map(|(_, toast, name)| (toast, name)).collect())
    }

    /// Records every listed wait, error and unseen turn as known, so seeing
    /// the same episode again later (a flap) does not announce it: those
    /// already there when the monitor started or a session came back, and
    /// those seen while their setting was off. Held-back ones are skipped.
    fn record_listed(&mut self, snapshot: &Snapshot, now_ms: i64) {
        for session in &snapshot.sessions {
            if !matches!(
                session.state,
                SessionState::Waiting | SessionState::Error | SessionState::Unseen
            ) || self.deferred.contains_key(&session.key)
            {
                continue;
            }
            if let Some(since) = session.state_since {
                self.announced
                    .entry((session.key.clone(), session.state, since))
                    .or_insert(now_ms);
            }
        }
    }

    /// The episode of `transition` was already recorded.
    fn is_announced(&self, transition: &Transition) -> bool {
        transition.state_since.is_some_and(|since| {
            self.announced
                .contains_key(&(transition.key.clone(), transition.to, since))
        })
    }

    /// What `transition` announces, if the settings and the rules ask for it.
    fn kind(&self, transition: &Transition, settings: &Settings, now_ms: i64) -> Option<Kind> {
        let from = transition.from;
        match transition.to {
            SessionState::Waiting
                if settings.notify_on_waiting
                    && !is_probable_question(transition.waiting_for.as_deref()) =>
            {
                // A session that just appeared is only announced for a wait
                // that just began: older ones were there before.
                let new_wait = from.is_some()
                    || transition
                        .state_since
                        .is_some_and(|since| now_ms.saturating_sub(since) <= FRESH_WAIT_MS);
                new_wait.then_some(Kind::Waiting)
            }
            SessionState::Error if settings.notify_on_error && from.is_some() => Some(Kind::Error),
            SessionState::Unseen if settings.notify_on_finished && from.is_some() => {
                let finished = from == Some(SessionState::Working)
                    || self
                        .left_working
                        .get(&transition.key)
                        .is_some_and(|at| now_ms.saturating_sub(*at) <= FINISH_WINDOW_MS);
                finished.then_some(Kind::Finished)
            }
            _ => None,
        }
    }

    /// Records the episode of `transition`; false when it was already
    /// recorded. Without a start time every change counts as a new episode.
    fn first_announcement(&mut self, transition: &Transition, now_ms: i64) -> bool {
        let Some(since) = transition.state_since else {
            return true;
        };
        self.announced
            .insert((transition.key.clone(), transition.to, since), now_ms)
            .is_none()
    }

    fn forget_old(&mut self, now_ms: i64) {
        self.announced
            .retain(|_, at| now_ms.saturating_sub(*at) < EPISODE_MEMORY_MS);
        if self.announced.len() > EPISODE_MEMORY_MAX {
            let mut times: Vec<i64> = self.announced.values().copied().collect();
            times.sort_unstable();
            let cutoff = times[times.len() - EPISODE_MEMORY_MAX];
            self.announced.retain(|_, at| *at >= cutoff);
        }
        self.left_working
            .retain(|_, at| now_ms.saturating_sub(*at) <= FINISH_WINDOW_MS);
        self.deferred
            .retain(|_, held| now_ms.saturating_sub(held.at) < EPISODE_MEMORY_MS);
    }
}

/// The wait was only guessed from a trailing question mark.
fn is_probable_question(waiting_for: Option<&str>) -> bool {
    waiting_for.map(str::trim) == Some(PROBABLE_QUESTION)
}

/// One toast per due reminder when there is only one, else a single toast
/// listing the sessions (`name` of each), whose click shows the monitor.
fn coalesce(due: Vec<(Toast, String)>) -> Vec<Toast> {
    if due.len() < 2 {
        return due.into_iter().map(|(toast, _)| toast).collect();
    }
    let names: Vec<String> = due.into_iter().map(|(_, name)| name).collect();
    vec![Toast {
        key: SUMMARY_KEY.to_owned(),
        title: format!("{} sessions t'attendent toujours", names.len()),
        body: one_line(&names.join(" · "), BODY_MAX_CHARS),
    }]
}

fn toast(kind: Kind, transition: &Transition, session: Option<&SessionView>) -> Toast {
    let name = display_name(transition);
    let (title, body) = match kind {
        Kind::Waiting => (
            format!("{name} t'attend toujours"),
            waiting_body(transition, session),
        ),
        Kind::Error => (
            format!("{name} : erreur"),
            "Le dernier tour s'est arrêté sur une erreur".to_owned(),
        ),
        Kind::Finished => (
            format!("{name} a terminé"),
            "Résultat pas encore consulté".to_owned(),
        ),
    };
    Toast {
        key: transition.key.clone(),
        title,
        body,
    }
}

fn display_name(transition: &Transition) -> &str {
    let title = transition.title.trim();
    if title.is_empty() { "Une session" } else { title }
}

/// The question the session ended its turn on, else the reason it waits.
fn waiting_body(transition: &Transition, session: Option<&SessionView>) -> String {
    let question = session
        .and_then(|s| s.question.as_ref())
        .map(|q| one_line(&q.text, BODY_MAX_CHARS))
        .filter(|text| !text.is_empty());
    question.unwrap_or_else(|| waiting_reason(transition.waiting_for.as_deref()))
}

/// French wording of the session's `waitingFor` value.
pub fn waiting_reason(raw: Option<&str>) -> String {
    match raw.map(str::trim) {
        Some("input needed") => "Une question attend ta réponse".to_owned(),
        Some("permission prompt") => "Une permission est demandée".to_owned(),
        Some("sandbox request") => "Accès sandbox demandé".to_owned(),
        Some("worker request") => "Un worker attend une réponse".to_owned(),
        Some("dialog open") => "Une boîte de dialogue est ouverte".to_owned(),
        Some("decision") => "Une décision est attendue".to_owned(),
        Some("probable question") => "Question dans le dernier message".to_owned(),
        Some("offer") => "Un choix de session est attendu".to_owned(),
        Some("plan approval") => "Un plan attend ton approbation".to_owned(),
        Some(other) if !other.is_empty() => other.to_owned(),
        _ => "Action requise".to_owned(),
    }
}

/// `text` on one line, blanks collapsed, cut to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let mut cut: String = line.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Shows a toast through the notification plugin (Windows PowerShell's AUMID
/// when the exe runs from `target\`). Failures are ignored: a missed toast
/// must not disturb the app.
pub fn show_with_plugin<R: Runtime>(app: &AppHandle<R>, toast: &Toast) {
    let _ = app
        .notification()
        .builder()
        .id(toast_id(&toast.key))
        .title(&toast.title)
        .body(&toast.body)
        .show();
}

/// Stable notification id derived from the session key (FNV-1a, positive).
pub fn toast_id(key: &str) -> i32 {
    let hash = key.bytes().fold(0x811c_9dc5_u32, |acc, b| {
        (acc ^ u32::from(b)).wrapping_mul(0x0100_0193)
    });
    (hash & 0x7fff_ffff) as i32
}

/// Toast tag: one toast per session in the Notification Center, a newer one
/// replacing the older.
pub fn toast_tag(key: &str) -> String {
    format!("{:08x}", toast_id(key))
}

/// Group of every toast of this app.
pub const TOAST_GROUP: &str = "sessions";

/// The toast's XML. With `link`, a click (on the banner or in the
/// Notification Center) launches that URI; without it, it activates the app,
/// which only hears it while running. Silent, like the plugin's toasts.
pub fn toast_xml(toast: &Toast, link: Option<&str>) -> String {
    let activation = link
        .map(|link| format!(r#" launch="{}" activationType="protocol""#, xml_escape(link)))
        .unwrap_or_default();
    format!(
        r#"<toast{activation}><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual><audio silent="true"/></toast>"#,
        xml_escape(&toast.title),
        xml_escape(&toast.body),
    )
}

/// Escapes text for XML content or attributes, replacing characters XML 1.0
/// does not allow with a space.
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || matches!(c, '\u{FFFE}' | '\u{FFFF}') => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Native WinRT toasts, shown with the app's registered AUMID.
#[cfg(windows)]
pub mod native {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::core::HSTRING;

    use super::{TOAST_GROUP, Toast, toast_tag, toast_xml};

    /// Shows `toast` under `app_id`. `on_click` runs when Windows reports a
    /// click to this process (while it runs and the toast is alive). The
    /// returned notification should be kept alive for that.
    pub fn show(
        app_id: &str,
        toast: &Toast,
        link: Option<&str>,
        on_click: impl Fn() + Send + 'static,
    ) -> windows::core::Result<ToastNotification> {
        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(toast_xml(toast, link)))?;
        let notification = ToastNotification::CreateToastNotification(&xml)?;
        notification.SetTag(&HSTRING::from(toast_tag(&toast.key)))?;
        notification.SetGroup(&HSTRING::from(TOAST_GROUP))?;
        notification.Activated(&TypedEventHandler::new(move |_, _| {
            on_click();
            Ok(())
        }))?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(app_id))?
            .Show(&notification)?;
        Ok(notification)
    }
}

#[cfg(test)]
mod tests {
    use monitor_core::model::PendingQuestion;

    use super::*;

    /// Wall clock of the tests.
    const NOW: i64 = 1_800_000_000_000;
    /// When a wait made by [`transition`] (started at `NOW - 1_000`) is due.
    const DUE: i64 = NOW - 1_000 + REMINDER_DELAY_MS;

    fn transition(key: &str, from: Option<SessionState>, to: SessionState) -> Transition {
        Transition {
            key: key.to_owned(),
            title: format!("Session {key}"),
            from,
            to,
            waiting_for: (to == SessionState::Waiting).then(|| "permission prompt".to_owned()),
            state_since: Some(NOW - 1_000),
            on_screen: false,
        }
    }

    fn since(mut t: Transition, at: Option<i64>) -> Transition {
        t.state_since = at;
        t
    }

    fn empty() -> Snapshot {
        Snapshot::default()
    }

    fn primed() -> Notifier {
        let mut notifier = Notifier::default();
        assert!(
            notifier
                .select(
                    &[transition("a", None, SessionState::Waiting)],
                    &empty(),
                    &Settings::default(),
                    NOW
                )
                .is_empty(),
            "the first snapshot never toasts"
        );
        notifier
    }

    #[test]
    fn quota_thresholds_toast_with_the_snapshots() {
        use monitor_core::model::{Quota, QuotaWindow};
        let with_quota = |percent: f64| Snapshot {
            ready: true,
            quota: Some(Quota {
                sampled_at: NOW,
                five_hour: Some(QuotaWindow {
                    percent,
                    ends_by: NOW + 3_600_000,
                    ..QuotaWindow::default()
                }),
                seven_day: None,
                fed_by_sessions: false,
            }),
            ..Snapshot::default()
        };
        let mut notifier = Notifier::default();
        // The first snapshot: already above, so quiet.
        assert!(notifier.select(&[], &with_quota(85.0), &Settings::default(), NOW).is_empty());
        assert!(notifier.select(&[], &with_quota(90.0), &Settings::default(), NOW).is_empty());
        let toasts = notifier.select(&[], &with_quota(95.0), &Settings::default(), NOW);
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].title, "Quota 5 h à 95 %");
        assert!(toasts[0].is_summary());
    }

    fn all_on() -> Settings {
        Settings {
            notify_on_waiting: true,
            notify_on_finished: true,
            notify_on_error: true,
            ..Settings::default()
        }
    }

    /// A ready snapshot listing `sessions`.
    fn listing(sessions: Vec<SessionView>) -> Snapshot {
        Snapshot {
            ready: true,
            sessions,
            ..Snapshot::default()
        }
    }

    /// The view of a session as `transition` leaves it.
    fn view_of(t: &Transition) -> SessionView {
        SessionView {
            key: t.key.clone(),
            title: t.title.clone(),
            state: t.to,
            waiting_for: t.waiting_for.clone(),
            state_since: t.state_since,
            on_screen: t.on_screen,
            ..SessionView::default()
        }
    }

    fn on_screen(mut t: Transition) -> Transition {
        t.on_screen = true;
        t
    }

    /// Toasts of `transitions` seen at `NOW`, then of a tick once they are
    /// due, the sessions staying as the transitions left them.
    fn announce(notifier: &mut Notifier, transitions: &[Transition], settings: &Settings) -> Vec<Toast> {
        let snapshot = listing(transitions.iter().map(view_of).collect());
        let mut toasts = notifier.select(transitions, &snapshot, settings, NOW);
        toasts.extend(notifier.tick(&snapshot, settings, DUE));
        toasts
    }

    #[test]
    fn waiting_toast_has_french_reason() {
        let mut notifier = primed();
        let toasts = announce(
            &mut notifier,
            &[transition("a", Some(SessionState::Working), SessionState::Waiting)],
            &Settings::default(),
        );
        assert_eq!(
            toasts,
            vec![Toast {
                key: "a".into(),
                title: "Session a t'attend toujours".into(),
                body: "Une permission est demandée".into(),
            }]
        );
        assert!(!toasts[0].is_summary());
    }

    #[test]
    fn waiting_toast_shows_the_question() {
        let mut t = transition("a", Some(SessionState::Working), SessionState::Waiting);
        t.waiting_for = Some("decision".into());
        let mut snapshot = listing(vec![view_of(&t)]);
        snapshot.sessions[0].question = Some(PendingQuestion {
            text: "Pousser feat/6-lot et\nouvrir la PR ?".into(),
            options: vec!["Pousser et ouvrir".into(), "Pas encore".into()],
            source: "wrap_up".into(),
            at: Some(NOW - 1_000),
        });
        let mut notifier = primed();
        assert!(notifier.select(std::slice::from_ref(&t), &snapshot, &Settings::default(), NOW).is_empty());
        let toasts = notifier.tick(&snapshot, &Settings::default(), DUE);
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].title, "Session a t'attend toujours");
        assert_eq!(toasts[0].body, "Pousser feat/6-lot et ouvrir la PR ?");

        // Without a question text, the reason is used.
        snapshot.sessions[0].question = None;
        let mut notifier = primed();
        notifier.select(&[t], &snapshot, &Settings::default(), NOW);
        let toasts = notifier.tick(&snapshot, &Settings::default(), DUE);
        assert_eq!(toasts[0].body, "Une décision est attendue");
    }

    #[test]
    fn waits_are_reminders_after_the_delay() {
        let mut notifier = primed();
        let wait = transition("a", Some(SessionState::Working), SessionState::Waiting);
        let snapshot = listing(vec![view_of(&wait)]);
        let settings = Settings::default();
        assert!(notifier.select(&[wait], &snapshot, &settings, NOW).is_empty());
        assert!(notifier.tick(&snapshot, &settings, NOW + 45_000).is_empty());
        assert!(notifier.tick(&snapshot, &settings, DUE - 1).is_empty());
        assert_eq!(notifier.tick(&snapshot, &settings, DUE).len(), 1);
        // Once per episode.
        assert!(notifier.tick(&snapshot, &settings, DUE + 60_000).is_empty());
        assert!(notifier.select(&[], &snapshot, &settings, DUE + 70_000).is_empty());
    }

    #[test]
    fn answered_before_the_delay_is_never_announced() {
        let mut notifier = primed();
        let settings = Settings::default();
        let wait = transition("a", Some(SessionState::Working), SessionState::Waiting);
        notifier.select(std::slice::from_ref(&wait), &listing(vec![view_of(&wait)]), &settings, NOW);
        let answered = transition("a", Some(SessionState::Waiting), SessionState::Working);
        let working = listing(vec![view_of(&answered)]);
        assert!(notifier.select(&[answered], &working, &settings, NOW + 5_000).is_empty());
        assert!(notifier.tick(&working, &settings, DUE).is_empty());
        assert!(notifier.deferred.is_empty());
    }

    #[test]
    fn only_the_same_episode_is_announced() {
        let mut notifier = primed();
        let settings = Settings::default();
        let wait = transition("a", Some(SessionState::Working), SessionState::Waiting);
        notifier.select(std::slice::from_ref(&wait), &listing(vec![view_of(&wait)]), &settings, NOW);
        // Answered and waiting again, the transitions missed: a new episode.
        let other = since(wait.clone(), Some(NOW + 60_000));
        assert!(notifier.tick(&listing(vec![view_of(&other)]), &settings, DUE).is_empty());
        // The session gone: nothing either.
        let mut notifier = primed();
        notifier.select(std::slice::from_ref(&wait), &listing(vec![view_of(&wait)]), &settings, NOW);
        assert!(notifier.tick(&listing(vec![]), &settings, DUE).is_empty());
        // A new wait replaces the held one, and counts from its own start.
        let mut notifier = primed();
        notifier.select(std::slice::from_ref(&wait), &listing(vec![view_of(&wait)]), &settings, NOW);
        let next = since(wait, Some(NOW + 60_000));
        let shown = listing(vec![view_of(&next)]);
        notifier.select(std::slice::from_ref(&next), &shown, &settings, NOW + 60_000);
        assert!(notifier.tick(&shown, &settings, DUE).is_empty());
        assert_eq!(
            notifier.tick(&shown, &settings, NOW + 60_000 + REMINDER_DELAY_MS).len(),
            1
        );
    }

    #[test]
    fn probable_questions_are_never_announced() {
        let mut notifier = primed();
        let mut wait = transition("a", Some(SessionState::Working), SessionState::Waiting);
        wait.waiting_for = Some("probable question".into());
        assert!(announce(&mut notifier, &[wait.clone()], &all_on()).is_empty());
        assert!(notifier.deferred.is_empty());
        // A wait that turns into a probable question is not announced either.
        let mut notifier = primed();
        wait.waiting_for = Some("decision".into());
        let mut snapshot = listing(vec![view_of(&wait)]);
        notifier.select(&[wait], &snapshot, &all_on(), NOW);
        snapshot.sessions[0].waiting_for = Some("probable question".into());
        assert!(notifier.tick(&snapshot, &all_on(), DUE + 60_000).is_empty());
    }

    #[test]
    fn never_while_on_screen() {
        let mut notifier = primed();
        let all = [
            on_screen(transition("a", Some(SessionState::Working), SessionState::Waiting)),
            on_screen(transition("b", Some(SessionState::Working), SessionState::Unseen)),
            on_screen(transition("c", Some(SessionState::Working), SessionState::Error)),
        ];
        let shown = listing(all.iter().map(view_of).collect());
        assert!(notifier.select(&all, &shown, &all_on(), NOW).is_empty());
        assert!(notifier.tick(&shown, &all_on(), DUE + 30 * 60_000).is_empty());
        // Leaving the screen still unanswered: announced at once, the delay
        // being over. A finished turn seen on screen was seen.
        let mut gone = shown.clone();
        for s in &mut gone.sessions {
            s.on_screen = false;
        }
        let toasts = notifier.select(&[], &gone, &all_on(), DUE + 31 * 60_000);
        assert_eq!(toasts.len(), 1);
        assert!(toasts[0].is_summary());
        assert_eq!(toasts[0].body, "Session a · Session c");
        assert!(notifier.tick(&gone, &all_on(), DUE + 40 * 60_000).is_empty());
    }

    #[test]
    fn leaving_the_screen_early_still_waits_for_the_delay() {
        let mut notifier = primed();
        let wait = on_screen(transition("a", Some(SessionState::Working), SessionState::Waiting));
        notifier.select(std::slice::from_ref(&wait), &listing(vec![view_of(&wait)]), &all_on(), NOW);
        let mut off = listing(vec![view_of(&wait)]);
        off.sessions[0].on_screen = false;
        assert!(notifier.select(&[], &off, &all_on(), NOW + 20_000).is_empty());
        assert!(notifier.tick(&off, &all_on(), DUE - 1).is_empty());
        assert_eq!(notifier.tick(&off, &all_on(), DUE).len(), 1);
    }

    #[test]
    fn several_due_together_make_one_toast() {
        let mut notifier = primed();
        let all = [
            transition("a", Some(SessionState::Working), SessionState::Waiting),
            transition("b", Some(SessionState::Working), SessionState::Waiting),
            transition("c", Some(SessionState::Working), SessionState::Error),
        ];
        let toasts = announce(&mut notifier, &all, &all_on());
        assert_eq!(
            toasts,
            vec![Toast {
                key: SUMMARY_KEY.into(),
                title: "3 sessions t'attendent toujours".into(),
                body: "Session a · Session b · Session c".into(),
            }]
        );
        assert!(toasts[0].is_summary());

        // Long lists are cut.
        let mut notifier = primed();
        let many: Vec<Transition> = (0..40)
            .map(|i| {
                let mut t = transition(&format!("k{i}"), Some(SessionState::Working), SessionState::Waiting);
                t.title = format!("Une session au titre assez long numéro {i}");
                t
            })
            .collect();
        let toasts = announce(&mut notifier, &many, &all_on());
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].title, "40 sessions t'attendent toujours");
        assert_eq!(toasts[0].body.chars().count(), BODY_MAX_CHARS);
        assert!(toasts[0].body.ends_with('…'));
    }

    #[test]
    fn due_at_different_times_stay_per_session() {
        let mut notifier = primed();
        let a = transition("a", Some(SessionState::Working), SessionState::Waiting);
        let b = since(
            transition("b", Some(SessionState::Working), SessionState::Waiting),
            Some(NOW + 60_000),
        );
        let snapshot = listing(vec![view_of(&a), view_of(&b)]);
        notifier.select(&[a, b], &snapshot, &all_on(), NOW + 60_000);
        let first = notifier.tick(&snapshot, &all_on(), DUE);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].key, "a");
        let second = notifier.tick(&snapshot, &all_on(), NOW + 60_000 + REMINDER_DELAY_MS);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].key, "b");
    }

    #[test]
    fn respects_settings() {
        let quiet = Settings {
            notify_on_waiting: false,
            notify_on_finished: false,
            notify_on_error: false,
            ..Settings::default()
        };
        let all = [
            transition("a", Some(SessionState::Working), SessionState::Waiting),
            transition("b", Some(SessionState::Working), SessionState::Unseen),
            transition("c", Some(SessionState::Working), SessionState::Error),
        ];
        assert!(announce(&mut primed(), &all, &quiet).is_empty());

        let errors_only = Settings {
            notify_on_error: true,
            ..quiet.clone()
        };
        let toasts = announce(&mut primed(), &all, &errors_only);
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].title, "Session c : erreur");

        // Defaults: no finished toast, wait and error coalesced.
        let toasts = announce(&mut primed(), &all, &Settings::default());
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].title, "2 sessions t'attendent toujours");

        // Finished turns, when asked for, right away and on their own.
        let mut notifier = primed();
        let snapshot = listing(all.iter().map(view_of).collect());
        let at_once = notifier.select(&all, &snapshot, &all_on(), NOW);
        assert_eq!(at_once.len(), 1);
        assert_eq!(at_once[0].title, "Session b a terminé");

        // Turned off while held back: dropped.
        let mut notifier = primed();
        notifier.select(&all, &snapshot, &Settings::default(), NOW);
        assert!(notifier.tick(&snapshot, &quiet, DUE).is_empty());
        assert!(notifier.tick(&snapshot, &Settings::default(), DUE + 1_000).is_empty());
    }

    #[test]
    fn unready_snapshot_decides_nothing() {
        let mut notifier = primed();
        let wait = transition("b", Some(SessionState::Working), SessionState::Waiting);
        notifier.select(&[wait], &Snapshot::default(), &all_on(), NOW);
        assert!(notifier.tick(&Snapshot::default(), &all_on(), DUE).is_empty());
        assert_eq!(notifier.deferred.len(), 1);
    }

    #[test]
    fn old_waits_stay_silent_when_they_flap() {
        let mut notifier = primed();
        let old = NOW - 2 * 60 * 60_000;
        // Listed again with a 2 h old wait: silent…
        let back = since(transition("a", None, SessionState::Waiting), Some(old));
        assert!(announce(&mut notifier, std::slice::from_ref(&back), &all_on()).is_empty());
        // …and so is the same wait coming back after a flap.
        let away = transition("a", Some(SessionState::Waiting), SessionState::Unseen);
        assert!(notifier.select(&[away], &empty(), &all_on(), NOW + 1_000).is_empty());
        let mut flap = back;
        flap.from = Some(SessionState::Unseen);
        let snapshot = listing(vec![view_of(&flap)]);
        notifier.select(&[flap], &snapshot, &all_on(), NOW + 2_000);
        assert!(notifier.tick(&snapshot, &all_on(), DUE).is_empty());
        // A fresh wait from None is announced.
        let fresh = since(transition("b", None, SessionState::Waiting), Some(NOW - 3_000));
        assert_eq!(announce(&mut notifier, &[fresh], &all_on()).len(), 1);
        // Waits already there in the first snapshot are known too.
        let mut first = Notifier::default();
        let there = since(transition("c", None, SessionState::Waiting), Some(old));
        let snapshot = listing(vec![view_of(&there)]);
        first.select(&[], &snapshot, &all_on(), NOW);
        let mut flap = there;
        flap.from = Some(SessionState::Idle);
        first.select(&[flap], &snapshot, &all_on(), NOW + 1_000);
        assert!(first.tick(&snapshot, &all_on(), DUE).is_empty());
    }

    #[test]
    fn reappearing_sessions_are_not_new() {
        let mut notifier = primed();
        // Sessions listed again with an old state: silent, whatever the state.
        let old = NOW - 10 * 60_000;
        let back = [
            since(transition("a", None, SessionState::Waiting), Some(old)),
            since(transition("b", None, SessionState::Error), Some(old)),
            since(transition("c", None, SessionState::Unseen), Some(old)),
            since(transition("d", None, SessionState::Waiting), None),
            transition("e", None, SessionState::Error),
            transition("f", None, SessionState::Unseen),
        ];
        assert!(announce(&mut notifier, &back, &all_on()).is_empty());

        // A new session that starts by asking something is announced.
        let fresh = since(transition("g", None, SessionState::Waiting), Some(NOW - 3_000));
        let toasts = announce(&mut notifier, &[fresh], &all_on());
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].key, "g");
    }

    #[test]
    fn one_toast_per_episode() {
        let mut notifier = primed();
        let settings = Settings::default();
        let wait = transition("a", Some(SessionState::Working), SessionState::Waiting);
        assert_eq!(announce(&mut notifier, std::slice::from_ref(&wait), &settings).len(), 1);

        // The same wait (same start) seen again: from a flap, or after the
        // session dropped out of the list and came back.
        let mut flap = wait.clone();
        flap.from = Some(SessionState::Idle);
        assert!(announce(&mut notifier, &[flap], &settings).is_empty());
        let mut back = wait.clone();
        back.from = None;
        assert!(announce(&mut notifier, &[back], &settings).is_empty());

        // A new wait, after the user answered: announced in its turn.
        let next = since(wait, Some(NOW + 19_000));
        let snapshot = listing(vec![view_of(&next)]);
        notifier.select(std::slice::from_ref(&next), &snapshot, &settings, NOW + 20_000);
        let toasts = notifier.tick(&snapshot, &settings, NOW + 19_000 + REMINDER_DELAY_MS);
        assert_eq!(toasts.len(), 1);
    }

    #[test]
    fn without_a_start_time_the_delay_counts_from_first_sight() {
        let mut notifier = primed();
        let wait = since(
            transition("a", Some(SessionState::Working), SessionState::Waiting),
            None,
        );
        let settings = Settings::default();
        let snapshot = listing(vec![view_of(&wait)]);
        notifier.select(std::slice::from_ref(&wait), &snapshot, &settings, NOW);
        assert!(notifier.tick(&snapshot, &settings, NOW + REMINDER_DELAY_MS - 1).is_empty());
        assert_eq!(notifier.tick(&snapshot, &settings, NOW + REMINDER_DELAY_MS).len(), 1);
        // Each change counts as a new episode.
        let later = NOW + REMINDER_DELAY_MS + 1_000;
        notifier.select(&[wait], &snapshot, &settings, later);
        assert_eq!(notifier.tick(&snapshot, &settings, later + REMINDER_DELAY_MS).len(), 1);
    }

    #[test]
    fn finished_needs_a_turn_that_just_ended() {
        let mut notifier = primed();
        let settings = all_on();
        let toasts = notifier.select(
            &[
                transition("a", Some(SessionState::Idle), SessionState::Unseen),
                transition("b", None, SessionState::Unseen),
                transition("c", Some(SessionState::Working), SessionState::Unseen),
            ],
            &empty(),
            &settings,
            NOW,
        );
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].title, "Session c a terminé");
    }

    #[test]
    fn finished_through_idle_still_toasts() {
        // The registry goes idle a few seconds before Desktop records the end
        // of the turn: Working -> Idle -> Unseen.
        let mut notifier = primed();
        let settings = all_on();
        let to_idle = transition("a", Some(SessionState::Working), SessionState::Idle);
        assert!(notifier.select(&[to_idle], &empty(), &settings, NOW).is_empty());
        let to_unseen = since(
            transition("a", Some(SessionState::Idle), SessionState::Unseen),
            Some(NOW + 3_000),
        );
        let toasts = notifier.select(std::slice::from_ref(&to_unseen), &empty(), &settings, NOW + 3_000);
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].title, "Session a a terminé");

        // Too long after the turn: not a finish any more.
        let mut notifier = primed();
        let to_idle = transition("b", Some(SessionState::Working), SessionState::Idle);
        notifier.select(&[to_idle], &empty(), &settings, NOW);
        let mut late = to_unseen;
        late.key = "b".into();
        let later = NOW + FINISH_WINDOW_MS + 1_000;
        assert!(notifier.select(&[late], &empty(), &settings, later).is_empty());
    }

    #[test]
    fn errors_need_a_known_previous_state() {
        let mut notifier = primed();
        let settings = Settings::default();
        assert!(settings.notify_on_error, "on by default");
        assert!(!settings.notify_on_finished, "off by default");
        let toasts = announce(
            &mut notifier,
            &[
                transition("a", None, SessionState::Error),
                transition("b", Some(SessionState::Working), SessionState::Error),
            ],
            &settings,
        );
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].title, "Session b : erreur");
    }

    #[test]
    fn memory_is_bounded() {
        let mut notifier = primed();
        let settings = all_on();
        for i in 0..(EPISODE_MEMORY_MAX as i64 + 50) {
            let t = since(
                transition("a", Some(SessionState::Working), SessionState::Unseen),
                Some(i),
            );
            notifier.select(&[t], &empty(), &settings, NOW + i);
        }
        notifier.forget_old(NOW + EPISODE_MEMORY_MAX as i64 + 50);
        assert!(notifier.announced.len() <= EPISODE_MEMORY_MAX);
        notifier.forget_old(NOW + EPISODE_MEMORY_MS + 10_000);
        assert!(notifier.announced.is_empty());
    }

    #[test]
    fn reasons_are_french_with_raw_fallback() {
        assert_eq!(waiting_reason(Some("input needed")), "Une question attend ta réponse");
        assert_eq!(waiting_reason(Some("sandbox request")), "Accès sandbox demandé");
        assert_eq!(waiting_reason(Some("worker request")), "Un worker attend une réponse");
        assert_eq!(waiting_reason(Some("dialog open")), "Une boîte de dialogue est ouverte");
        assert_eq!(waiting_reason(Some("decision")), "Une décision est attendue");
        assert_eq!(
            waiting_reason(Some("probable question")),
            "Question dans le dernier message"
        );
        assert_eq!(waiting_reason(Some("offer")), "Un choix de session est attendu");
        assert_eq!(waiting_reason(Some("plan approval")), "Un plan attend ton approbation");
        assert_eq!(waiting_reason(Some("new reason")), "new reason");
        assert_eq!(waiting_reason(None), "Action requise");
    }

    #[test]
    fn empty_title_gets_a_name() {
        let mut t = transition("a", Some(SessionState::Working), SessionState::Error);
        t.title = "  ".into();
        assert_eq!(toast(Kind::Error, &t, None).title, "Une session : erreur");
    }

    #[test]
    fn long_questions_are_cut() {
        let text = "é".repeat(BODY_MAX_CHARS + 20);
        let cut = one_line(&text, BODY_MAX_CHARS);
        assert_eq!(cut.chars().count(), BODY_MAX_CHARS);
        assert!(cut.ends_with('…'));
        assert_eq!(one_line("  a \t b\r\nc ", 10), "a b c");
    }

    #[test]
    fn toast_ids_are_stable_and_positive() {
        assert_eq!(toast_id("local_x"), toast_id("local_x"));
        assert_ne!(toast_id("local_x"), toast_id("local_y"));
        assert!(toast_id("local_x") >= 0);
        assert_eq!(toast_tag("local_x").len(), 8);
    }

    #[test]
    fn toast_xml_opens_desktop_sessions_by_protocol() {
        let toast = Toast {
            key: "local_abc".into(),
            title: "Tom & \"Jerry\" <t'attend>".into(),
            body: "Ok ?\u{1}".into(),
        };
        assert_eq!(
            toast_xml(&toast, Some("claude://code/continue?session=local_abc")),
            concat!(
                r#"<toast launch="claude://code/continue?session=local_abc" activationType="protocol">"#,
                r#"<visual><binding template="ToastGeneric">"#,
                r#"<text>Tom &amp; &quot;Jerry&quot; &lt;t&apos;attend&gt;</text><text>Ok ? </text>"#,
                r#"</binding></visual><audio silent="true"/></toast>"#
            )
        );
        assert!(toast_xml(&toast, None).starts_with("<toast><visual>"));
    }
}
