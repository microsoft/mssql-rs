// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// This file is reached only through include! from build.rs and unit tests, so
// cargo fmt does not discover it. Run `rustfmt --edition 2024 i18n/build_i18n.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::Deserialize;

pub const SOURCE_LOCALE: &str = "en-US";
pub const PSEUDO_LOCALE: &str = "qps-ploc";
pub const SOURCE_CATALOG: &str = "i18n/locales/en-US/sqlcmd.json";
/// Where the localization team checks in each language's translations, as
/// `<lang>/sqlcmd.json.lcl` (the `LclFile` of `i18n/LocProject.json`).
pub const LCL_DIR: &str = "i18n/LCL";
pub const LCL_FILE: &str = "sqlcmd.json.lcl";

/// The languages sqlcmd ships translated (its Windows installer's, and the
/// localization team's standard set), and only those: the `Languages` of
/// `i18n/LocProject.json`.
pub const LOCALIZED_LOCALES: &[&str] = &[
    "de-DE", "es-ES", "fr-FR", "it-IT", "ja-JP", "ko-KR", "pt-BR", "ru-RU", "zh-CN", "zh-TW",
];

#[derive(Debug, Deserialize)]
pub struct Catalog {
    pub language: String,
    #[serde(default)]
    pub messages: Vec<CatalogMessage>,
}

#[derive(Debug, Deserialize)]
pub struct CatalogMessage {
    pub id: String,
    pub message: String,
    #[serde(default)]
    pub translation: String,
    #[serde(default)]
    pub placeholders: Vec<CatalogPlaceholder>,
}

#[derive(Debug, Deserialize)]
pub struct CatalogPlaceholder {
    pub id: String,
    #[serde(flatten)]
    _rest: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct MessageSpec {
    pub id: String,
    pub message: String,
    pub placeholders: BTreeSet<String>,
    /// Its position in the en-US catalog, which `.lcl` items are named by.
    pub index: usize,
}

pub fn read_source_catalog(manifest_dir: &Path) -> Result<BTreeMap<String, MessageSpec>, String> {
    let path = manifest_dir.join(SOURCE_CATALOG);
    let catalog = read_catalog(&path)?;
    if catalog.language != SOURCE_LOCALE {
        return Err(format!(
            "{} has language {}, expected {SOURCE_LOCALE}",
            path.display(),
            catalog.language
        ));
    }

    let mut messages = BTreeMap::new();
    for (index, message) in catalog.messages.into_iter().enumerate() {
        if message.id.trim().is_empty() {
            return Err(format!(
                "{} contains a message with an empty id",
                path.display()
            ));
        }
        let from_text = placeholders_in_text(&message.message)?;
        let from_metadata = placeholders_from_metadata(&message.placeholders);
        if from_text != from_metadata {
            return Err(format!(
                "{} message {} has placeholder metadata {:?}, but text uses {:?}",
                path.display(),
                message.id,
                from_metadata,
                from_text
            ));
        }
        if message.translation != message.message {
            return Err(format!(
                "{} message {} must use the English message as its source translation",
                path.display(),
                message.id
            ));
        }
        let spec = MessageSpec {
            id: message.id.clone(),
            message: message.message,
            placeholders: from_text,
            index,
        };
        if messages.insert(message.id.clone(), spec).is_some() {
            return Err(format!(
                "{} contains duplicate message id {}",
                path.display(),
                message.id
            ));
        }
    }
    Ok(messages)
}

/// Whether `translation` changed what the renderer relies on at the edges of
/// `source`: the whitespace before and after it, and, when the source starts
/// with punctuation (a fragment the renderer appends, such as `"; encrypt
/// {encrypt}"`), that the translation starts with punctuation too. Not
/// repaired: a separator can legitimately differ (a CJK `、` has no trailing
/// space), so a difference is reported, not fixed.
pub fn edges_differ(source: &str, translation: &str) -> bool {
    let leading = |text: &str| text.len() - text.trim_start().len();
    let trailing = |text: &str| text.len() - text.trim_end().len();
    let starts_with_punctuation = |text: &str| {
        text.trim_start()
            .chars()
            .next()
            .is_some_and(|c| !c.is_alphanumeric() && c != '{')
    };
    source[..leading(source)] != translation[..leading(translation)]
        || source[source.len() - trailing(source)..]
            != translation[translation.len() - trailing(translation)..]
        || (starts_with_punctuation(source) && !starts_with_punctuation(translation))
}

/// One string of a `.lcl` file: its item id (`messages[N].translation`), the
/// English the translator saw, and the translation, if there is one yet.
#[derive(Debug, PartialEq, Eq)]
pub struct LclItem {
    pub id: String,
    pub source: String,
    pub target: Option<String>,
}

/// The strings of a `.lcl` file, the localization team's format (LocStudio
/// XML): one leaf `<Item ItemId=";...">` per string, with the English in
/// `<Str><Val>` and the translation in `<Str><Tgt><Val>`. `<Prev>`, the
/// English an item had before, is ignored.
pub fn parse_lcl(text: &str) -> Vec<LclItem> {
    let mut items = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<Item ") {
        rest = &rest[start + "<Item ".len()..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let tag = &rest[..tag_end];
        rest = &rest[tag_end + 1..];
        let Some(id) = attribute(tag, "ItemId") else {
            continue;
        };
        if tag.ends_with('/') {
            continue;
        }
        // A string's item holds no items: it ends at its `</Item>`, or at the
        // next item when it is malformed.
        let end = [rest.find("</Item>"), rest.find("<Item ")]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(rest.len());
        let body = &rest[..end];
        let Some(body) = body.find("<Str").map(|at| &body[at..]) else {
            continue;
        };
        let target_at = body.find("<Tgt");
        let source_end = [target_at, body.find("<Prev")]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(body.len());
        let Some(source) = val(&body[..source_end]) else {
            continue;
        };
        // An empty `<Tgt />` has no `</Tgt>`: not translated yet.
        let target = target_at.and_then(|at| {
            let target = &body[at..];
            val(&target[..target.find("</Tgt>")?])
        });
        items.push(LclItem {
            id: id.trim_start_matches(';').to_string(),
            source,
            target,
        });
    }
    items
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let mut from = 0;
    while let Some(at) = tag[from..].find(&key) {
        let at = from + at;
        if at == 0 || tag[..at].ends_with(char::is_whitespace) {
            let start = at + key.len();
            let end = tag[start..].find('"')?;
            return Some(decode_entities(&tag[start..start + end]));
        }
        from = at + key.len();
    }
    None
}

/// The text of the first `<Val>` in `xml`: CDATA as it is, other text with
/// its XML entities decoded, then LocStudio's escapes undone.
fn val(xml: &str) -> Option<String> {
    let start = xml.find("<Val>")? + "<Val>".len();
    let end = xml[start..].find("</Val>")?;
    let mut raw = &xml[start..start + end];
    let mut text = String::new();
    while !raw.is_empty() {
        if let Some(cdata) = raw.strip_prefix("<![CDATA[") {
            let end = cdata.find("]]>")?;
            text.push_str(&cdata[..end]);
            raw = &cdata[end + 3..];
        } else {
            let end = raw.find("<![CDATA[").unwrap_or(raw.len());
            text.push_str(&decode_entities(&raw[..end]));
            raw = &raw[end..];
        }
    }
    Some(unescape_lcl(&text))
}

fn decode_entities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        output.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest.find(';').and_then(|end| {
            let ch = match &rest[1..end] {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                name => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .map(|hex| u32::from_str_radix(hex, 16))
                    .or_else(|| name.strip_prefix('#').map(str::parse::<u32>))
                    .and_then(Result::ok)
                    .and_then(char::from_u32),
            }?;
            Some((ch, end + 1))
        });
        match decoded {
            Some((ch, length)) => {
                output.push(ch);
                rest = &rest[length..];
            }
            None => {
                output.push('&');
                rest = &rest[1..];
            }
        }
    }
    output.push_str(rest);
    output
}

/// LocStudio writes control characters and `]` in `.lcl` text as `]`, the
/// character's code in hex, and `;`: `]A;` is a line feed, `]5D;` is `]`.
/// A `]` that does not start such a code is kept.
pub fn unescape_lcl(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(']') {
        output.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest.find(';').and_then(|end| {
            let hex = &rest[1..end];
            if !(1..=6).contains(&hex.len()) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let ch = char::from_u32(u32::from_str_radix(hex, 16).ok()?)?;
            Some((ch, end + 1))
        });
        match decoded {
            Some((ch, length)) => {
                output.push(ch);
                rest = &rest[length..];
            }
            None => {
                output.push(']');
                rest = &rest[1..];
            }
        }
    }
    output.push_str(rest);
    output
}

/// One language's usable translations, and what was left out or changed.
#[derive(Debug, Default)]
pub struct LocalizedCatalog {
    pub messages: BTreeMap<String, String>,
    /// `.lcl` items translated from English the en-US catalog no longer has
    /// at their position (reworded, moved or removed since): skipped.
    pub stale: Vec<String>,
    /// Message ids whose translation has other placeholders than the
    /// English: skipped.
    pub placeholder_mismatches: Vec<String>,
    /// Message ids whose translation changed the source's edges (see
    /// [`edges_differ`]): kept.
    pub edge_changes: Vec<String>,
}

/// The translations of one language's `.lcl` file. Item `messages[N]` is
/// the message at position N of the en-US catalog; its translation is used
/// only while the English it was translated from is still that message's,
/// and has the same placeholders. Anything else leaves English.
pub fn read_lcl(text: &str, source: &BTreeMap<String, MessageSpec>) -> LocalizedCatalog {
    let by_index: BTreeMap<usize, &MessageSpec> =
        source.values().map(|spec| (spec.index, spec)).collect();
    let mut catalog = LocalizedCatalog::default();
    for item in parse_lcl(text) {
        let Some(index) = item
            .id
            .strip_prefix("messages[")
            .and_then(|rest| rest.strip_suffix("].translation"))
            .and_then(|index| index.parse::<usize>().ok())
        else {
            // `language`, the catalog's own language name.
            continue;
        };
        let Some(target) = item.target.filter(|target| !target.is_empty()) else {
            continue;
        };
        let Some(spec) = by_index
            .get(&index)
            .filter(|spec| spec.message == item.source)
        else {
            catalog.stale.push(item.id);
            continue;
        };
        if catalog.messages.contains_key(&spec.id) {
            continue;
        }
        if placeholders_in_text(&target).ok().as_ref() != Some(&spec.placeholders) {
            catalog.placeholder_mismatches.push(spec.id.clone());
            continue;
        }
        if edges_differ(&spec.message, &target) {
            catalog.edge_changes.push(spec.id.clone());
        }
        catalog.messages.insert(spec.id.clone(), target);
    }
    catalog
}

/// The target language a `.lcl` file names (`<LCX ... TgtCul="de-DE">`).
fn lcl_language(text: &str) -> Option<String> {
    let start = text.find("<LCX ")?;
    let end = text[start..].find('>')?;
    attribute(&text[start..start + end], "TgtCul")
}

/// Every catalog, by locale, and the build's warnings about the
/// translations. None fails the build: the localization team checks in
/// `i18n/LCL`, and whatever of it cannot be used leaves English.
#[derive(Debug)]
pub struct Catalogs {
    pub by_locale: BTreeMap<String, BTreeMap<String, String>>,
    pub warnings: Vec<String>,
}

pub fn load_catalogs(manifest_dir: &Path) -> Result<Catalogs, String> {
    let source = read_source_catalog(manifest_dir)?;
    let mut catalogs = BTreeMap::new();
    let mut warnings = Vec::new();
    let mut missing = Vec::new();
    catalogs.insert(
        SOURCE_LOCALE.to_string(),
        source
            .values()
            .map(|message| (message.id.clone(), message.message.clone()))
            .collect(),
    );

    for &locale in LOCALIZED_LOCALES {
        let path = manifest_dir.join(LCL_DIR).join(locale).join(LCL_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(locale);
                continue;
            }
            Err(error) => {
                warnings.push(format!(
                    "{} cannot be read, so {locale} is English: {error}",
                    path.display()
                ));
                continue;
            }
        };
        if let Some(language) = lcl_language(&text).filter(|language| language != locale) {
            warnings.push(format!(
                "{} is for {language}, not {locale}; {locale} is English",
                path.display()
            ));
            continue;
        }
        let localized = read_lcl(&text, &source);
        warnings.extend(localized.stale.iter().map(|item| {
            format!("{locale}: {item} was translated from English the en-US catalog no longer has there; English is used until the next localization round")
        }));
        warnings.extend(localized.placeholder_mismatches.iter().map(|id| {
            format!("{locale}: {id} has other placeholders than the English; English is used")
        }));
        warnings.extend(localized.edge_changes.iter().map(|id| {
            format!("{locale}: {id} changed its leading or trailing spacing or punctuation; kept")
        }));
        catalogs.insert(locale.to_string(), localized.messages);
    }
    if !missing.is_empty() {
        warnings.push(format!(
            "no {LCL_DIR}/<lang>/{LCL_FILE} yet for {}; English is used",
            missing.join(", ")
        ));
    }

    let pseudo: BTreeMap<String, String> = source
        .values()
        .map(|message| (message.id.clone(), pseudo_localize(&message.message)))
        .collect();
    catalogs.insert(PSEUDO_LOCALE.to_string(), pseudo);
    Ok(Catalogs {
        by_locale: catalogs,
        warnings,
    })
}
pub fn write_generated_catalogs(
    catalogs: &BTreeMap<String, BTreeMap<String, String>>,
    out_file: &Path,
) -> Result<(), String> {
    let mut output = String::from("// @generated by build.rs; do not edit.\n\n");
    for (locale, messages) in catalogs {
        output.push_str(&format!(
            "pub(crate) static {}_MESSAGES: &[(&str, &str)] = &[\n",
            static_name(locale)
        ));
        for (id, text) in messages {
            output.push_str(&format!("    ({id:?}, {text:?}),\n"));
        }
        output.push_str("];\n\n");
    }

    output.push_str("pub(crate) static CATALOGS: &[(&str, &[(&str, &str)])] = &[\n");
    for locale in catalogs.keys() {
        output.push_str(&format!(
            "    ({locale:?}, {}_MESSAGES),\n",
            static_name(locale)
        ));
    }
    output.push_str("];\n");

    fs::write(out_file, output)
        .map_err(|error| format!("failed to write {}: {error}", out_file.display()))
}

pub fn emit_rerun_if_changed(manifest_dir: &Path) -> Result<(), String> {
    let root = manifest_dir.join("i18n");
    println!("cargo:rerun-if-changed={}", root.display());
    if root.exists() {
        emit_rerun_files(&root)?;
    }
    Ok(())
}

fn emit_rerun_files(path: &Path) -> Result<(), String> {
    for entry in
        fs::read_dir(path).map_err(|error| format!("failed to read {}: {error}", path.display()))?
    {
        let entry =
            entry.map_err(|error| format!("failed to read {} entry: {error}", path.display()))?;
        let entry_path = entry.path();
        if entry
            .file_type()
            .map_err(|error| format!("failed to read {} type: {error}", entry_path.display()))?
            .is_dir()
        {
            println!("cargo:rerun-if-changed={}", entry_path.display());
            emit_rerun_files(&entry_path)?;
        } else {
            println!("cargo:rerun-if-changed={}", entry_path.display());
        }
    }
    Ok(())
}

fn read_catalog(path: &Path) -> Result<Catalog, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn placeholders_from_metadata(placeholders: &[CatalogPlaceholder]) -> BTreeSet<String> {
    placeholders
        .iter()
        .map(|placeholder| placeholder.id.clone())
        .collect()
}

pub fn placeholders_in_text(text: &str) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        if rest[..start].contains('}') {
            return Err(format!("unopened placeholder in {text:?}"));
        }
        rest = &rest[start + 1..];
        let Some(end) = rest.find('}') else {
            return Err(format!("unclosed placeholder in {text:?}"));
        };
        let name = &rest[..end];
        if name.is_empty() || !name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric()) {
            return Err(format!("invalid placeholder {{{name}}} in {text:?}"));
        }
        names.insert(name.to_string());
        rest = &rest[end + 1..];
    }
    if rest.contains('}') {
        return Err(format!("unopened placeholder in {text:?}"));
    }
    Ok(names)
}

pub fn pseudo_localize(text: &str) -> String {
    let mut output = String::from("[!!! ");
    let mut index = 0;
    while index < text.len() {
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        if ch == '{'
            && let Some(end) = text[index..].find('}')
        {
            let end = index + end + 1;
            output.push_str(&text[index..end]);
            index = end;
            continue;
        }
        output.push(accent(ch));
        index += ch.len_utf8();
    }
    output.push_str(" !!!]");
    output
}

fn accent(ch: char) -> char {
    match ch {
        'A' => 'Å',
        'C' => 'Ç',
        'E' => 'Ë',
        'I' => 'Ï',
        'N' => 'Ñ',
        'O' => 'Ö',
        'S' => 'Š',
        'U' => 'Û',
        'Y' => 'Ý',
        'Z' => 'Ž',
        'a' => 'å',
        'c' => 'ç',
        'e' => 'ë',
        'i' => 'ï',
        'n' => 'ñ',
        'o' => 'ö',
        's' => 'š',
        'u' => 'û',
        'y' => 'ý',
        'z' => 'ž',
        _ => ch,
    }
}

fn static_name(locale: &str) -> String {
    locale
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}
