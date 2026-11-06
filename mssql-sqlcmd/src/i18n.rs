// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Message catalog lookup and locale resolution for Rust-generated sqlcmd text.
//!
//! The catalog is translated into the 10 languages sqlcmd's Windows installer
//! ships SQLCMD.rll in: de-DE, es-ES, fr-FR, it-IT, ja-JP, ko-KR, pt-BR, ru-RU,
//! zh-CN and zh-TW. Any other language resolves to en-US.

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

// en-US plus the 10 languages sqlcmd's Windows installer ships SQLCMD.rll in.
// Native sqlcmd can pass the LCID of another language (1030 for Danish, or
// 1029 for Czech, whose SQLCMD.rll is translated but not shipped, for example);
// those resolve to en-US, so the report never speaks a language the rest of
// sqlcmd does not.
const SUPPORTED_LOCALES: &[&str] = &[
    DEFAULT_LOCALE,
    "de-DE",
    "es-ES",
    "fr-FR",
    "it-IT",
    "ja-JP",
    "ko-KR",
    "pt-BR",
    "ru-RU",
    "zh-CN",
    "zh-TW",
    PSEUDO_LOCALE,
];

const LANGUAGE_FALLBACKS: &[(&str, &str)] = &[
    ("de", "de-DE"),
    ("en", DEFAULT_LOCALE),
    ("es", "es-ES"),
    ("fr", "fr-FR"),
    ("it", "it-IT"),
    ("ja", "ja-JP"),
    ("ko", "ko-KR"),
    ("pt", "pt-BR"),
    ("ru", "ru-RU"),
    ("zh", "zh-CN"),
];

const LCID_LOCALES: &[(u32, &str)] = &[
    (1028, "zh-TW"),
    (1031, "de-DE"),
    (1033, DEFAULT_LOCALE),
    (1034, "es-ES"),
    (1036, "fr-FR"),
    (1040, "it-IT"),
    (1041, "ja-JP"),
    (1042, "ko-KR"),
    (1046, "pt-BR"),
    (1049, "ru-RU"),
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
    // Only the core tag (language, script, region) chooses: extension and
    // private-use subtags start at the first singleton (`u`, `x`, ...).
    let core_len = parts
        .iter()
        .skip(1)
        .position(|part| part.len() == 1)
        .map_or(parts.len(), |at| at + 1);
    let parts = &parts[..core_len];
    let language = parts.first()?.as_str();

    if language == "zh" {
        // An explicit script decides; only without one does the region.
        if parts.iter().any(|part| part == "hant") {
            return Some("zh-TW");
        }
        if parts.iter().any(|part| part == "hans") {
            return Some("zh-CN");
        }
        if parts
            .iter()
            .any(|part| matches!(part.as_str(), "tw" | "hk" | "mo"))
        {
            return Some("zh-TW");
        }
        if parts
            .iter()
            .any(|part| matches!(part.as_str(), "cn" | "sg"))
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
            // An explicit script wins over the region.
            ("zh-Hans-HK", Some("zh-CN")),
            ("zh-Hans-TW", Some("zh-CN")),
            ("zh_Hans_MO", Some("zh-CN")),
            ("zh-Hant-CN", Some("zh-TW")),
            ("zh-Hant-SG", Some("zh-TW")),
            // Extension and private-use subtags do not choose.
            ("zh-Hans-x-hant", Some("zh-CN")),
            ("zh-HK-x-hans", Some("zh-TW")),
            ("zh-CN-u-nu-hant", Some("zh-CN")),
            ("de-x-fr", Some("de-DE")),
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
            ("1031", "de-DE"),
            ("1034", "es-ES"),
            ("1036", "fr-FR"),
            ("1040", "it-IT"),
            ("1041", "ja-JP"),
            ("1042", "ko-KR"),
            ("1046", "pt-BR"),
            ("1049", "ru-RU"),
            ("2052", "zh-CN"),
            ("1028", "zh-TW"),
            ("3076", "zh-TW"),
            ("3082", "es-ES"),
        ];
        for (lcid, locale) in cases {
            assert_eq!(resolve_locale(lcid), Some(locale), "{lcid}");
        }
    }

    /// Czech, Polish and Turkish: SQLCMD.rll is translated into them but the
    /// installer does not ship it, so sqlcmd is English there, and so is the
    /// report.
    #[test]
    fn languages_sqlcmd_does_not_ship_resolve_to_english() {
        for locale in ["1029", "1045", "1055", "cs-CZ", "pl-PL", "tr-TR", "tr"] {
            assert_eq!(resolve_locale(locale), None, "{locale}");
            assert!(!set_locale(locale), "{locale}");
            assert_eq!(super::locale(), DEFAULT_LOCALE, "{locale}");
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
    fn lcl_text_is_unescaped_as_locstudio_writes_it() {
        assert_eq!(
            build_i18n::unescape_lcl("Feedback:]A;  {url}"),
            "Feedback:\n  {url}"
        );
        assert_eq!(
            build_i18n::unescape_lcl("[{Master}]5D;-Datenbank"),
            "[{Master}]-Datenbank"
        );
        // A `]` that does not start a code is kept.
        assert_eq!(build_i18n::unescape_lcl("[xFFFFFFFF]. "), "[xFFFFFFFF]. ");
        assert_eq!(build_i18n::unescape_lcl("a]GG;b]"), "a]GG;b]");

        let lcl = r#"<?xml version="1.0" encoding="utf-8"?>
<LCX SchemaVersion="6.0" Name="sqlcmd.json" PsrId="306" FileType="1" SrcCul="en-US" TgtCul="de-DE" xmlns="http://schemas.microsoft.com/locstudio/2006/6/lcx">
  <Item ItemId=";String Table" ItemType="0" PsrId="306" Leaf="false">
    <Disp Icon="Expand" Expand="true" Disp="true" LocTbl="false" />
    <Item ItemId=";Strings" ItemType="0" PsrId="306" Leaf="false">
      <Item ItemId=";messages[13].translation" ItemType="0" PsrId="306" Leaf="true">
        <Str Cat="Text">
          <Val><![CDATA[Run a query on [master]5D;]A;now]]></Val>
          <Tgt Cat="Text" Stat="Loc" Orig="New">
            <Val><![CDATA[Abfrage auf [master]5D;]A;jetzt]]></Val>
          </Tgt>
          <Prev Cat="Text">
            <Val><![CDATA[Older English]]></Val>
          </Prev>
        </Str>
        <Disp Icon="Str" />
      </Item>
      <Item ItemId=";messages[14].translation" ItemType="0" PsrId="306" Leaf="true">
        <Str Cat="Text">
          <Val><![CDATA[Not translated yet]]></Val>
        </Str>
      </Item>
      <Item ItemId=";messages[15].translation" ItemType="0" PsrId="306" Leaf="true">
        <Str Cat="Text">
          <Val><![CDATA[Emptied target]]></Val>
          <Tgt Cat="Text" Stat="Loc" Orig="New" />
          <Prev Cat="Text">
            <Val><![CDATA[Not the translation]]></Val>
          </Prev>
        </Str>
      </Item>
      <Item ItemId=";messages[16].translation" ItemType="0" PsrId="306" Leaf="true">
        <Str Cat="Text">
          <Val>a &lt;b&gt; &amp; c&#xA;&#33;</Val>
          <Tgt Cat="Text" Stat="Loc" Orig="New">
            <Val>x &lt;y&gt; &amp; <![CDATA[<z>]]></Val>
          </Tgt>
        </Str>
      </Item>
    </Item>
  </Item>
</LCX>"#;
        let item = |id: &str, source: &str, target: Option<&str>| build_i18n::LclItem {
            id: id.to_string(),
            source: source.to_string(),
            target: target.map(str::to_string),
        };
        assert_eq!(
            build_i18n::parse_lcl(lcl),
            [
                item(
                    "messages[13].translation",
                    "Run a query on [master]\nnow",
                    Some("Abfrage auf [master]\njetzt"),
                ),
                item("messages[14].translation", "Not translated yet", None),
                item("messages[15].translation", "Emptied target", None),
                item(
                    "messages[16].translation",
                    "a <b> & c\n!",
                    Some("x <y> & <z>")
                ),
            ]
        );
    }

    /// Item `messages[N]` is the en-US message at position N: its translation
    /// is used while the English it was translated from is still that
    /// message's, with the same placeholders; anything else leaves English.
    #[test]
    fn lcl_translations_are_used_only_for_their_current_english() {
        let source = read_fixture_source();
        let listener = &source["diagnose.finding.no_tcp_listener"];
        let tcp = &source["diagnose.check.tcp_connect"];
        let encrypt = &source["diagnose.depth.encrypt_suffix"];
        let database = &source["diagnose.depth.database_suffix"];
        let warning = &source["diagnose.sharing.warning"];
        let item = |index: usize| format!("messages[{index}].translation");
        let lcl = lcl_with(&[
            ("language", "en-US", Some("de-DE")),
            (
                &item(listener.index),
                &listener.message,
                Some("Nichts lauscht an Port {port} an diesen Adressen."),
            ),
            // Reworded since it was translated.
            (&item(tcp.index), "TCP connection", Some("TCP-Verbindung")),
            // Past the end of the catalog.
            (&item(source.len() + 7), "Gone", Some("Weg")),
            (
                &item(database.index),
                &database.message,
                Some("; Datenbank {datenbank}"),
            ),
            (
                &item(encrypt.index),
                &encrypt.message,
                Some("Verschlüsselung {encrypt}"),
            ),
            // Not translated yet.
            (&item(warning.index), &warning.message, None),
        ]);
        let catalog = build_i18n::read_lcl(&lcl, &source);
        assert_eq!(
            catalog.messages[&listener.id],
            "Nichts lauscht an Port {port} an diesen Adressen."
        );
        assert_eq!(catalog.stale, [item(tcp.index), item(source.len() + 7)]);
        assert_eq!(
            catalog.placeholder_mismatches,
            std::slice::from_ref(&database.id)
        );
        assert_eq!(catalog.edge_changes, std::slice::from_ref(&encrypt.id));
        assert_eq!(catalog.messages[&encrypt.id], "Verschlüsselung {encrypt}");
        assert_eq!(catalog.messages.len(), 2, "{:?}", catalog.messages);
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
    }

    /// Each language's `.lcl` file is read where the localization team
    /// checks it in; a missing or misplaced one leaves English, with a
    /// warning, never a failed build.
    #[test]
    fn lcl_files_load_and_nothing_in_them_fails_the_build() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("i18n-test-fixtures")
            .join("manifest-with-lcl");
        let _ = std::fs::remove_dir_all(&manifest);
        let source_dir = manifest.join("i18n").join("locales").join("en-US");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::copy(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(build_i18n::SOURCE_CATALOG),
            source_dir.join("sqlcmd.json"),
        )
        .unwrap();

        let catalogs = build_i18n::load_catalogs(&manifest).unwrap();
        assert_eq!(catalogs.warnings.len(), 1, "{:?}", catalogs.warnings);
        assert!(
            catalogs.warnings[0].contains(&build_i18n::LOCALIZED_LOCALES.join(", ")),
            "{:?}",
            catalogs.warnings
        );
        assert!(
            build_i18n::LOCALIZED_LOCALES
                .iter()
                .all(|locale| !catalogs.by_locale.contains_key(*locale))
        );
        assert!(catalogs.by_locale.contains_key("qps-ploc"));

        let source = read_fixture_source();
        let listener = &source["diagnose.finding.no_tcp_listener"];
        let lcl = lcl_with(&[(
            &format!("messages[{}].translation", listener.index),
            &listener.message,
            Some("Nichts lauscht an Port {port} an diesen Adressen."),
        )]);
        let root = manifest.join(build_i18n::LCL_DIR);
        for locale in ["de-DE", "fr-FR"] {
            std::fs::create_dir_all(root.join(locale)).unwrap();
            std::fs::write(root.join(locale).join(build_i18n::LCL_FILE), &lcl).unwrap();
        }
        let catalogs = build_i18n::load_catalogs(&manifest).unwrap();
        assert_eq!(
            catalogs.by_locale["de-DE"][&listener.id],
            "Nichts lauscht an Port {port} an diesen Adressen."
        );
        // The German file in the French folder is not used.
        assert!(!catalogs.by_locale.contains_key("fr-FR"));
        assert_eq!(catalogs.warnings.len(), 2, "{:?}", catalogs.warnings);
        assert!(
            catalogs.warnings[0].contains("is for de-DE, not fr-FR"),
            "{:?}",
            catalogs.warnings
        );
        assert!(
            !catalogs.warnings[1].contains("de-DE") && catalogs.warnings[1].contains("es-ES"),
            "{:?}",
            catalogs.warnings
        );
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

    /// A `.lcl` file as the localization team writes it, for de-DE: each
    /// item with its English and, once translated, its translation.
    fn lcl_with(items: &[(&str, &str, Option<&str>)]) -> String {
        let escape = |text: &str| {
            text.chars()
                .map(|c| match c {
                    ']' => "]5D;".to_string(),
                    c if u32::from(c) < 0x20 => format!("]{:X};", u32::from(c)),
                    c => c.to_string(),
                })
                .collect::<String>()
        };
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<LCX SchemaVersion=\"6.0\" \
             Name=\"sqlcmd.json\" PsrId=\"306\" FileType=\"1\" SrcCul=\"en-US\" TgtCul=\"de-DE\" \
             xmlns=\"http://schemas.microsoft.com/locstudio/2006/6/lcx\">\n  \
             <Item ItemId=\";String Table\" ItemType=\"0\" PsrId=\"306\" Leaf=\"false\">\n    \
             <Item ItemId=\";Strings\" ItemType=\"0\" PsrId=\"306\" Leaf=\"false\">\n",
        );
        for (item, source, target) in items {
            xml.push_str(&format!(
                "      <Item ItemId=\";{item}\" ItemType=\"0\" PsrId=\"306\" Leaf=\"true\">\n        \
                 <Str Cat=\"Text\">\n          <Val><![CDATA[{}]]></Val>\n",
                escape(source)
            ));
            if let Some(target) = target {
                xml.push_str(&format!(
                    "          <Tgt Cat=\"Text\" Stat=\"Loc\" Orig=\"New\">\n            \
                     <Val><![CDATA[{}]]></Val>\n          </Tgt>\n",
                    escape(target)
                ));
            }
            xml.push_str("        </Str>\n        <Disp Icon=\"Str\" />\n      </Item>\n");
        }
        xml.push_str("    </Item>\n  </Item>\n</LCX>\n");
        xml
    }
}
