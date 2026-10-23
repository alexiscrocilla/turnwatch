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

use monitor_core::model::{Quota, QuotaWindow, Settings};

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

#[derive(Debug, Default)]
pub struct QuotaAlerts {
    /// Per threshold, in [`THRESHOLDS`] order: true while it may fire; None
    /// until its window is first seen.
    armed: [Option<bool>; THRESHOLDS.len()],
    /// Per window: the latest possible end of the window last seen.
    ends_by: [Option<i64>; 2],
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
