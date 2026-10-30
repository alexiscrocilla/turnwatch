//! Toasts when the plan's usage crosses a threshold: the 5-hour window at
//! 80 % then 95 %, the 7-day window at 90 %.
//!
//! Each threshold is announced once per window. It is armed while the window
//! is below it and fires when a sample reaches it. A window's usage only
//! grows until it resets, so it falls back below a threshold, and rearms it,
//! only by resetting. A new window does not always show a drop (Desktop
//! closed across the reset, the new window already as high), so the
//! thresholds also rearm when the window expired (its latest possible end
//! is past) or when a sample comes after the previous window's latest
//! possible end. Thresholds already reached when the monitor first sees a
//! window are not announced: the window shows them. When one sample crosses
//! both 5-hour thresholds, only the higher is announced. With the setting
//! off, the thresholds still follow the usage, so turning it on announces
//! nothing already reached.
//!
//! Another toast says when Desktop stopped sampling the quota while
//! sessions work: it only samples for a few hours after a right-click on
//! its tray icon, and the gauges then stop moving and the thresholds stop
//! firing. With the `monitor-compagnon` mod, sessions report the quota
//! themselves and that toast has no reason to be.

use monitor_core::model::{Quota, QuotaWindow, Settings, Snapshot};

use crate::toasts::{MONITOR_PREFIX, Toast};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    FiveHour = 0,
    SevenDay = 1,
}

/// The thresholds announced, in percent, lowest first within a window.
const THRESHOLDS: [(Window, u8); 3] = [
    (Window::FiveHour, 80),
    (Window::FiveHour, 95),
    (Window::SevenDay, 90),
];

/// While Desktop samples the quota, it does so every 15 minutes at most:
/// this long without a sample, the person at the computer, means it stopped.
const STOPPED_AFTER_MS: i64 = 20 * 60_000;
/// Desktop skips its samples while the computer is locked or idle this long.
const DESKTOP_IDLE_MS: i64 = 10 * 60_000;

#[derive(Debug, Default)]
pub struct QuotaAlerts {
    /// Per threshold, in [`THRESHOLDS`] order: true while it may fire; None
    /// until its window is first seen.
    armed: [Option<bool>; THRESHOLDS.len()],
    /// Per window: the latest possible end of the window last seen.
    ends_by: [Option<i64>; 2],
    /// Since when the person has been using the computer without a pause
    /// long enough for Desktop to skip its samples; None while idle.
    active_since: Option<i64>,
    /// The latest sample whose stop was announced (or passed over with the
    /// setting off).
    stop_announced: Option<i64>,
}

impl QuotaAlerts {
    /// The toasts for the thresholds `quota` crossed since the previous
    /// call: at most one per window, the highest crossed.
    pub fn check(&mut self, quota: Option<&Quota>, settings: &Settings, now_ms: i64) -> Vec<Toast> {
        let Some(quota) = quota else {
            return Vec::new();
        };
        // A sample past the previous window's latest end is a new window.
        let mut new_window = [false; 2];
        for window in [Window::FiveHour, Window::SevenDay] {
            if let Some(usage) = window_of(quota, window) {
                let slot = window as usize;
                new_window[slot] = self.ends_by[slot].is_some_and(|end| quota.sampled_at > end);
                self.ends_by[slot] = Some(usage.ends_by);
            }
        }
        let mut crossed: Vec<(Window, u8, &QuotaWindow)> = Vec::new();
        for (index, &(window, threshold)) in THRESHOLDS.iter().enumerate() {
            let Some(usage) = window_of(quota, window) else {
                continue;
            };
            let armed = &mut self.armed[index];
            if usage.expired {
                *armed = Some(true);
                continue;
            }
            if new_window[window as usize] && armed.is_some() {
                *armed = Some(true);
            }
            let reached = usage.percent >= f64::from(threshold);
            match *armed {
                // First sight: what is already reached stays quiet.
                None => *armed = Some(!reached),
                Some(true) if reached => {
                    *armed = Some(false);
                    crossed.retain(|(w, _, _)| *w != window);
                    crossed.push((window, threshold, usage));
                }
                Some(false) if !reached => *armed = Some(true),
                _ => {}
            }
        }
        if !settings.notify_on_quota {
            return Vec::new();
        }
        crossed
            .into_iter()
            .map(|(window, _, usage)| toast(window, usage, now_ms))
            .collect()
    }

    /// The toast when Desktop stopped sampling the quota while a session
    /// works: no sample for [`STOPPED_AFTER_MS`] of the person's activity
    /// (an idle pause, when Desktop skips its samples, starts the count
    /// over). Once per latest sample: a new sample rearms it. `idle_ms`:
    /// time since the person's last input, None when unknown. Call it every
    /// few seconds, the time alone makes it due.
    pub fn check_stop(
        &mut self,
        snapshot: &Snapshot,
        settings: &Settings,
        now_ms: i64,
        idle_ms: Option<i64>,
    ) -> Option<Toast> {
        let idle = idle_ms.unwrap_or(0).max(0);
        if idle >= DESKTOP_IDLE_MS {
            self.active_since = None;
        } else if self.active_since.is_none() {
            self.active_since = Some(now_ms - idle);
        }
        let quota = snapshot.quota.as_ref()?;
        // Sessions report it as they run: there is nothing to start again.
        if quota.fed_by_sessions {
            return None;
        }
        let sampled_at = quota.sampled_at;
        let silent_since = sampled_at.max(self.active_since?);
        if now_ms - silent_since < STOPPED_AFTER_MS
            || self.stop_announced == Some(sampled_at)
            || snapshot.counts.working == 0
        {
            return None;
        }
        self.stop_announced = Some(sampled_at);
        settings.notify_on_quota.then(|| Toast {
            key: format!("{MONITOR_PREFIX}quota-stopped"),
            title: "Quota : relevé arrêté".to_owned(),
            body: format!(
                "Claude Desktop n'a pas relevé le quota depuis {}. Clic droit sur son icône, dans la zone de notification, pour le relancer.",
                duration(now_ms - sampled_at)
            ),
        })
    }
}

fn window_of(quota: &Quota, window: Window) -> Option<&QuotaWindow> {
    match window {
        Window::FiveHour => quota.five_hour.as_ref(),
        Window::SevenDay => quota.seven_day.as_ref(),
    }
}

/// "Quota 5 h à 82 %", "Réinitialisation dans 1 h 20", with "Presque épuisé"
/// from 95 % and "Épuisé" at 100 %. One key per window: a later alert of the
/// same window replaces the earlier one, nothing else. A click shows the
/// monitor.
fn toast(window: Window, usage: &QuotaWindow, now_ms: i64) -> Toast {
    let (name, key) = match window {
        Window::FiveHour => ("Quota 5 h", "quota-5h"),
        Window::SevenDay => ("Quota de la semaine", "quota-7d"),
    };
    let percent = usage.percent.round().clamp(0.0, 999.0) as u32;
    let reset = match usage.resets_at {
        Some(at) if at > now_ms => format!("Réinitialisation dans {}", duration(at - now_ms)),
        _ => "Heure de réinitialisation inconnue".to_owned(),
    };
    let body = if usage.percent >= 100.0 {
        format!("Épuisé. {reset}")
    } else if usage.percent >= 95.0 {
        format!("Presque épuisé. {reset}")
    } else {
        reset
    };
    Toast {
        key: format!("{MONITOR_PREFIX}{key}"),
        title: format!("{name} à {percent} %"),
        body,
    }
}

/// "45 min", "1 h 20", "6 j 4 h" (minutes rounded up: never too early).
fn duration(ms: i64) -> String {
    let minutes = (ms.max(0) + 59_999) / 60_000;
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{mins} min"),
        (0, _) => format!("{hours} h {mins:02}"),
        _ => format!("{days} j {hours} h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const MIN: i64 = 60_000;

    fn window(percent: f64) -> Option<QuotaWindow> {
        Some(QuotaWindow {
            percent,
            resets_at: Some(NOW + 80 * MIN),
            ends_by: NOW + 90 * MIN,
            ..QuotaWindow::default()
        })
    }

    fn quota(five_hour: f64, seven_day: f64) -> Quota {
        Quota {
            sampled_at: NOW,
            five_hour: window(five_hour),
            seven_day: window(seven_day),
            fed_by_sessions: false,
        }
    }

    fn titles(alerts: &mut QuotaAlerts, quota: &Quota) -> Vec<String> {
        alerts
            .check(Some(quota), &Settings::default(), NOW)
            .into_iter()
            .map(|t| t.title)
            .collect()
    }

    #[test]
    fn each_threshold_fires_once_per_window() {
        let mut alerts = QuotaAlerts::default();
        assert!(titles(&mut alerts, &quota(40.0, 20.0)).is_empty());
        assert_eq!(titles(&mut alerts, &quota(81.0, 20.0)), ["Quota 5 h à 81 %"]);
        assert!(titles(&mut alerts, &quota(85.0, 20.0)).is_empty(), "once");
        assert_eq!(titles(&mut alerts, &quota(95.0, 20.0)), ["Quota 5 h à 95 %"]);
        assert_eq!(titles(&mut alerts, &quota(96.0, 90.4)), ["Quota de la semaine à 90 %"]);
        // The 5-hour window resets: its thresholds rearm.
        assert!(titles(&mut alerts, &quota(3.0, 91.0)).is_empty());
        assert_eq!(titles(&mut alerts, &quota(82.0, 92.0)), ["Quota 5 h à 82 %"]);
    }

    #[test]
    fn a_jump_over_both_thresholds_announces_the_higher() {
        let mut alerts = QuotaAlerts::default();
        titles(&mut alerts, &quota(50.0, 10.0));
        let toasts = alerts.check(Some(&quota(97.0, 10.0)), &Settings::default(), NOW);
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].title, "Quota 5 h à 97 %");
        assert_eq!(toasts[0].body, "Presque épuisé. Réinitialisation dans 1 h 20");
        assert!(crate::toasts::shows_monitor(&toasts[0].key), "a click shows the monitor");
        assert!(titles(&mut alerts, &quota(99.0, 10.0)).is_empty());
    }

    #[test]
    fn each_window_has_its_own_toast_key() {
        let mut alerts = QuotaAlerts::default();
        alerts.check(Some(&quota(50.0, 50.0)), &Settings::default(), NOW);
        let toasts = alerts.check(Some(&quota(100.0, 100.0)), &Settings::default(), NOW);
        let keys: Vec<&str> = toasts.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, ["monitor:quota-5h", "monitor:quota-7d"]);
        assert_ne!(keys[0], crate::toasts::SUMMARY_KEY);
        assert!(toasts.iter().all(|t| t.body.starts_with("Épuisé. ")));
    }

    #[test]
    fn a_sample_past_the_windows_end_is_a_new_window() {
        let mut alerts = QuotaAlerts::default();
        alerts.check(Some(&quota(50.0, 10.0)), &Settings::default(), NOW);
        assert_eq!(titles(&mut alerts, &quota(84.0, 10.0)), ["Quota 5 h à 84 %"]);
        // Desktop closed across the reset: the next sample, already higher,
        // comes after the window's latest possible end.
        let later = Quota {
            sampled_at: NOW + 91 * MIN,
            ..quota(90.0, 10.0)
        };
        assert_eq!(titles(&mut alerts, &later), ["Quota 5 h à 90 %"]);
    }

    #[test]
    fn what_is_reached_at_start_stays_quiet() {
        let mut alerts = QuotaAlerts::default();
        assert!(titles(&mut alerts, &quota(88.0, 93.0)).is_empty());
        assert_eq!(titles(&mut alerts, &quota(95.5, 94.0)), ["Quota 5 h à 96 %"]);
        assert!(titles(&mut alerts, &quota(96.0, 99.0)).is_empty());
    }

    #[test]
    fn an_expired_window_rearms_its_thresholds() {
        let mut alerts = QuotaAlerts::default();
        titles(&mut alerts, &quota(50.0, 10.0));
        titles(&mut alerts, &quota(85.0, 10.0));
        let mut expired = quota(85.0, 10.0);
        if let Some(w) = expired.five_hour.as_mut() {
            w.expired = true;
        }
        assert!(titles(&mut alerts, &expired).is_empty(), "the stale value says nothing");
        assert_eq!(titles(&mut alerts, &quota(81.0, 10.0)), ["Quota 5 h à 81 %"]);
    }

    #[test]
    fn the_setting_silences_without_losing_track() {
        let mut alerts = QuotaAlerts::default();
        let off = Settings {
            notify_on_quota: false,
            ..Settings::default()
        };
        alerts.check(Some(&quota(50.0, 10.0)), &off, NOW);
        assert!(alerts.check(Some(&quota(85.0, 10.0)), &off, NOW).is_empty());
        // Turned on: the threshold already crossed is not announced late.
        assert!(titles(&mut alerts, &quota(86.0, 10.0)).is_empty());
        assert!(alerts.check(None, &Settings::default(), NOW).is_empty());
    }

    /// A snapshot with `working` sessions at work and a quota sampled at `sampled_at`.
    fn sampled(sampled_at: i64, working: u32) -> Snapshot {
        let mut snapshot = Snapshot {
            ready: true,
            quota: Some(Quota {
                sampled_at,
                ..quota(10.0, 10.0)
            }),
            ..Snapshot::default()
        };
        snapshot.counts.working = working;
        snapshot
    }

    fn stop_at(alerts: &mut QuotaAlerts, snapshot: &Snapshot, at: i64, idle: i64) -> Option<String> {
        alerts
            .check_stop(snapshot, &Settings::default(), at, Some(idle))
            .map(|t| t.body)
    }

    #[test]
    fn a_stop_is_announced_once_per_sample() {
        let mut alerts = QuotaAlerts::default();
        let snapshot = sampled(NOW, 1);
        assert_eq!(stop_at(&mut alerts, &snapshot, NOW, 0), None);
        assert_eq!(stop_at(&mut alerts, &snapshot, NOW + 19 * MIN, 0), None, "Desktop may still sample");
        let toast = alerts
            .check_stop(&snapshot, &Settings::default(), NOW + 20 * MIN, Some(0))
            .expect("toast");
        assert_eq!(toast.title, "Quota : relevé arrêté");
        assert_eq!(
            toast.body,
            "Claude Desktop n'a pas relevé le quota depuis 20 min. Clic droit sur son icône, dans la zone de notification, pour le relancer."
        );
        assert!(crate::toasts::shows_monitor(&toast.key), "a click shows the monitor");
        assert_eq!(stop_at(&mut alerts, &snapshot, NOW + 3 * 60 * MIN, 0), None, "once");
        // A new sample, then Desktop stops again.
        let later = sampled(NOW + 4 * 60 * MIN, 1);
        assert_eq!(stop_at(&mut alerts, &later, NOW + 4 * 60 * MIN + 10 * MIN, 0), None);
        assert!(stop_at(&mut alerts, &later, NOW + 4 * 60 * MIN + 25 * MIN, 0).is_some());
    }

    #[test]
    fn an_idle_pause_lets_desktop_skip_its_samples() {
        let mut alerts = QuotaAlerts::default();
        let snapshot = sampled(NOW, 1);
        stop_at(&mut alerts, &snapshot, NOW, 0);
        // Away 12 minutes: Desktop skipped its samples, the count starts over.
        assert_eq!(stop_at(&mut alerts, &snapshot, NOW + 18 * MIN, 11 * MIN), None);
        assert_eq!(stop_at(&mut alerts, &snapshot, NOW + 22 * MIN, 30_000), None, "back for 30 s");
        assert_eq!(stop_at(&mut alerts, &snapshot, NOW + 41 * MIN, 0), None);
        assert!(stop_at(&mut alerts, &snapshot, NOW + 42 * MIN, 0).is_some());
    }

    #[test]
    fn a_quota_fed_by_sessions_never_stops() {
        let mut alerts = QuotaAlerts::default();
        let mut snapshot = sampled(NOW, 1);
        if let Some(quota) = &mut snapshot.quota {
            quota.fed_by_sessions = true;
        }
        stop_at(&mut alerts, &snapshot, NOW, 0);
        assert_eq!(stop_at(&mut alerts, &snapshot, NOW + 3 * 60 * MIN, 0), None);
    }

    #[test]
    fn a_stop_waits_for_a_session_at_work() {
        let mut alerts = QuotaAlerts::default();
        stop_at(&mut alerts, &sampled(NOW, 0), NOW, 0);
        assert_eq!(stop_at(&mut alerts, &sampled(NOW, 0), NOW + 2 * 60 * MIN, 0), None);
        assert!(stop_at(&mut alerts, &sampled(NOW, 2), NOW + 2 * 60 * MIN, 0).is_some());
        // Unknown idle time: taken as active.
        let mut alerts = QuotaAlerts::default();
        let snapshot = sampled(NOW, 1);
        assert!(alerts.check_stop(&snapshot, &Settings::default(), NOW, None).is_none());
        assert!(alerts.check_stop(&snapshot, &Settings::default(), NOW + 20 * MIN, None).is_some());
        assert!(alerts.check_stop(&Snapshot::default(), &Settings::default(), NOW + 60 * MIN, None).is_none());
    }

    #[test]
    fn the_setting_passes_over_a_stop_for_good() {
        let mut alerts = QuotaAlerts::default();
        let off = Settings {
            notify_on_quota: false,
            ..Settings::default()
        };
        let snapshot = sampled(NOW, 1);
        alerts.check_stop(&snapshot, &off, NOW, Some(0));
        assert!(alerts.check_stop(&snapshot, &off, NOW + 30 * MIN, Some(0)).is_none());
        // Turned on: the stop already passed over is not announced late.
        assert_eq!(stop_at(&mut alerts, &snapshot, NOW + 40 * MIN, 0), None);
    }

    #[test]
    fn the_reset_time_reads_as_a_delay() {
        let at = |resets_at: Option<i64>| QuotaWindow {
            percent: 91.0,
            resets_at,
            ..QuotaWindow::default()
        };
        let body = |w: QuotaWindow| toast(Window::SevenDay, &w, NOW).body;
        assert_eq!(body(at(Some(NOW + 6 * 1440 * MIN + 4 * 60 * MIN))), "Réinitialisation dans 6 j 4 h");
        assert_eq!(body(at(Some(NOW + 45 * MIN - 1))), "Réinitialisation dans 45 min");
        assert_eq!(body(at(Some(NOW - MIN))), "Heure de réinitialisation inconnue");
        assert_eq!(body(at(None)), "Heure de réinitialisation inconnue");
        assert_eq!(toast(Window::SevenDay, &at(None), NOW).title, "Quota de la semaine à 91 %");
    }
}
