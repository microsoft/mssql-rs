// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Message catalog lookup and locale resolution for Rust-generated sqlcmd text.

use std::env;
use std::fmt::Display;

#[cfg(not(test))]
use std::sync::{OnceLock, RwLock};

const DEFAULT_LOCALE: &str = "en-US";
const PSEUDO_LOCALE: &str = "qps-ploc";

include!(concat!(env!("OUT_DIR"), "/i18n_catalog.rs"));

#[cfg(not(test))]
static CURRENT_LOCALE: OnceLock<RwLock<&'static str>> = OnceLock::new();

// Unit tests in this crate run concurrently under `cargo test` and some render
// with the default locale. Use a thread-local test override there to avoid
// cross-test races. The integration test in tests/locale.rs runs the library
// without cfg(test), so it covers the shipping OnceLock/RwLock locale state.
#[cfg(test)]
std::thread_local! {
    static TEST_CURRENT_LOCALE: std::cell::RefCell<&'static str> =
        const { std::cell::RefCell::new(DEFAULT_LOCALE) };
}

const SUPPORTED_LOCALES: &[&str] = &[
    DEFAULT_LOCALE,
    "ar-SA",
    "cs-CZ",
    "da-DK",
    "de-DE",
    "el-GR",
    "en-GB",
    "es-ES",
    "es-MX",
    "fi-FI",
    "fr-CA",
    "fr-FR",
    "he-IL",
    "hu-HU",
    "id-ID",
    "it-IT",
    "ja-JP",
    "ko-KR",
    "nb-NO",
    "nl-NL",
    "pl-PL",
    "pt-BR",
    "pt-PT",
    "ru-RU",
    "sk-SK",
    "sv-SE",
    "th-TH",
    "tr-TR",
    "zh-CN",
    "zh-TW",
    PSEUDO_LOCALE,
];

const LANGUAGE_FALLBACKS: &[(&str, &str)] = &[
    ("ar", "ar-SA"),
    ("cs", "cs-CZ"),
    ("da", "da-DK"),
    ("de", "de-DE"),
    ("el", "el-GR"),
    ("en", DEFAULT_LOCALE),
    ("es", "es-ES"),
    ("fi", "fi-FI"),
    ("fr", "fr-FR"),
    ("he", "he-IL"),
    ("hu", "hu-HU"),
    ("id", "id-ID"),
    ("it", "it-IT"),
    ("ja", "ja-JP"),
    ("ko", "ko-KR"),
    ("nb", "nb-NO"),
    ("nl", "nl-NL"),
    ("pl", "pl-PL"),
    ("pt", "pt-BR"),
    ("ru", "ru-RU"),
    ("sk", "sk-SK"),
    ("sv", "sv-SE"),
    ("th", "th-TH"),
    ("tr", "tr-TR"),
    ("zh", "zh-CN"),
];

const LCID_LOCALES: &[(u32, &str)] = &[
    (1025, "ar-SA"),
    (1028, "zh-TW"),
    (1029, "cs-CZ"),
    (1030, "da-DK"),
    (1031, "de-DE"),
    (1032, "el-GR"),
    (1033, DEFAULT_LOCALE),
    (1034, "es-ES"),
    (1035, "fi-FI"),
    (1036, "fr-FR"),
    (1037, "he-IL"),
    (1038, "hu-HU"),
    (1040, "it-IT"),
    (1041, "ja-JP"),
    (1042, "ko-KR"),
    (1043, "nl-NL"),
    (1044, "nb-NO"),
    (1045, "pl-PL"),
    (1046, "pt-BR"),
    (1049, "ru-RU"),
    (1051, "sk-SK"),
    (1053, "sv-SE"),
    (1054, "th-TH"),
    (1055, "tr-TR"),
    (1057, "id-ID"),
    (2052, "zh-CN"),
    (2057, "en-GB"),
    (2058, "es-MX"),
    (2070, "pt-PT"),
    (3076, "zh-TW"),
    (3082, "es-ES"),
    (3084, "fr-CA"),
];

/// Sets the process-wide sqlcmd locale. Returns `true` when `locale` was recognized.
/// Unknown values select the English fallback and return `false`.
pub fn set_locale(locale: &str) -> bool {
    let resolved = resolve_locale(locale);
    let recognized = resolved.is_some();
    let locale = resolved.unwrap_or(DEFAULT_LOCALE);
    #[cfg(test)]
    {
        TEST_CURRENT_LOCALE.with(|current| *current.borrow_mut() = locale);
        recognized
    }
    #[cfg(not(test))]
    {
        let mut guard = match locale_state().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = locale;
        recognized
    }
}

/// Returns the process-wide resolved locale.
pub fn locale() -> &'static str {
    #[cfg(test)]
    {
        TEST_CURRENT_LOCALE.with(|current| *current.borrow())
    }
    #[cfg(not(test))]
    {
        let guard = match locale_state().read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard
    }
}

/// Translates `id` in the current locale, falling back to English and then the id.
pub fn tr(id: &str, args: &[(&str, &dyn Display)]) -> String {
    tr_for_locale(locale(), id, args)
}

pub(crate) fn tr_for_locale(locale: &str, id: &str, args: &[(&str, &dyn Display)]) -> String {
    debug_assert!(
        find_message(DEFAULT_LOCALE, id).is_some(),
        "unknown i18n message id {id}"
    );
    let text = find_message(locale, id)
        .or_else(|| find_message(DEFAULT_LOCALE, id))
        .unwrap_or(id);
    substitute(text, args)
}

#[cfg(not(test))]
fn locale_state() -> &'static RwLock<&'static str> {
    CURRENT_LOCALE.get_or_init(|| RwLock::new(default_locale()))
}

#[cfg(not(test))]
fn default_locale() -> &'static str {
    resolve_from_environment()
}

#[cfg_attr(test, allow(dead_code))]
fn resolve_from_environment() -> &'static str {
    let sqlcmd_lang = env::var("SQLCMD_LANG").ok();
    let lc_all = env::var("LC_ALL").ok();
    let lc_messages = env::var("LC_MESSAGES").ok();
    let lang = env::var("LANG").ok();
    resolve_from_env_values(
        sqlcmd_lang.as_deref(),
        lc_all.as_deref(),
        lc_messages.as_deref(),
        lang.as_deref(),
        user_default_ui_language().as_deref(),
    )
}

pub(crate) fn resolve_from_env_values(
    sqlcmd_lang: Option<&str>,
    lc_all: Option<&str>,
    lc_messages: Option<&str>,
    lang: Option<&str>,
    user_default_ui_language: Option<&str>,
) -> &'static str {
    if let Some(value) = [
        sqlcmd_lang,
        lc_all,
        lc_messages,
        lang,
        user_default_ui_language,
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|value| !value.is_empty())
    {
        return resolve_locale(value).unwrap_or(DEFAULT_LOCALE);
    }
    DEFAULT_LOCALE
}

pub(crate) fn resolve_locale(locale: &str) -> Option<&'static str> {
    let locale = locale.trim();
    if locale.is_empty() {
        return None;
    }
    if locale.eq_ignore_ascii_case("C") || locale.eq_ignore_ascii_case("POSIX") {
        return Some(DEFAULT_LOCALE);
    }
    if locale.eq_ignore_ascii_case(PSEUDO_LOCALE) {
        return Some(PSEUDO_LOCALE);
    }
    if let Some(locale) = resolve_lcid(locale) {
        return Some(locale);
    }

    let without_codeset = locale.split(['.', '@']).next().unwrap_or(locale);
    let tag = without_codeset.replace('_', "-");
    let parts: Vec<String> = tag
        .split('-')
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let language = parts.first()?.as_str();

    if language == "zh" {
        if parts
            .iter()
            .any(|part| matches!(part.as_str(), "hant" | "tw" | "hk" | "mo"))
        {
            return Some("zh-TW");
        }
        if parts
            .iter()
            .any(|part| matches!(part.as_str(), "hans" | "cn" | "sg"))
        {
            return Some("zh-CN");
        }
    }

    if parts.len() >= 2 {
        let candidate = format!("{}-{}", language, parts[1].to_ascii_uppercase());
        if let Some(locale) = supported_locale(&candidate) {
            return Some(locale);
        }
    }

    LANGUAGE_FALLBACKS
        .iter()
        .find_map(|(prefix, locale)| (*prefix == language).then_some(*locale))
}

fn resolve_lcid(locale: &str) -> Option<&'static str> {
    let lcid = if let Some(hex) = locale
        .strip_prefix("0x")
        .or_else(|| locale.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else if locale.chars().all(|ch| ch.is_ascii_digit()) {
        locale.parse().ok()?
    } else {
        return None;
    };
    LCID_LOCALES
        .iter()
        .find_map(|(candidate, locale)| (*candidate == lcid).then_some(*locale))
}

fn supported_locale(candidate: &str) -> Option<&'static str> {
    SUPPORTED_LOCALES
        .iter()
        .find_map(|locale| locale.eq_ignore_ascii_case(candidate).then_some(*locale))
}

fn find_message(locale: &str, id: &str) -> Option<&'static str> {
    let messages = CATALOGS
        .iter()
        .find_map(|(candidate, messages)| (*candidate == locale).then_some(*messages))?;
    messages
        .binary_search_by_key(&id, |(message_id, _)| *message_id)
        .ok()
        .map(|index| messages[index].1)
}

fn substitute(text: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        let after_open = &rest[start + 1..];
        let Some(end) = after_open.find('}') else {
            output.push_str(&rest[start..]);
            return output;
        };
        let name = &after_open[..end];
        if let Some((_, value)) = args.iter().find(|(arg_name, _)| *arg_name == name) {
            output.push_str(&value.to_string());
        } else {
            output.push('{');
            output.push_str(name);
            output.push('}');
        }
        rest = &after_open[end + 1..];
    }
    output.push_str(rest);
    output
}

#[cfg(windows)]
fn user_default_ui_language() -> Option<String> {
    use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

    // SAFETY: GetUserDefaultUILanguage takes no arguments and cannot fail.
    let lcid = unsafe { GetUserDefaultUILanguage() };
    (lcid != 0).then(|| lcid.to_string())
}

#[cfg(not(windows))]
fn user_default_ui_language() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    mod build_i18n {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/i18n/build_i18n.rs"));
    }

    #[test]
    fn normalizes_bcp47_posix_lcid_and_special_zh_forms() {
        let cases = [
            ("de-DE", Some("de-DE")),
            ("de", Some("de-DE")),
            ("de_de.UTF-8", Some("de-DE")),
            ("de_DE@euro", Some("de-DE")),
            ("1031", Some("de-DE")),
            ("0x0407", Some("de-DE")),
            ("3076", Some("zh-TW")),
            ("zh-HK", Some("zh-TW")),
            ("zh-Hant", Some("zh-TW")),
            ("zh-SG", Some("zh-CN")),
            ("zh-Hans", Some("zh-CN")),
            ("fr", Some("fr-FR")),
            ("es", Some("es-ES")),
            ("pt", Some("pt-BR")),
            ("en", Some("en-US")),
            ("C", Some("en-US")),
            ("POSIX", Some("en-US")),
            ("qps-ploc", Some("qps-ploc")),
            ("unknown", None),
        ];
        for (input, expected) in cases {
            assert_eq!(resolve_locale(input), expected, "{input}");
        }
    }

    #[test]
    fn environment_resolution_uses_documented_precedence() {
        assert_eq!(
            resolve_from_env_values(
                Some("fr-CA"),
                Some("de-DE"),
                Some("ja-JP"),
                Some("ko-KR"),
                Some("1031")
            ),
            "fr-CA"
        );
        assert_eq!(
            resolve_from_env_values(
                None,
                Some("de_DE.UTF-8"),
                Some("ja-JP"),
                Some("ko-KR"),
                Some("1031")
            ),
            "de-DE"
        );
        assert_eq!(
            resolve_from_env_values(None, None, Some("ja_JP.UTF-8"), Some("ko-KR"), Some("1031")),
            "ja-JP"
        );
        assert_eq!(
            resolve_from_env_values(None, None, None, Some("C"), Some("1031")),
            "en-US"
        );
        assert_eq!(
            resolve_from_env_values(None, None, None, None, Some("1031")),
            "de-DE"
        );
        assert_eq!(
            resolve_from_env_values(Some("not-real"), None, None, Some("de-DE"), None),
            "en-US"
        );
    }

    #[test]
    fn supported_locales_match_build_catalog_locales() {
        let mut runtime: Vec<&str> = SUPPORTED_LOCALES
            .iter()
            .copied()
            .filter(|locale| *locale != DEFAULT_LOCALE && *locale != PSEUDO_LOCALE)
            .collect();
        runtime.sort_unstable();
        let mut build = build_i18n::LOCALIZED_LOCALES.to_vec();
        build.sort_unstable();
        assert_eq!(runtime, build);
    }

    #[test]
    fn supported_real_locales_have_lcid_mappings() {
        for locale in SUPPORTED_LOCALES
            .iter()
            .copied()
            .filter(|locale| *locale != PSEUDO_LOCALE)
        {
            let lcid = LCID_LOCALES
                .iter()
                .find_map(|(lcid, mapped)| (*mapped == locale).then_some(*lcid))
                .unwrap_or_else(|| panic!("{locale} has no LCID mapping"));
            assert_eq!(resolve_lcid(&lcid.to_string()), Some(locale));
        }
    }

    #[test]
    fn native_windows_sqlcmd_resource_lcids_resolve_to_their_locales() {
        let cases = [
            ("1029", "cs-CZ"),
            ("1031", "de-DE"),
            ("1034", "es-ES"),
            ("1036", "fr-FR"),
            ("1040", "it-IT"),
            ("1041", "ja-JP"),
            ("1042", "ko-KR"),
            ("1045", "pl-PL"),
            ("1046", "pt-BR"),
            ("1049", "ru-RU"),
            ("1055", "tr-TR"),
            ("2052", "zh-CN"),
            ("1028", "zh-TW"),
            ("3076", "zh-TW"),
            ("3082", "es-ES"),
        ];
        for (lcid, locale) in cases {
            assert_eq!(resolve_locale(lcid), Some(locale), "{lcid}");
        }
    }

    #[test]
    fn set_locale_and_locale_round_trip() {
        assert!(set_locale("1031"));
        assert_eq!(locale(), "de-DE");
        assert!(!set_locale("not-a-locale"));
        assert_eq!(locale(), "en-US");
    }

    #[test]
    fn translates_with_substitution_and_fallback() {
        assert_eq!(
            tr_for_locale(
                "zz-ZZ",
                "diagnose.finding.no_tcp_listener",
                &[("port", &1433)]
            ),
            "Nothing is listening on port 1433 at those addresses."
        );
    }

    #[test]
    fn pseudo_locale_routes_catalog_messages_and_preserves_placeholders() {
        assert!(set_locale("qps-ploc"));
        let translated = tr("diagnose.finding.no_tcp_listener", &[("port", &1433)]);
        assert!(translated.starts_with("[!!! "), "{translated}");
        assert!(translated.ends_with(" !!!]"), "{translated}");
        assert!(translated.contains("1433"), "{translated}");
        assert!(!translated.contains("{port}"), "{translated}");
        assert!(set_locale("en-US"));
    }

    #[test]
    fn placeholder_substitution_leaves_unknown_placeholders_visible() {
        assert_eq!(
            substitute("Hello, {name}. {missing}", &[("name", &"Ada")]),
            "Hello, Ada. {missing}"
        );
    }

    #[test]
    fn build_validation_rejects_unknown_ids_and_placeholder_mismatches() {
        let source = read_fixture_source();
        let invalid_id = catalog_with_messages("de-DE", [("new.id", "New", "Neu")]);
        let path = write_fixture("invalid-id.json", &invalid_id);
        let error = build_i18n::read_localized_catalog(&path, "de-DE", &source).unwrap_err();
        assert!(error.contains("unknown message id"), "{error}");

        let bad_placeholder = catalog_with_messages(
            "de-DE",
            [(
                "diagnose.finding.no_tcp_listener",
                "Nothing is listening on port {port} at those addresses.",
                "An Port {anschluss} lauscht nichts.",
            )],
        );
        let path = write_fixture("bad-placeholder.json", &bad_placeholder);
        let error = build_i18n::read_localized_catalog(&path, "de-DE", &source).unwrap_err();
        assert!(error.contains("placeholders"), "{error}");
    }

    #[test]
    fn build_validation_rejects_duplicate_ids_even_when_first_translation_is_empty() {
        let source = read_fixture_source();
        let duplicate = catalog_with_messages(
            "de-DE",
            [
                (
                    "diagnose.finding.no_tcp_listener",
                    "Nothing is listening on port {port} at those addresses.",
                    "",
                ),
                (
                    "diagnose.finding.no_tcp_listener",
                    "Nothing is listening on port {port} at those addresses.",
                    "Nichts lauscht an Port {port} an diesen Adressen.",
                ),
            ],
        );
        let path = write_fixture("duplicate-empty-first.json", &duplicate);
        let error = build_i18n::read_localized_catalog(&path, "de-DE", &source).unwrap_err();
        assert!(error.contains("duplicate message id"), "{error}");
    }

    #[test]
    fn placeholder_scanner_rejects_stray_closing_braces() {
        assert!(build_i18n::placeholders_in_text("a} b {x}").is_err());
        assert!(build_i18n::placeholders_in_text("{x} c}").is_err());
        assert_eq!(
            build_i18n::placeholders_in_text("a {x} b").unwrap(),
            std::collections::BTreeSet::from(["x".to_string()])
        );
    }

    #[test]
    #[ignore]
    fn print_resolved_locale_for_environment() {
        let sqlcmd_lang = env::var("SQLCMD_LANG").ok();
        let lc_all = env::var("LC_ALL").ok();
        let lc_messages = env::var("LC_MESSAGES").ok();
        let lang = env::var("LANG").ok();
        let resolved = resolve_from_env_values(
            sqlcmd_lang.as_deref(),
            lc_all.as_deref(),
            lc_messages.as_deref(),
            lang.as_deref(),
            None,
        );
        println!("resolved locale: {resolved}");
        println!(
            "sample: {}",
            tr_for_locale(
                resolved,
                "diagnose.finding.no_tcp_listener",
                &[("port", &1433)]
            )
        );
    }

    fn read_fixture_source() -> std::collections::BTreeMap<String, build_i18n::MessageSpec> {
        build_i18n::read_source_catalog(&std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")))
            .unwrap()
    }

    fn catalog_with_messages<const N: usize>(
        language: &str,
        messages: [(&str, &str, &str); N],
    ) -> String {
        let messages: Vec<serde_json::Value> = messages
            .into_iter()
            .map(|(id, message, translation)| {
                let placeholders: Vec<serde_json::Value> =
                    build_i18n::placeholders_in_text(message)
                        .unwrap()
                        .into_iter()
                        .enumerate()
                        .map(|(index, id)| {
                            serde_json::json!({
                                "id": id,
                                "string": format!("%[{}]s", index + 1),
                                "type": "string",
                                "underlyingType": "string",
                                "argNum": index + 1,
                                "expr": id,
                            })
                        })
                        .collect();
                serde_json::json!({
                    "id": id,
                    "message": message,
                    "translation": translation,
                    "placeholders": placeholders,
                })
            })
            .collect();
        serde_json::json!({ "language": language, "messages": messages }).to_string()
    }

    fn write_fixture(file_name: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("i18n-test-fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        std::fs::write(&path, contents).unwrap();
        path
    }
}
