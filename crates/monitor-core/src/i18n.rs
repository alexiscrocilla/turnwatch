//! The language of the text the monitor writes itself: notifications, the tray
//! menu, source errors and the labels of Desktop's buttons. What sessions
//! write stays in the language it was written in.
//!
//! The `language` setting says "en", "fr", or anything else to follow Windows'
//! display language: French if it is French, else English. The language in
//! use is global, as code deep in the engine needs it: the shell sets it, the
//! rest reads it, and the window gets it in [`Snapshot::lang`]. Text already
//! read from a transcript keeps its language until the transcript changes.
//!
//! [`Snapshot::lang`]: crate::model::Snapshot::lang

use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    En,
    Fr,
}

impl Lang {
    /// `en` or `fr`, whichever is this language.
    pub fn pick<T>(self, en: T, fr: T) -> T {
        match self {
            Lang::En => en,
            Lang::Fr => fr,
        }
    }

    /// "82%" in English, "82 %" in French.
    pub fn percent(self, value: impl Display) -> String {
        match self {
            Lang::En => format!("{value}%"),
            Lang::Fr => format!("{value} %"),
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The language in use.
pub fn current() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        0 => Lang::En,
        _ => Lang::Fr,
    }
}

pub fn set(lang: Lang) {
    CURRENT.store(lang.pick(0, 1), Ordering::Relaxed);
}

/// The language a `language` setting stands for.
pub fn resolve(setting: &str) -> Lang {
    match setting {
        "en" => Lang::En,
        "fr" => Lang::Fr,
        _ => system_lang(),
    }
}

/// Windows' display language, when it is one the monitor speaks.
#[cfg(windows)]
fn system_lang() -> Lang {
    use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
    /// The primary language of a LANGID is its low 10 bits.
    const PRIMARY_LANGUAGE_MASK: u16 = 0x03FF;
    const LANG_FRENCH: u16 = 0x0C;

    // SAFETY: takes nothing and only reads the user's settings.
    let id = unsafe { GetUserDefaultUILanguage() };
    if id & PRIMARY_LANGUAGE_MASK == LANG_FRENCH {
        Lang::Fr
    } else {
        Lang::En
    }
}

#[cfg(not(windows))]
fn system_lang() -> Lang {
    Lang::En
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_language_picks_its_text() {
        assert_eq!(Lang::En.pick("Quit", "Quitter"), "Quit");
        assert_eq!(Lang::Fr.pick("Quit", "Quitter"), "Quitter");
    }

    #[test]
    fn percents_follow_each_language() {
        assert_eq!(Lang::En.percent(82), "82%");
        assert_eq!(Lang::Fr.percent(82), "82 %");
    }

    #[test]
    fn a_setting_names_a_language_or_follows_windows() {
        assert_eq!(resolve("en"), Lang::En);
        assert_eq!(resolve("fr"), Lang::Fr);
        assert_eq!(resolve("auto"), system_lang());
        assert_eq!(resolve("de"), system_lang());
        assert_eq!(resolve(""), system_lang());
    }

    #[test]
    fn the_language_travels_as_two_letters() {
        assert_eq!(serde_json::to_string(&Lang::Fr).unwrap(), r#""fr""#);
        assert_eq!(serde_json::from_str::<Lang>(r#""en""#).unwrap(), Lang::En);
    }
}
