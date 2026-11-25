//! The app's two languages: Portuguese and English. Each text on screen is
//! written where it is used with both languages ([`tr`]); a text with values
//! in it picks its format with [`lang`]. The language is the one chosen in
//! the settings, or Windows' own (Portuguese when Windows is in Portuguese,
//! English for anything else). Logs and the command line stay in English.

use serde::{Deserialize, Serialize};

/// A language the app is shown in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Pt,
    En,
}

/// The language chosen in the settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LanguageChoice {
    /// Windows' language.
    #[default]
    Auto,
    Portuguese,
    English,
}

impl LanguageChoice {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Portuguese, Self::English];

    pub fn resolve(self) -> Lang {
        match self {
            Self::Auto => system(),
            Self::Portuguese => Lang::Pt,
            Self::English => Lang::En,
        }
    }

    /// The choice's name, each language in its own words.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => tr("Automático (Windows)", "Automatic (Windows)"),
            Self::Portuguese => "Português",
            Self::English => "English",
        }
    }
}

// Tests start in Portuguese instead (see below).
#[cfg_attr(test, allow(dead_code))]
const UNSET: u8 = 0;
const PT: u8 = 1;
const EN: u8 = 2;

// The language in effect. Tests keep one per thread, so tests running side by
// side never see another test's language; they start in Portuguese, whatever
// Windows' language.
#[cfg(not(test))]
static CURRENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(UNSET);
#[cfg(test)]
thread_local! {
    static CURRENT: std::cell::Cell<u8> = const { std::cell::Cell::new(PT) };
}

fn load() -> u8 {
    #[cfg(not(test))]
    return CURRENT.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    return CURRENT.with(std::cell::Cell::get);
}

fn store(value: u8) {
    #[cfg(not(test))]
    CURRENT.store(value, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    CURRENT.with(|current| current.set(value));
}

/// Shows the app in `lang` from now on.
pub fn set(lang: Lang) {
    store(match lang {
        Lang::Pt => PT,
        Lang::En => EN,
    });
}

/// The language in effect (Windows' until the settings choose one).
pub fn lang() -> Lang {
    match load() {
        PT => Lang::Pt,
        EN => Lang::En,
        _ => {
            let lang = system();
            set(lang);
            lang
        }
    }
}

/// `pt` or `en`, whichever language is in effect.
pub fn tr(pt: &'static str, en: &'static str) -> &'static str {
    match lang() {
        Lang::Pt => pt,
        Lang::En => en,
    }
}

/// Windows' display language: Portuguese, or English for any other.
pub fn system() -> Lang {
    // SAFETY: a plain query without arguments.
    let id = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    from_language_id(id)
}

/// Portuguese for any Portuguese language id (Brazil, Portugal, ...),
/// English for anything else.
fn from_language_id(id: u16) -> Lang {
    const LANG_PORTUGUESE: u16 = 0x16;
    if id & 0x3FF == LANG_PORTUGUESE {
        Lang::Pt
    } else {
        Lang::En
    }
}

/// `n` with thousands separated: "6.000" in Portuguese, "6,000" in English.
pub fn thousands(n: impl Into<u64>) -> String {
    let n: u64 = n.into();
    let separator = match lang() {
        Lang::Pt => '.',
        Lang::En => ',',
    };
    let digits = n.to_string();
    let mut out = String::new();
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(separator);
        }
        out.push(d);
    }
    out
}

/// `value` with `places` decimals: "0,4" in Portuguese, "0.4" in English.
pub fn decimal(value: f64, places: usize) -> String {
    let text = format!("{value:.places$}");
    match lang() {
        Lang::Pt => text.replace('.', ","),
        Lang::En => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_portuguese_is_portuguese_and_the_rest_english() {
        // pt-BR, pt-PT, en-US, es-ES, ja-JP.
        assert_eq!(from_language_id(0x0416), Lang::Pt);
        assert_eq!(from_language_id(0x0816), Lang::Pt);
        assert_eq!(from_language_id(0x0409), Lang::En);
        assert_eq!(from_language_id(0x0C0A), Lang::En);
        assert_eq!(from_language_id(0x0411), Lang::En);
    }

    #[test]
    fn texts_and_numbers_follow_the_language() {
        set(Lang::Pt);
        assert_eq!(tr("Assistir", "Watch"), "Assistir");
        assert_eq!(thousands(6_000_u32), "6.000");
        assert_eq!(thousands(1_234_567_u32), "1.234.567");
        assert_eq!(decimal(0.4, 1), "0,4");
        set(Lang::En);
        assert_eq!(tr("Assistir", "Watch"), "Watch");
        assert_eq!(thousands(6_000_u32), "6,000");
        assert_eq!(thousands(500_u32), "500");
        assert_eq!(decimal(0.4, 1), "0.4");
        assert_eq!(LanguageChoice::Portuguese.resolve(), Lang::Pt);
        assert_eq!(LanguageChoice::English.resolve(), Lang::En);
    }
}
