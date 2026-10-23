//! Plan usage: the quota windows Claude Desktop records in
//! `%APPDATA%\Claude\plan-usage-history.json`.
//!
//! While it runs, Desktop asks claude.ai for the plan's usage every 5 to 30
//! minutes and appends the percents it got: one sample per organization at
//! most every 4.5 minutes, kept 30 days. Undocumented format:
//!
//! - version 2: `{"version":2,"samples":[{"t":<ms>,"org":"<uuid>","u":{"fh":19,"sd":26}}]}`,
//!   `fh` the 5-hour window, `sd` the 7-day one (other keys: per-model weekly
//!   windows, extra usage);
//! - version 1: `{"version":1,"samples":[{"t":<ms>,"fh":19,"sd":26}]}`.
//!
//! Samples carry no reset time, so resets are placed from the history: a
//! window's percent only grows until it resets. The 5-hour window starts
//! with the first use after the previous one ended. The 7-day window resets
//! on a fixed weekly schedule, sometimes also out of schedule.

use serde_json::Value;

use crate::model::{Quota, QuotaWindow};

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;
const FIVE_HOURS: i64 = 5 * HOUR;
const WEEK: i64 = 7 * 24 * HOUR;
/// A 5-hour reset is given only when the history places it this closely.
const FIVE_HOUR_PRECISION: i64 = 30 * MIN;
/// Same for the weekly reset.
const WEEK_PRECISION: i64 = 6 * HOUR;

/// A percent recorded at a time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub t: i64,
    pub percent: f64,
}

/// The samples of one organization, oldest first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct History {
    pub five_hour: Vec<Point>,
    pub seven_day: Vec<Point>,
}

/// The samples of the organization of the latest sample. None when the text
/// is not a history.
pub fn parse_history(json: &str) -> Option<History> {
    let root: Value = serde_json::from_str(json.strip_prefix('\u{feff}').unwrap_or(json)).ok()?;
    let samples = root.get("samples")?.as_array()?;
    // (time, organization, usage) of each sample with a time.
    let parsed: Vec<(i64, Option<&str>, &Value)> = samples
        .iter()
        .filter_map(|sample| {
            let t = sample.get("t")?.as_f64().filter(|t| t.is_finite())? as i64;
            let org = sample.get("org").and_then(Value::as_str);
            // Version 1 keeps the percents on the sample itself.
            let usage = sample.get("u").filter(|u| u.is_object()).unwrap_or(sample);
            Some((t, org, usage))
        })
        .collect();
    let latest_org = parsed.iter().max_by_key(|(t, _, _)| *t).map(|(_, org, _)| *org);
    let mut mine: Vec<&(i64, Option<&str>, &Value)> = parsed
        .iter()
        .filter(|(_, org, _)| Some(*org) == latest_org)
        .collect();
    mine.sort_by_key(|(t, _, _)| *t);
    let series = |key: &str| -> Vec<Point> {
        mine.iter()
            .filter_map(|(t, _, usage)| {
                let percent = usage.get(key)?.as_f64().filter(|p| p.is_finite() && *p >= 0.0)?;
                Some(Point { t: *t, percent })
            })
            .collect()
    };
    Some(History {
        five_hour: series("fh"),
        seven_day: series("sd"),
    })
}

/// The plan's usage as of the latest sample, None without any sample.
pub fn quota(history: &History, now_ms: i64) -> Option<Quota> {
    let five_hour = rolling_window(&history.five_hour, now_ms);
    let seven_day = weekly_window(&history.seven_day, now_ms);
    let sampled_at = history
        .five_hour
        .last()
        .map(|p| p.t)
        .max(history.seven_day.last().map(|p| p.t))?;
    Some(Quota {
        sampled_at,
        five_hour,
        seven_day,
    })
}

/// The 5-hour window: it starts with the first use after the previous one
/// ended, so its reset is 5 hours after that start. Without a reset placed
/// closely enough, the window still began at the latest at the first point
/// of its run, so it ends by 5 hours after it (unless the run outlasts one
/// window: two windows met without a visible reset).
fn rolling_window(points: &[Point], now_ms: i64) -> Option<QuotaWindow> {
    let last = *points.last()?;
    let range = if last.percent > 0.0 {
        rolling_start(points, FIVE_HOURS)
            .map(|(lo, hi)| (lo + FIVE_HOURS, hi + FIVE_HOURS))
            .filter(|(lo, hi)| hi - lo <= FIVE_HOUR_PRECISION)
    } else {
        None
    };
    let run_start = points[run_first(points)].t;
    let ends_by = match range {
        Some((_, hi)) => hi,
        None if last.percent > 0.0 && last.t - run_start <= FIVE_HOURS => run_start + FIVE_HOURS,
        None => last.t + FIVE_HOURS,
    };
    Some(window(last, FIVE_HOURS, range, ends_by, now_ms))
}

/// Index of the first point of the latest run of non-zero percents that
/// never decrease (the latest point's own index when it is 0 %).
fn run_first(points: &[Point]) -> usize {
    let mut first = points.len().saturating_sub(1);
    while first > 0 {
        let (before, at) = (points[first - 1], points[first]);
        if before.percent <= 0.0 || before.percent > at.percent {
            break;
        }
        first -= 1;
    }
    first
}

/// Range (start excluded, end included) in which the window holding the
/// latest point started: after the last point outside it, at the latest at
/// the first point inside it. The points inside are the latest run of
/// non-zero percents that never decrease, within one window length. None
/// when that run starts the history: the start may be older.
fn rolling_start(points: &[Point], length: i64) -> Option<(i64, i64)> {
    let last = points.last()?;
    let first = run_first(points);
    if first == 0 {
        return None;
    }
    let lo = points[first - 1].t.max(last.t - length);
    let hi = points[first].t;
    (lo < hi).then_some((lo, hi))
}

/// The 7-day window: resets on a weekly schedule placed from the drops of
/// the history.
fn weekly_window(points: &[Point], now_ms: i64) -> Option<QuotaWindow> {
    let last = *points.last()?;
    let range = weekly_reset(points)
        .filter(|(lo, hi)| hi - lo <= WEEK_PRECISION)
        .map(|(lo, hi)| {
            // The scheduled reset that started the window of the latest
            // point, then the next one.
            let mid = lo + (hi - lo) / 2;
            let weeks = (last.t - mid).div_euclid(WEEK) + 1;
            (lo + weeks * WEEK, hi + weeks * WEEK)
        });
    let ends_by = range.map_or(last.t + WEEK, |(_, hi)| hi);
    Some(window(last, WEEK, range, ends_by, now_ms))
}

/// Range of one scheduled weekly reset, from the drops of the history (a
/// percent lower than the one before: a reset happened in between). Each
/// drop, shifted by whole weeks, narrows the ranges it overlaps; the range
/// most drops agree with wins (the latest on a tie), so an out-of-schedule
/// reset is ignored.
fn weekly_reset(points: &[Point]) -> Option<(i64, i64)> {
    let drops: Vec<(i64, i64)> = points
        .windows(2)
        .filter(|pair| pair[1].percent < pair[0].percent)
        .map(|pair| (pair[0].t, pair[1].t))
        .filter(|(lo, hi)| hi - lo < WEEK)
        .collect();
    let mut best: Option<(usize, (i64, i64))> = None;
    for &anchor in &drops {
        let center = anchor.0 + (anchor.1 - anchor.0) / 2;
        let mut range = anchor;
        let mut support = 0;
        for &(lo, hi) in &drops {
            let shift = ((center - (lo + (hi - lo) / 2)) as f64 / WEEK as f64).round() as i64 * WEEK;
            let narrowed = (range.0.max(lo + shift), range.1.min(hi + shift));
            if narrowed.0 < narrowed.1 {
                range = narrowed;
                support += 1;
            }
        }
        if best.is_none_or(|(most, _)| support >= most) {
            best = Some((support, range));
        }
    }
    best.map(|(_, range)| range)
}

/// A window from its latest point, the range its reset falls in, and the
/// latest it can end.
fn window(
    last: Point,
    length: i64,
    reset: Option<(i64, i64)>,
    ends_by: i64,
    now_ms: i64,
) -> QuotaWindow {
    let (resets_at, reset_margin_ms) = match reset {
        Some((lo, hi)) => (Some(lo + (hi - lo) / 2), (hi - lo) / 2),
        None => (None, 0),
    };
    QuotaWindow {
        percent: last.percent,
        length_ms: length,
        resets_at,
        reset_margin_ms,
        ends_by,
        expired: now_ms > ends_by,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thursday 2026-10-22 05:00 UTC.
    const T0: i64 = 1_792_645_200_000;

    fn p(t: i64, percent: f64) -> Point {
        Point { t, percent }
    }

    /// One point every 15 minutes from `start`, one per percent.
    fn every_15_min(start: i64, percents: &[f64]) -> Vec<Point> {
        percents
            .iter()
            .enumerate()
            .map(|(i, &percent)| p(start + i as i64 * 15 * MIN, percent))
            .collect()
    }

    #[test]
    fn version_2_keeps_the_latest_organization() {
        let json = r#"{"version":2,"samples":[
            {"t":3000,"org":"a","u":{"fh":30,"sd":12}},
            {"t":1000,"org":"a","u":{"fh":10,"sd":10}},
            {"t":2000,"org":"b","u":{"fh":99,"sd":99}},
            {"t":4000,"org":"a","u":{"sd":13}},
            {"t":"x","org":"a","u":{"fh":1}},
            {"t":5000,"org":"a","u":{"fh":-3,"sd":"14"}},
            {"org":"a","u":{"fh":50}}
        ]}"#;
        let history = parse_history(json).expect("history");
        assert_eq!(history.five_hour, [p(1000, 10.0), p(3000, 30.0)]);
        assert_eq!(history.seven_day, [p(1000, 10.0), p(3000, 12.0), p(4000, 13.0)]);
    }

    #[test]
    fn version_1_and_a_bom_are_read() {
        let json = "\u{feff}{\"version\":1,\"samples\":[{\"t\":10,\"fh\":4,\"sd\":null},{\"t\":20,\"fh\":6,\"sd\":2}]}";
        let history = parse_history(json).expect("history");
        assert_eq!(history.five_hour, [p(10, 4.0), p(20, 6.0)]);
        assert_eq!(history.seven_day, [p(20, 2.0)]);
    }

    #[test]
    fn anything_else_is_no_history() {
        for json in ["", "[]", "{}", r#"{"samples":{}}"#, "pas du json"] {
            assert_eq!(parse_history(json), None, "{json}");
        }
        let empty = parse_history(r#"{"version":2,"samples":[]}"#).expect("history");
        assert_eq!(empty, History::default());
        assert_eq!(quota(&empty, T0), None);
    }

    #[test]
    fn five_hour_reset_comes_from_the_drop() {
        // 82 % then 1 %: the new window started between the two samples.
        let mut points = every_15_min(T0, &[60.0, 70.0, 82.0]);
        points.extend(every_15_min(T0 + 45 * MIN, &[1.0, 3.0, 7.0]));
        let now = T0 + 90 * MIN;
        let w = rolling_window(&points, now).expect("window");
        assert_eq!(w.percent, 7.0);
        assert_eq!(w.length_ms, FIVE_HOURS);
        let start = T0 + 30 * MIN + 15 * MIN / 2;
        assert_eq!(w.resets_at, Some(start + FIVE_HOURS));
        assert_eq!(w.reset_margin_ms, 15 * MIN / 2);
        assert!(!w.expired);
        // Past the latest possible reset: the percent is out of date.
        let after = rolling_window(&points, T0 + 45 * MIN + FIVE_HOURS + 1).expect("window");
        assert!(after.expired);
    }

    #[test]
    fn five_hour_window_starts_with_the_first_use_after_idle() {
        // At 0 % no window runs; it starts before the first non-zero sample.
        let points = every_15_min(T0, &[40.0, 0.0, 0.0, 5.0, 9.0]);
        let w = rolling_window(&points, T0 + HOUR).expect("window");
        assert_eq!(w.resets_at, Some(T0 + 30 * MIN + 15 * MIN / 2 + FIVE_HOURS));
        // Idle now: nothing to reset.
        let idle = rolling_window(&every_15_min(T0, &[40.0, 0.0]), T0 + 20 * MIN).expect("window");
        assert_eq!((idle.percent, idle.resets_at, idle.expired), (0.0, None, false));
    }

    #[test]
    fn five_hour_reset_is_left_out_when_unsure() {
        // A 3-hour gap around the drop: too wide.
        let points = vec![p(T0, 80.0), p(T0 + 3 * HOUR, 10.0), p(T0 + 3 * HOUR + 15 * MIN, 12.0)];
        let w = rolling_window(&points, T0 + 4 * HOUR).expect("window");
        assert_eq!(w.resets_at, None);
        assert!(!w.expired);
        // The run starts the history: its start may be older.
        let w = rolling_window(&every_15_min(T0, &[3.0, 8.0, 9.0]), T0 + HOUR).expect("window");
        assert_eq!(w.resets_at, None);
        // Without a reset time, a sample older than the window is out of date.
        let w = rolling_window(&every_15_min(T0, &[3.0]), T0 + FIVE_HOURS + 1).expect("window");
        assert!(w.expired);
    }

    #[test]
    fn an_unplaced_window_ends_five_hours_after_its_first_point() {
        // 0 % then hourly samples: the start is only known within an hour,
        // too wide for a reset time, but the window began by 09:00.
        let points = vec![
            p(T0, 0.0),
            p(T0 + HOUR, 20.0),
            p(T0 + 2 * HOUR, 45.0),
            p(T0 + 3 * HOUR, 70.0),
            p(T0 + 4 * HOUR, 84.0),
        ];
        let w = rolling_window(&points, T0 + 4 * HOUR).expect("window");
        assert_eq!(w.resets_at, None);
        assert_eq!(w.ends_by, T0 + HOUR + FIVE_HOURS);
        assert!(!w.expired);
        // Over at 14:00, not 5 hours after the 84 % sample.
        assert!(rolling_window(&points, T0 + HOUR + FIVE_HOURS + 1).expect("window").expired);
        // A placed reset ends the window at the latest end of its range.
        let mut placed = every_15_min(T0, &[60.0, 70.0, 82.0]);
        placed.extend(every_15_min(T0 + 45 * MIN, &[1.0, 3.0]));
        let w = rolling_window(&placed, T0 + HOUR).expect("window");
        assert_eq!(w.ends_by, T0 + 45 * MIN + FIVE_HOURS);
        // The weekly window without a schedule: a week after its latest point.
        let w = weekly_window(&[p(T0, 5.0)], T0).expect("window");
        assert_eq!(w.ends_by, T0 + WEEK);
    }

    #[test]
    fn a_run_longer_than_the_window_is_not_one_window() {
        // No drop for 6 hours: two windows met without a visible reset.
        let percents: Vec<f64> = (0..25).map(|i| f64::from(i) + 1.0).collect();
        let mut points = vec![p(T0 - 15 * MIN, 0.0)];
        points.extend(every_15_min(T0, &percents));
        let w = rolling_window(&points, T0 + 6 * HOUR).expect("window");
        assert_eq!(w.resets_at, None);
        // The latest window began after the run did: 5 hours after the latest point.
        assert_eq!(w.ends_by, T0 + 6 * HOUR + FIVE_HOURS);
    }

    /// A weekly series dropping to `after` between `lo` and `hi`.
    fn drop(points: &mut Vec<Point>, lo: i64, hi: i64, before: f64, after: f64) {
        points.push(p(lo, before));
        points.push(p(hi, after));
    }

    #[test]
    fn weekly_schedule_comes_from_the_drops_that_agree() {
        let mut points = Vec::new();
        // Scheduled resets, Thursdays between 02:49 and 08:00 (UTC here),
        // seen through wide gaps that overlap there.
        drop(&mut points, T0 - 21 * 24 * HOUR - 2 * HOUR - 11 * MIN, T0 - 21 * 24 * HOUR + 3 * HOUR, 100.0, 0.0);
        drop(&mut points, T0 - 14 * 24 * HOUR - 11 * HOUR, T0 - 14 * 24 * HOUR + 7 * HOUR, 100.0, 0.0);
        // An out-of-schedule reset on a Saturday.
        drop(&mut points, T0 - 12 * 24 * HOUR + 12 * HOUR, T0 - 12 * 24 * HOUR + 12 * HOUR + 16 * MIN, 98.0, 3.0);
        drop(&mut points, T0 - 7 * 24 * HOUR - 4 * HOUR, T0 - 7 * 24 * HOUR + 9 * HOUR, 100.0, 0.0);
        drop(&mut points, T0 - 4 * HOUR, T0 + 9 * HOUR, 68.0, 0.0);
        points.push(p(T0 + 24 * HOUR, 26.0));
        let now = T0 + 25 * HOUR;
        let w = weekly_window(&points, now).expect("window");
        assert_eq!(w.percent, 26.0);
        assert_eq!(w.length_ms, WEEK);
        // Agreed range: 02:49 to 08:00, next Thursday.
        let lo = T0 - 2 * HOUR - 11 * MIN + WEEK;
        let hi = T0 + 3 * HOUR + WEEK;
        assert_eq!(w.resets_at, Some(lo + (hi - lo) / 2));
        assert_eq!(w.reset_margin_ms, (hi - lo) / 2);
        assert!(!w.expired);
        assert!(weekly_window(&points, hi + 1).expect("window").expired);
    }

    #[test]
    fn an_out_of_schedule_reset_does_not_move_the_schedule() {
        let mut points = Vec::new();
        drop(&mut points, T0 - 7 * 24 * HOUR - HOUR, T0 - 7 * 24 * HOUR + HOUR, 100.0, 0.0);
        drop(&mut points, T0 - HOUR, T0 + HOUR, 90.0, 0.0);
        // The latest drop is out of schedule, two days later.
        drop(&mut points, T0 + 48 * HOUR, T0 + 48 * HOUR + 15 * MIN, 100.0, 1.0);
        let w = weekly_window(&points, T0 + 49 * HOUR).expect("window");
        assert_eq!(w.resets_at, Some(T0 + WEEK));
    }

    #[test]
    fn weekly_reset_needs_a_narrow_enough_range() {
        let mut points = Vec::new();
        drop(&mut points, T0 - 10 * HOUR, T0 + 10 * HOUR, 100.0, 0.0);
        points.push(p(T0 + 24 * HOUR, 5.0));
        let w = weekly_window(&points, T0 + 25 * HOUR).expect("window");
        assert_eq!(w.resets_at, None);
        // No drop at all.
        let w = weekly_window(&every_15_min(T0, &[5.0, 6.0]), T0 + HOUR).expect("window");
        assert_eq!((w.resets_at, w.expired), (None, false));
        assert!(weekly_window(&[p(T0, 5.0)], T0 + WEEK + 1).expect("window").expired);
    }

    #[test]
    fn a_window_left_without_samples_moves_to_the_next_reset() {
        let mut points = Vec::new();
        drop(&mut points, T0 - HOUR, T0 + HOUR, 100.0, 0.0);
        // Ten days later, after a reset nobody saw.
        points.push(p(T0 + 10 * 24 * HOUR, 4.0));
        let w = weekly_window(&points, T0 + 10 * 24 * HOUR).expect("window");
        assert_eq!(w.resets_at, Some(T0 + 2 * WEEK));
    }

    #[test]
    fn quota_takes_the_latest_sample_time() {
        let history = History {
            five_hour: vec![p(T0, 10.0)],
            seven_day: vec![p(T0 - MIN, 20.0), p(T0 + MIN, 21.0)],
        };
        let q = quota(&history, T0 + 2 * MIN).expect("quota");
        assert_eq!(q.sampled_at, T0 + MIN);
        assert_eq!(q.five_hour.map(|w| w.percent), Some(10.0));
        assert_eq!(q.seven_day.map(|w| w.percent), Some(21.0));
    }
}
