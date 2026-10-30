//! What the `monitor-compagnon` Claude Code mod writes for the monitor, in
//! `<claude_home>\session-monitor`:
//!
//! - `quota.json`: `{"version":1,"sampledAt":<ms>,"fiveHour":{"percent":23.5,"resetsAt":<ms>},"sevenDay":{…}}`,
//!   the plan's usage as the latest API response of any session reported it,
//!   each window's `resetsAt` left out when the engine gave none;
//! - `sessions/<cli id>.json`: `{"version":1,"agents":[{"id":"…","description":"…","type":"Explore"}]}`,
//!   the sub-agents running in that session.
//!
//! The mod is optional: without these files, or with files of another
//! version, everything comes from the other sources as before.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::model::{Quota, QuotaWindow, RunningAgent};
use crate::transcript::is_valid_session_id;

/// The format this monitor reads.
const VERSION: u32 = 1;
const FIVE_HOURS: i64 = 5 * 3_600_000;
const WEEK: i64 = 7 * 24 * 3_600_000;

/// One usage window as the engine reports it.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExactWindow {
    pub percent: f64,
    pub resets_at: Option<i64>,
}

/// The contents of `quota.json`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSample {
    pub version: u32,
    pub sampled_at: i64,
    pub five_hour: Option<ExactWindow>,
    pub seven_day: Option<ExactWindow>,
}

/// The contents of `sessions/<cli id>.json`.
#[derive(Debug, Deserialize)]
struct AgentsFile {
    version: u32,
    #[serde(default)]
    agents: Vec<RunningAgent>,
}

/// None when the text is not a sample of the format this monitor reads.
pub fn parse_quota(text: &str) -> Option<QuotaSample> {
    serde_json::from_str::<QuotaSample>(text)
        .ok()
        .filter(|sample| sample.version == VERSION)
}

/// None when the text is not a list of the format this monitor reads.
fn parse_agents(text: &str) -> Option<Vec<RunningAgent>> {
    serde_json::from_str::<AgentsFile>(text)
        .ok()
        .filter(|file| file.version == VERSION)
        .map(|file| file.agents)
}

/// The plan's usage from Desktop's history and the mod's latest sample. A
/// window's reset comes from the mod as long as it has one: the history only
/// estimates it. Its percent comes from the latest sample of the two. A
/// sample a week old tells nothing about any open window, and is left out.
pub fn merged_quota(desktop: Option<Quota>, sample: Option<&QuotaSample>, now_ms: i64) -> Option<Quota> {
    let Some(sample) = sample.filter(|s| now_ms.saturating_sub(s.sampled_at) < WEEK) else {
        return desktop;
    };
    let (desktop_at, five_hour, seven_day) = match desktop {
        Some(q) => (Some(q.sampled_at), q.five_hour, q.seven_day),
        None => (None, None, None),
    };
    let desktop_is_newer = desktop_at.is_some_and(|at| at > sample.sampled_at);
    let merge = |desktop, exact, length_ms| Merge {
        desktop,
        exact,
        length_ms,
        sampled_at: sample.sampled_at,
        desktop_is_newer,
        now_ms,
    };
    Some(Quota {
        sampled_at: sample.sampled_at.max(desktop_at.unwrap_or(i64::MIN)),
        five_hour: merge(five_hour, sample.five_hour, FIVE_HOURS).window(),
        seven_day: merge(seven_day, sample.seven_day, WEEK).window(),
        fed_by_sessions: true,
    })
}

/// One window from both sources.
struct Merge {
    desktop: Option<QuotaWindow>,
    exact: Option<ExactWindow>,
    length_ms: i64,
    sampled_at: i64,
    desktop_is_newer: bool,
    now_ms: i64,
}

impl Merge {
    fn window(self) -> Option<QuotaWindow> {
        let Some(exact) = self.exact else {
            return self.desktop;
        };
        let percent = exact.percent.max(0.0);
        let Some(resets_at) = exact.resets_at else {
            return Some(match self.desktop {
                Some(desktop) if self.desktop_is_newer => desktop,
                Some(desktop) => QuotaWindow { percent, ..desktop },
                None => QuotaWindow {
                    percent,
                    length_ms: self.length_ms,
                    resets_at: None,
                    reset_margin_ms: 0,
                    ends_by: self.sampled_at.saturating_add(self.length_ms),
                    expired: false,
                },
            });
        };
        let open = resets_at > self.now_ms;
        let newer_percent = self.desktop.as_ref().filter(|_| self.desktop_is_newer);
        if !open && newer_percent.is_some() {
            // Over since the mod's sample: Desktop's is of the next window.
            return self.desktop;
        }
        Some(QuotaWindow {
            percent: newer_percent.map_or(percent, |desktop| desktop.percent),
            length_ms: self.length_ms,
            resets_at: Some(resets_at),
            reset_margin_ms: 0,
            ends_by: resets_at,
            expired: !open,
        })
    }
}

/// A file read once, kept until it changes.
struct Stamped<T> {
    stamp: (SystemTime, u64),
    value: T,
}

/// Reads the mod's files, again only when they change, and keeps the last
/// good content while one is being rewritten.
pub struct CompanionReader {
    dir: PathBuf,
    quota: Option<Stamped<QuotaSample>>,
    agents: HashMap<String, Stamped<Vec<RunningAgent>>>,
}

impl CompanionReader {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            quota: None,
            agents: HashMap::new(),
        }
    }

    /// The latest sample, None without a readable file.
    pub fn quota(&mut self) -> Option<&QuotaSample> {
        let path = self.dir.join("quota.json");
        refresh(&mut self.quota, &path, parse_quota);
        self.quota.as_ref().map(|q| &q.value)
    }

    /// The sub-agents of `cli_ids`, by CLI session id, for the sessions that
    /// have some.
    pub fn agents(&mut self, cli_ids: &HashSet<String>) -> HashMap<String, Vec<RunningAgent>> {
        self.agents.retain(|id, _| cli_ids.contains(id));
        let mut running = HashMap::new();
        for id in cli_ids.iter().filter(|id| is_valid_session_id(id)) {
            let path = self.dir.join("sessions").join(format!("{id}.json"));
            let mut entry = self.agents.remove(id);
            refresh(&mut entry, &path, parse_agents);
            if let Some(entry) = entry {
                if !entry.value.is_empty() {
                    running.insert(id.clone(), entry.value.clone());
                }
                self.agents.insert(id.clone(), entry);
            }
        }
        running
    }
}

/// Reads `path` again if it changed. A file gone drops the value; one that
/// does not parse (being written) keeps it, and is read again next time.
fn refresh<T>(slot: &mut Option<Stamped<T>>, path: &Path, parse: impl Fn(&str) -> Option<T>) {
    let Some(stamp) = file_stamp(path) else {
        *slot = None;
        return;
    };
    if slot.as_ref().is_some_and(|s| s.stamp == stamp) {
        return;
    }
    let bytes = std::fs::read(path).unwrap_or_default();
    if let Some(value) = parse(&String::from_utf8_lossy(&bytes)) {
        *slot = Some(Stamped { stamp, value });
    }
}

fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const MIN: i64 = 60_000;

    fn estimate(percent: f64, resets_at: Option<i64>) -> QuotaWindow {
        QuotaWindow {
            percent,
            length_ms: FIVE_HOURS,
            resets_at,
            reset_margin_ms: 7 * MIN,
            ends_by: NOW + 80 * MIN,
            expired: false,
        }
    }

    fn desktop(sampled_at: i64, percent: f64) -> Option<Quota> {
        Some(Quota {
            sampled_at,
            five_hour: Some(estimate(percent, Some(NOW + 73 * MIN))),
            seven_day: None,
            fed_by_sessions: false,
        })
    }

    fn sample(sampled_at: i64, percent: f64, resets_at: Option<i64>) -> QuotaSample {
        QuotaSample {
            version: VERSION,
            sampled_at,
            five_hour: Some(ExactWindow { percent, resets_at }),
            seven_day: None,
        }
    }

    #[test]
    fn the_quota_file_is_read_as_the_mod_writes_it() {
        let text = r#"{"version":1,"sampledAt":5,"fiveHour":{"percent":23.5,"resetsAt":9},"sevenDay":{"percent":11}}"#;
        let sample = parse_quota(text).expect("sample");
        assert_eq!(sample.sampled_at, 5);
        assert_eq!(sample.five_hour.map(|w| (w.percent, w.resets_at)), Some((23.5, Some(9))));
        assert_eq!(sample.seven_day.map(|w| (w.percent, w.resets_at)), Some((11.0, None)));
    }

    #[test]
    fn files_of_another_version_or_half_written_are_refused() {
        assert_eq!(parse_quota(r#"{"version":2,"sampledAt":5}"#), None);
        assert_eq!(parse_quota(r#"{"version":1,"sampledAt":"#), None);
        assert_eq!(parse_agents(r#"{"version":2,"agents":[]}"#), None);
        let agents = parse_agents(r#"{"version":1,"agents":[{"id":"a","description":"Audit","type":"Explore"}]}"#);
        assert_eq!(agents.expect("agents")[0].agent_type, "Explore");
    }

    #[test]
    fn without_a_sample_nothing_changes() {
        assert_eq!(merged_quota(desktop(NOW, 40.0), None, NOW), desktop(NOW, 40.0));
        assert_eq!(merged_quota(None, None, NOW), None);
        let old = sample(NOW - WEEK, 40.0, Some(NOW + MIN));
        assert_eq!(merged_quota(desktop(NOW, 40.0), Some(&old), NOW), desktop(NOW, 40.0));
    }

    #[test]
    fn the_exact_reset_replaces_the_estimate() {
        let merged = merged_quota(desktop(NOW - 10 * MIN, 40.0), Some(&sample(NOW - MIN, 42.0, Some(NOW + HOUR_5))), NOW)
            .expect("quota");
        assert_eq!(merged.sampled_at, NOW - MIN);
        assert!(merged.fed_by_sessions);
        let window = merged.five_hour.expect("5 h");
        assert_eq!(window.percent, 42.0);
        assert_eq!((window.resets_at, window.reset_margin_ms), (Some(NOW + HOUR_5), 0));
        assert_eq!(window.ends_by, NOW + HOUR_5);
        assert!(!window.expired);
    }

    #[test]
    fn a_newer_desktop_sample_gives_the_percent_and_the_mod_the_reset() {
        let merged = merged_quota(desktop(NOW - MIN, 45.0), Some(&sample(NOW - 10 * MIN, 42.0, Some(NOW + HOUR_5))), NOW)
            .expect("quota");
        assert_eq!(merged.sampled_at, NOW - MIN);
        let window = merged.five_hour.expect("5 h");
        assert_eq!((window.percent, window.resets_at), (45.0, Some(NOW + HOUR_5)));
    }

    #[test]
    fn a_window_over_expires_unless_desktop_saw_the_next_one() {
        let over = sample(NOW - 2 * HOUR_5, 80.0, Some(NOW - HOUR_5));
        let alone = merged_quota(None, Some(&over), NOW).expect("quota");
        let window = alone.five_hour.expect("5 h");
        assert!(window.expired);
        assert_eq!(window.percent, 80.0);

        let next = merged_quota(desktop(NOW - MIN, 3.0), Some(&over), NOW).expect("quota");
        assert_eq!(next.five_hour, Some(estimate(3.0, Some(NOW + 73 * MIN))));
    }

    #[test]
    fn a_percent_without_a_reset_keeps_the_estimate() {
        let merged = merged_quota(desktop(NOW - 10 * MIN, 40.0), Some(&sample(NOW - MIN, 42.0, None)), NOW).expect("quota");
        let window = merged.five_hour.expect("5 h");
        assert_eq!((window.percent, window.resets_at), (42.0, Some(NOW + 73 * MIN)));
        let alone = merged_quota(None, Some(&sample(NOW - MIN, 42.0, None)), NOW).expect("quota");
        let window = alone.five_hour.expect("5 h");
        assert_eq!((window.resets_at, window.ends_by), (None, NOW - MIN + FIVE_HOURS));
    }

    #[test]
    fn a_window_the_mod_lacks_stays_on_desktop() {
        let mut quota = desktop(NOW, 40.0).expect("quota");
        quota.seven_day = Some(estimate(9.0, None));
        let merged = merged_quota(Some(quota), Some(&sample(NOW, 42.0, Some(NOW + HOUR_5))), NOW).expect("quota");
        assert_eq!(merged.seven_day, Some(estimate(9.0, None)));
    }

    const HOUR_5: i64 = FIVE_HOURS;
}
