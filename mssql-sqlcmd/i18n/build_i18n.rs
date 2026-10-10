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
pub const LOCALIZED_DIR: &str = "i18n/localized";

pub const LOCALIZED_LOCALES: &[&str] = &[
    "ar-SA", "cs-CZ", "da-DK", "de-DE", "el-GR", "en-GB", "es-ES", "es-MX", "fi-FI", "fr-CA",
    "fr-FR", "he-IL", "hu-HU", "id-ID", "it-IT", "ja-JP", "ko-KR", "nb-NO", "nl-NL", "pl-PL",
    "pt-BR", "pt-PT", "ru-RU", "sk-SK", "sv-SE", "th-TH", "tr-TR", "zh-CN", "zh-TW",
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
    for message in catalog.messages {
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

pub fn read_localized_catalog(
    path: &Path,
    locale: &str,
    source: &BTreeMap<String, MessageSpec>,
) -> Result<BTreeMap<String, String>, String> {
    let catalog = read_catalog(path)?;
    if catalog.language != locale {
        return Err(format!(
            "{} has language {}, expected {locale}",
            path.display(),
            catalog.language
        ));
    }

    let mut messages = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for message in catalog.messages {
        let Some(source_message) = source.get(&message.id) else {
            return Err(format!(
                "{} contains unknown message id {}",
                path.display(),
                message.id
            ));
        };
        if !seen.insert(message.id.clone()) {
            return Err(format!(
                "{} contains duplicate message id {}",
                path.display(),
                message.id
            ));
        }
        if message.translation.is_empty() {
            continue;
        }
        let placeholders = placeholders_in_text(&message.translation)?;
        if placeholders != source_message.placeholders {
            return Err(format!(
                "{} message {} has placeholders {:?}, expected {:?}",
                path.display(),
                message.id,
                placeholders,
                source_message.placeholders
            ));
        }
        messages.insert(message.id, message.translation);
    }
    Ok(messages)
}

pub fn load_catalogs(
    manifest_dir: &Path,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    let source = read_source_catalog(manifest_dir)?;
    let mut catalogs = BTreeMap::new();
    catalogs.insert(
        SOURCE_LOCALE.to_string(),
        source
            .values()
            .map(|message| (message.id.clone(), message.message.clone()))
            .collect(),
    );

    let localized_root = manifest_dir.join(LOCALIZED_DIR);
    if localized_root.exists() {
        let known: BTreeSet<&str> = LOCALIZED_LOCALES.iter().copied().collect();
        for entry in fs::read_dir(&localized_root)
            .map_err(|error| format!("failed to read {}: {error}", localized_root.display()))?
        {
            let entry =
                entry.map_err(|error| format!("failed to read localized entry: {error}"))?;
            if !entry
                .file_type()
                .map_err(|error| {
                    format!("failed to read {} type: {error}", entry.path().display())
                })?
                .is_dir()
            {
                continue;
            }
            let locale = entry.file_name().to_string_lossy().into_owned();
            if !known.contains(locale.as_str()) {
                return Err(format!(
                    "{} is not a supported localized locale folder",
                    entry.path().display()
                ));
            }
            let catalog_path = entry.path().join("sqlcmd.json");
            if catalog_path.exists() {
                let messages = read_localized_catalog(&catalog_path, &locale, &source)?;
                catalogs.insert(locale, messages);
            }
        }
    }

    let pseudo: BTreeMap<String, String> = source
        .values()
        .map(|message| (message.id.clone(), pseudo_localize(&message.message)))
        .collect();
    catalogs.insert(PSEUDO_LOCALE.to_string(), pseudo);
    Ok(catalogs)
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
