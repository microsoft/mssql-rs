// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Message catalog lookup and locale resolution for Rust-generated sqlcmd text.
//!
//! The catalog is translated into the 13 languages native sqlcmd's SQLCMD.rll
//! is translated into: cs-CZ, de-DE, es-ES, fr-FR, it-IT, ja-JP, ko-KR, pl-PL,
//! pt-BR, ru-RU, tr-TR, zh-CN and zh-TW. Any other language resolves to en-US.

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

// en-US plus the 13 languages native sqlcmd's SQLCMD.rll is translated into.
// Native builds also ship English copies of SQLCMD.rll for other driver
// languages and pass their LCID (1030 for Danish, for example); those resolve
// to en-US, so the report never speaks a language the rest of sqlcmd does not.
const SUPPORTED_LOCALES: &[&str] = &[
    DEFAULT_LOCALE,
    "cs-CZ",
    "de-DE",
    "es-ES",
    "fr-FR",
    "it-IT",
    "ja-JP",
    "ko-KR",
    "pl-PL",
    "pt-BR",
    "ru-RU",
    "tr-TR",
    "zh-CN",
    "zh-TW",
    PSEUDO_LOCALE,
];

const LANGUAGE_FALLBACKS: &[(&str, &str)] = &[
    ("cs", "cs-CZ"),
    ("de", "de-DE"),
    ("en", DEFAULT_LOCALE),
    ("es", "es-ES"),
    ("fr", "fr-FR"),
    ("it", "it-IT"),
    ("ja", "ja-JP"),
    ("ko", "ko-KR"),
    ("pl", "pl-PL"),
    ("pt", "pt-BR"),
    ("ru", "ru-RU"),
    ("tr", "tr-TR"),
    ("zh", "zh-CN"),
];

const LCID_LOCALES: &[(u32, &str)] = &[
    (1028, "zh-TW"),
    (1029, "cs-CZ"),
    (1031, "de-DE"),
    (1033, DEFAULT_LOCALE),
    (1034, "es-ES"),
    (1036, "fr-FR"),
    (1040, "it-IT"),
    (1041, "ja-JP"),
    (1042, "ko-KR"),
    (1045, "pl-PL"),
    (1046, "pt-BR"),
    (1049, "ru-RU"),
    (1055, "tr-TR"),
    (2052, "zh-CN"),
    (3076, "zh-TW"),
    (3082, "es-ES"),
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
            // Regions without a translation take their language's.
            ("es-MX", Some("es-ES")),
            ("fr-CA", Some("fr-FR")),
            ("pt-PT", Some("pt-BR")),
            ("en-GB", Some("en-US")),
        ];
        for (input, expected) in cases {
            assert_eq!(resolve_locale(input), expected, "{input}");
        }
    }

    #[test]
    fn languages_native_sqlcmd_does_not_translate_resolve_to_english() {
        // Native ships English copies of SQLCMD.rll under these LCIDs (Arabic,
        // Danish, English UK, Spanish Mexico, Portuguese Portugal, French
        // Canada, ...), so the report stays English with the rest of sqlcmd.
        for input in [
            "1025",
            "1030",
            "0x0406",
            "2057",
            "2058",
            "2070",
            "3084",
            "da",
            "da-DK",
            "da_DK.UTF-8",
            "ar-SA",
            "nl-NL",
            "sv-SE",
        ] {
            assert_eq!(resolve_locale(input), None, "{input}");
            assert_eq!(
                resolve_from_env_values(Some(input), None, None, None, None),
                DEFAULT_LOCALE,
                "{input}"
            );
        }
        assert_eq!(
            resolve_from_env_values(None, None, None, None, Some("1030")),
            DEFAULT_LOCALE
        );
        assert!(!set_locale("1030"));
        assert_eq!(locale(), DEFAULT_LOCALE);
    }

    #[test]
    fn environment_resolution_uses_documented_precedence() {
        assert_eq!(
            resolve_from_env_values(
                Some("it-IT"),
                Some("de-DE"),
                Some("ja-JP"),
                Some("ko-KR"),
                Some("1031")
            ),
            "it-IT"
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
    fn loc_project_languages_match_build_catalog_locales() {
        // The OneLocBuild pipeline translates what LocProject.json lists; the
        // build embeds what LOCALIZED_LOCALES lists. They must not drift.
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("i18n/LocProject.json");
        let project: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let items: Vec<&serde_json::Value> = project["Projects"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|project| project["LocItems"].as_array().unwrap())
            .collect();
        assert!(!items.is_empty());
        for item in items {
            let languages: Vec<&str> = item["Languages"].as_str().unwrap().split(';').collect();
            assert_eq!(languages, build_i18n::LOCALIZED_LOCALES);
        }
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
    fn build_validation_skips_removed_ids_and_rejects_placeholder_mismatches() {
        let source = read_fixture_source();
        // An id en-US no longer has (removed before the next localization
        // drop) is skipped and reported; the rest of the catalog still loads.
        let removed_id = catalog_with_messages(
            "de-DE",
            [
                ("removed.id", "Old", "Alt"),
                (
                    "diagnose.finding.no_tcp_listener",
                    "Nothing is listening on port {port} at those addresses.",
                    "Nichts lauscht an Port {port} an diesen Adressen.",
                ),
            ],
        );
        let path = write_fixture("removed-id.json", &removed_id);
        let localized = build_i18n::read_localized_catalog(&path, "de-DE", &source).unwrap();
        assert_eq!(localized.removed_ids, ["removed.id"]);
        assert!(!localized.messages.contains_key("removed.id"));
        assert!(
            localized
                .messages
                .contains_key("diagnose.finding.no_tcp_listener")
        );

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
    fn build_validation_reports_translations_that_change_their_edges() {
        // Spacing and leading punctuation the renderer appends a fragment with.
        assert!(!build_i18n::edges_differ(
            "; encrypt {encrypt}",
            "; Verschlüsselung {encrypt}"
        ));
        assert!(build_i18n::edges_differ(
            "; encrypt {encrypt}",
            "Verschlüsselung {encrypt}"
        ));
        assert!(build_i18n::edges_differ(" (default)", "(Standard)"));
        assert!(build_i18n::edges_differ("[xFFFFFFFF]. ", "[xFFFFFFFF]."));
        // Another punctuation mark is not a change; a separator without its
        // space is reported, not repaired.
        assert!(!build_i18n::edges_differ("; {x}", "；{x}"));
        assert!(build_i18n::edges_differ(", ", "、"));
        // A message that starts with a word or a placeholder may start with
        // anything.
        assert!(!build_i18n::edges_differ(
            "{outcome} (exit code {code})",
            "Ergebnis {outcome}"
        ));
        assert!(!build_i18n::edges_differ("Outcome:", "Ergebnis:"));

        let source = read_fixture_source();
        let catalog = catalog_with_messages(
            "de-DE",
            [
                (
                    "diagnose.depth.encrypt_suffix",
                    "; encrypt {encrypt}",
                    "Verschlüsselung {encrypt}",
                ),
                (
                    "diagnose.depth.database_suffix",
                    "; database {database}",
                    "; Datenbank {database}",
                ),
            ],
        );
        let path = write_fixture("edges.json", &catalog);
        let localized = build_i18n::read_localized_catalog(&path, "de-DE", &source).unwrap();
        assert_eq!(localized.edge_changes, ["diagnose.depth.encrypt_suffix"]);
        // Reported, not dropped: the build keeps the translation.
        assert_eq!(
            localized.messages["diagnose.depth.encrypt_suffix"],
            "Verschlüsselung {encrypt}"
        );
    }

    #[test]
    fn an_unsupported_localized_folder_is_skipped_not_fatal() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("i18n-test-fixtures")
            .join("manifest-with-unknown-folder");
        let _ = std::fs::remove_dir_all(&manifest);
        let source_dir = manifest.join("i18n").join("locales").join("en-US");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::copy(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(build_i18n::SOURCE_CATALOG),
            source_dir.join("sqlcmd.json"),
        )
        .unwrap();
        let localized = manifest.join("i18n").join("localized");
        for (folder, language) in [
            ("zh-Hans", "zh-Hans"),
            ("da-DK", "da-DK"),
            ("de-DE", "de-DE"),
        ] {
            let dir = localized.join(folder);
            std::fs::create_dir_all(&dir).unwrap();
            let catalog = catalog_with_messages(
                language,
                [(
                    "diagnose.finding.no_tcp_listener",
                    "Nothing is listening on port {port} at those addresses.",
                    "Nichts lauscht an Port {port} an diesen Adressen.",
                )],
            );
            std::fs::write(dir.join("sqlcmd.json"), catalog).unwrap();
        }
        std::fs::create_dir_all(localized.join("scratch")).unwrap();

        let catalogs = build_i18n::load_catalogs(&manifest).unwrap();
        let skipped: Vec<String> = catalogs
            .skipped_folders
            .iter()
            .map(|path| {
                std::path::Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(skipped, ["da-DK", "scratch", "zh-Hans"]);
        assert!(catalogs.by_locale.contains_key("de-DE"));
        assert!(!catalogs.by_locale.contains_key("da-DK"));
        assert!(!catalogs.by_locale.contains_key("zh-Hans"));

        // A content defect in a supported folder is still an error.
        let wrong_language = catalog_with_messages("fr-FR", []);
        std::fs::write(localized.join("de-DE").join("sqlcmd.json"), wrong_language).unwrap();
        let error = build_i18n::load_catalogs(&manifest).unwrap_err();
        assert!(error.contains("expected de-DE"), "{error}");
        let _ = std::fs::remove_dir_all(&manifest);
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
