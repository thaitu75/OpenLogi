//! UI localization plumbing.
//!
//! Translations live in `crates/openlogi-gui/locales/app.yml` and are loaded
//! at compile time by the `rust_i18n::i18n!` macro in `main.rs`. Call sites use
//! the [`tr!`](crate::tr) helper (or `rust_i18n::t!`) with the **English string
//! as the key** — a missing entry still falls back to that English text. Each
//! entry lists all shipped locales (`en` / `zh-CN` / `zh-HK`), with the `en:`
//! column mirroring the key as the canonical source.
//!
//! The current locale is a process-global atomic inside `rust_i18n`. Setting it
//! re-localizes both our own call sites *and* gpui-component's built-in widget
//! strings, since the framework reads the same global. Apply it once at startup
//! via [`apply`] and on a live switch via
//! [`AppState::set_language`](crate::state::AppState::set_language); each must be
//! followed by a window refresh so open views re-render with the new locale.

use openlogi_core::config::AppSettings;

/// Locales the GUI ships, as `(code, native name)`. The codes match the
/// sub-keys in `locales/app.yml` *and* gpui-component's bundled `ui.yml`, so
/// choosing one also localizes the framework's own widgets. Order here is the
/// order shown in the Settings picker (after a leading "Follow system").
pub const SUPPORTED: &[(&str, &str)] = &[
    ("en", "English"),
    ("zh-CN", "简体中文"),
    ("zh-HK", "繁體中文"),
];

/// Resolve the locale to apply, preferring an explicit stored `setting`, then
/// the system locale, and finally `"en"`. An unrecognized stored code is
/// treated as "follow system" rather than failing.
#[must_use]
pub fn resolve(setting: Option<&str>) -> &'static str {
    setting
        .and_then(match_supported)
        .or_else(|| {
            sys_locale::get_locale()
                .as_deref()
                .and_then(match_supported)
        })
        .unwrap_or("en")
}

/// Collapse an arbitrary locale string onto one of [`SUPPORTED`], or `None`.
/// `zh-Hant` / `zh-TW` / `zh-HK` / `zh-MO` map to Traditional (`zh-HK`); other
/// `zh*` to Simplified (`zh-CN`); anything starting with `en` to English.
fn match_supported(code: &str) -> Option<&'static str> {
    let lower = code.to_ascii_lowercase();
    if lower.starts_with("zh") {
        let traditional = ["hant", "-tw", "-hk", "-mo"]
            .iter()
            .any(|t| lower.contains(t));
        Some(if traditional { "zh-HK" } else { "zh-CN" })
    } else if lower.starts_with("en") {
        Some("en")
    } else {
        None
    }
}

/// Apply the configured language to the process-global locale at startup.
/// Safe to call before any window opens — the locale is a plain atomic.
pub fn apply(settings: &AppSettings) {
    rust_i18n::set_locale(resolve(settings.language.as_deref()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_chinese_variants_to_script() {
        assert_eq!(match_supported("zh-Hans-CN"), Some("zh-CN"));
        assert_eq!(match_supported("zh-CN"), Some("zh-CN"));
        assert_eq!(match_supported("zh-Hant-TW"), Some("zh-HK"));
        assert_eq!(match_supported("zh-HK"), Some("zh-HK"));
        assert_eq!(match_supported("en-US"), Some("en"));
        assert_eq!(match_supported("fr-FR"), None);
    }

    #[test]
    fn explicit_setting_wins_over_system() {
        assert_eq!(resolve(Some("zh-CN")), "zh-CN");
        // An unknown stored code falls through to system/`en`, never panics.
        assert!(
            SUPPORTED
                .iter()
                .any(|(c, _)| *c == resolve(Some("klingon")))
        );
    }

    /// End-to-end check that `locales/app.yml` loaded and the gettext-style
    /// English keys match — a typo'd key would silently fall back to English,
    /// which this would catch. Sets the process-global locale, so it owns that
    /// state; no other test reads it.
    #[test]
    fn locale_file_resolves_known_keys() {
        // The accessibility blurb is the longest, most typo-prone key.
        const BLURB: &str = "OpenLogi captures mouse buttons (Back / Forward / gesture button) through the system Accessibility permission and runs the actions you bind. Features that talk to the device directly — DPI, SmartShift — are unaffected.";

        rust_i18n::set_locale("zh-CN");
        assert_eq!(rust_i18n::t!("Settings"), "设置"); // GUI chrome
        assert_eq!(rust_i18n::t!("Left Click"), "左键单击"); // core enum label
        assert_eq!(rust_i18n::t!("Bind %{name}", name => "X"), "绑定 X"); // interpolation
        // Resolves to *something other than* the English source ⇒ the key hit.
        assert_ne!(
            rust_i18n::t!(BLURB),
            BLURB,
            "blurb key missing from app.yml"
        );

        // The explicit `en:` column resolves back to the English source.
        rust_i18n::set_locale("en");
        assert_eq!(rust_i18n::t!("Settings"), "Settings");
        assert_eq!(rust_i18n::t!(BLURB), BLURB);
    }
}
