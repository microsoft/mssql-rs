// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! LCID (Locale Identifier) to Encoding Mapping Module
//!
//! This module provides functionality to map Windows LCIDs to their corresponding
//! character encodings. This is essential for correctly decoding TEXT and NTEXT
//! columns in SQL Server, which use LCID-based collations.
//!
//! The mappings are based on the default ANSI code page for each Windows locale,
//! matching the behavior of .NET's SqlClient (TdsParserStateObject.TryReadStringWithEncoding).

use crate::error::Error;
use encoding_rs::Encoding;

/// Maps LCID (Locale Identifier) to the corresponding encoding.
///
/// This function maps Windows LCIDs to their corresponding code pages using the `encoding_rs` crate.
/// The mapping is based on the default ANSI code page for each locale.
///
/// # Common LCID Mappings:
/// - Western European languages (English, German, French, etc.) → Windows-1252
/// - Japanese → Shift_JIS
/// - Simplified Chinese → GBK
/// - Traditional Chinese → Big5
/// - Korean → EUC-KR
/// - Central/Eastern European → Windows-1250
/// - Cyrillic (Russian, Ukrainian, etc.) → Windows-1251
/// - Greek → Windows-1253
/// - Turkish → Windows-1254
/// - Hebrew → Windows-1255
/// - Arabic → Windows-1256
/// - Baltic → Windows-1257
/// - Vietnamese → Windows-1258
///
/// # Arguments
/// * `lcid` - The Windows Locale Identifier (lower 20 bits of SQL collation info)
///
/// # Returns
/// * `Ok(&'static Encoding)` - The corresponding encoding from `encoding_rs`
/// * `Err(Error::UnsupportedEncoding)` - If the LCID is not recognized
///
/// # References
/// - Windows LCID List: https://learn.microsoft.com/windows/win32/intl/locale-identifiers
/// - Code Page Identifiers: https://learn.microsoft.com/windows/win32/intl/code-page-identifiers
/// - .NET SqlClient: TdsParserStateObject.TryReadStringWithEncoding()
pub fn lcid_to_encoding(lcid: u32) -> Result<&'static Encoding, Error> {
    match lcid {
        // Western European (CP1252 - Windows-1252)
        // English variants
        0x0409 | // en-US (United States)
        0x0809 | // en-GB (United Kingdom)
        0x0C09 | // en-AU (Australia)
        0x1009 | // en-CA (Canada)
        0x1409 | // en-NZ (New Zealand)
        0x1809 | // en-IE (Ireland)
        0x1C09 | // en-ZA (South Africa)
        0x2009 | // en-JM (Jamaica)
        0x2409 | // en-029 (Caribbean)
        0x2809 | // en-BZ (Belize)
        0x2C09 | // en-TT (Trinidad)
        0x3009 | // en-ZW (Zimbabwe)
        0x3409 | // en-PH (Philippines)
        // German variants
        0x0407 | // de-DE (Germany)
        0x0807 | // de-CH (Switzerland)
        0x0C07 | // de-AT (Austria)
        0x1007 | // de-LU (Luxembourg)
        0x1407 | // de-LI (Liechtenstein)
        // French variants
        0x040C | // fr-FR (France)
        0x080C | // fr-BE (Belgium)
        0x0C0C | // fr-CA (Canada)
        0x100C | // fr-CH (Switzerland)
        0x140C | // fr-LU (Luxembourg)
        0x180C | // fr-MC (Monaco)
        // Spanish variants
        0x040A | // es-ES (Spain - Traditional)
        0x080A | // es-MX (Mexico)
        0x0C0A | // es-ES (Spain - Modern)
        0x100A | // es-GT (Guatemala)
        0x140A | // es-CR (Costa Rica)
        0x180A | // es-PA (Panama)
        0x1C0A | // es-DO (Dominican Republic)
        0x200A | // es-VE (Venezuela)
        0x240A | // es-CO (Colombia)
        0x280A | // es-PE (Peru)
        0x2C0A | // es-AR (Argentina)
        0x300A | // es-EC (Ecuador)
        0x340A | // es-CL (Chile)
        0x380A | // es-UY (Uruguay)
        0x3C0A | // es-PY (Paraguay)
        0x400A | // es-BO (Bolivia)
        0x440A | // es-SV (El Salvador)
        0x480A | // es-HN (Honduras)
        0x4C0A | // es-NI (Nicaragua)
        0x500A | // es-PR (Puerto Rico)
        // Italian variants
        0x0410 | // it-IT (Italy)
        0x0810 | // it-CH (Switzerland)
        // Dutch variants
        0x0413 | // nl-NL (Netherlands)
        0x0813 | // nl-BE (Belgium)
        // Portuguese variants
        0x0416 | // pt-BR (Brazil)
        0x0816 | // pt-PT (Portugal)
        // Swedish variants
        0x041D | // sv-SE (Sweden)
        0x081D | // sv-FI (Finland)
        // Norwegian variants
        0x0414 | // nb-NO (Norway - Bokmål)
        0x0814 | // nn-NO (Norway - Nynorsk)
        // Danish
        0x0406 | // da-DK (Denmark)
        // Finnish
        0x040B | // fi-FI (Finland)
        // Icelandic
        0x040F | // is-IS (Iceland)
        // Faroese
        0x0438   // fo-FO (Faroe Islands)
        => Ok(encoding_rs::WINDOWS_1252),

        // Japanese (CP932 - Shift_JIS)
        0x0411   // ja-JP (Japan)
        => Ok(encoding_rs::SHIFT_JIS),

        // Simplified Chinese (CP936 - GBK)
        0x0804 | // zh-CN (China)
        0x1004   // zh-SG (Singapore)
        => Ok(encoding_rs::GBK),

        // Traditional Chinese (CP950 - Big5)
        0x0404 | // zh-TW (Taiwan)
        0x0C04 | // zh-HK (Hong Kong SAR)
        0x1404   // zh-MO (Macao SAR)
        => Ok(encoding_rs::BIG5),

        // Korean (CP949 - EUC-KR)
        0x0412   // ko-KR (Korea)
        => Ok(encoding_rs::EUC_KR),

        // Central/Eastern European (CP1250 - Windows-1250)
        // Polish
        0x0415 | // pl-PL (Poland)
        // Czech
        0x0405 | // cs-CZ (Czech Republic)
        // Hungarian
        0x040E | // hu-HU (Hungary)
        // Romanian
        0x0418 | // ro-RO (Romania)
        // Croatian
        0x041A | // hr-HR (Croatia)
        // Slovak
        0x041B | // sk-SK (Slovakia)
        // Slovenian
        0x0424 | // sl-SI (Slovenia)
        // Albanian
        0x041C   // sq-AL (Albania)
        => Ok(encoding_rs::WINDOWS_1250),

        // Cyrillic (CP1251 - Windows-1251)
        // Russian
        0x0419 | // ru-RU (Russia)
        // Ukrainian
        0x0422 | // uk-UA (Ukraine)
        // Belarusian
        0x0423 | // be-BY (Belarus)
        // Bulgarian
        0x0402 | // bg-BG (Bulgaria)
        // Serbian (Cyrillic)
        0x0C1A | // sr-Cyrl-CS (Serbia and Montenegro)
        0x1C1A | // sr-Cyrl-BA (Bosnia and Herzegovina)
        0x281A | // sr-Cyrl-RS (Serbia)
        // Macedonian
        0x042F | // mk-MK (Macedonia)
        // Kazakh
        0x043F | // kk-KZ (Kazakhstan)
        // Uzbek (Cyrillic)
        0x0843 | // uz-Cyrl-UZ (Uzbekistan)
        // Tatar
        0x0444 | // tt-RU (Russia)
        // Kyrgyz
        0x0440   // ky-KG (Kyrgyzstan)
        => Ok(encoding_rs::WINDOWS_1251),

        // Greek (CP1253 - Windows-1253)
        0x0408   // el-GR (Greece)
        => Ok(encoding_rs::WINDOWS_1253),

        // Turkish (CP1254 - Windows-1254)
        0x041F | // tr-TR (Turkey)
        // Azeri (Latin)
        0x042C   // az-Latn-AZ (Azerbaijan)
        => Ok(encoding_rs::WINDOWS_1254),

        // Hebrew (CP1255 - Windows-1255)
        0x040D   // he-IL (Israel)
        => Ok(encoding_rs::WINDOWS_1255),

        // Arabic (CP1256 - Windows-1256)
        0x0401 | // ar-SA (Saudi Arabia)
        0x0801 | // ar-IQ (Iraq)
        0x0C01 | // ar-EG (Egypt)
        0x1001 | // ar-LY (Libya)
        0x1401 | // ar-DZ (Algeria)
        0x1801 | // ar-MA (Morocco)
        0x1C01 | // ar-TN (Tunisia)
        0x2001 | // ar-OM (Oman)
        0x2401 | // ar-YE (Yemen)
        0x2801 | // ar-SY (Syria)
        0x2C01 | // ar-JO (Jordan)
        0x3001 | // ar-LB (Lebanon)
        0x3401 | // ar-KW (Kuwait)
        0x3801 | // ar-AE (U.A.E.)
        0x3C01 | // ar-BH (Bahrain)
        0x4001 | // ar-QA (Qatar)
        // Urdu
        0x0420 | // ur-PK (Pakistan)
        // Farsi/Persian
        0x0429   // fa-IR (Iran)
        => Ok(encoding_rs::WINDOWS_1256),

        // Baltic (CP1257 - Windows-1257)
        // Estonian, Latvian, Lithuanian
        0x0425..=0x0427 // et-EE, lv-LV, lt-LT
        => Ok(encoding_rs::WINDOWS_1257),

        // Vietnamese (CP1258 - Windows-1258)
        0x042A   // vi-VN (Vietnam)
        => Ok(encoding_rs::WINDOWS_1258),

        // Thai (CP874 - Windows-874)
        0x041E   // th-TH (Thailand)
        => Ok(encoding_rs::WINDOWS_874),

        // Unsupported LCID
        _ => Err(Error::UnsupportedEncoding { lcid }),
    }
}

/// Maps a Windows LCID to its ANSI code page, mirroring the server's
/// `x_rgLocaleMap` (`Sql/Common/include/LocaleMap.h`). Used to resolve a
/// Windows (sort-ID 0) collation's code page for `SQL_COLLATION_SEQ` the way
/// msodbcsql's `CodePageFromTDSCollation` does — a broader set than
/// [`lcid_to_encoding`], which covers only encodings this crate can decode.
/// `None` for an LCID the server's locale map does not list.
pub(crate) fn lcid_to_code_page(lcid: u32) -> Option<u16> {
    let code_page = match lcid {
        0x0401 => 1256,
        0x0404 => 950,
        0x0405 => 1250,
        0x0406 => 1252,
        0x0408 => 1253,
        0x0409 => 1252,
        0x040A => 1252,
        0x040B => 1252,
        0x040C => 1252,
        0x040D => 1255,
        0x040E => 1250,
        0x040F => 1252,
        0x0411 => 932,
        0x0412 => 949,
        0x0414 => 1252,
        0x0415 => 1250,
        0x0417 => 1252,
        0x0418 => 1250,
        0x0419 => 1251,
        0x041A => 1250,
        0x041B => 1250,
        0x041C => 1250,
        0x041E => 874,
        0x041F => 1254,
        0x0420 => 1256,
        0x0422 => 1251,
        0x0424 => 1250,
        0x0425 => 1257,
        0x0426 => 1257,
        0x0427 => 1257,
        0x0429 => 1256,
        0x042A => 1258,
        0x042C => 1254,
        0x042E => 1252,
        0x042F => 1251,
        0x043B => 1252,
        0x043F => 1251,
        0x0442 => 1250,
        0x0443 => 1254,
        0x0444 => 1251,
        0x0452 => 1252,
        0x0462 => 1252,
        0x046D => 1251,
        0x047A => 1252,
        0x047C => 1252,
        0x047E => 1252,
        0x0480 => 1256,
        0x0483 => 1252,
        0x0485 => 1251,
        0x048C => 1256,
        0x0804 => 936,
        0x081A => 1250,
        0x0827 => 1257,
        0x082C => 1251,
        0x083B => 1252,
        0x085F => 1252,
        0x0C04 => 950,
        0x0C0A => 1252,
        0x0C1A => 1251,
        0x1404 => 950,
        0x141A => 1250,
        0x201A => 1251,
        0x1_0407 => 1252,
        0x1_040E => 1250,
        0x1_0411 => 932,
        0x1_0412 => 949,
        0x1_0437 => 1252,
        0x2_0804 => 936,
        0x2_1404 => 950,
        0x3_0404 => 950,
        0x4_0411 => 932,
        _ => return None,
    };
    Some(code_page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_western_european_lcids() {
        // English
        assert_eq!(lcid_to_encoding(0x0409).unwrap(), encoding_rs::WINDOWS_1252);
        assert_eq!(lcid_to_encoding(0x0809).unwrap(), encoding_rs::WINDOWS_1252);

        // German
        assert_eq!(lcid_to_encoding(0x0407).unwrap(), encoding_rs::WINDOWS_1252);

        // French
        assert_eq!(lcid_to_encoding(0x040C).unwrap(), encoding_rs::WINDOWS_1252);
    }

    #[test]
    fn test_asian_lcids() {
        // Japanese
        assert_eq!(lcid_to_encoding(0x0411).unwrap(), encoding_rs::SHIFT_JIS);

        // Simplified Chinese
        assert_eq!(lcid_to_encoding(0x0804).unwrap(), encoding_rs::GBK);

        // Traditional Chinese
        assert_eq!(lcid_to_encoding(0x0404).unwrap(), encoding_rs::BIG5);

        // Korean
        assert_eq!(lcid_to_encoding(0x0412).unwrap(), encoding_rs::EUC_KR);
    }

    #[test]
    fn test_eastern_european_lcids() {
        // Polish
        assert_eq!(lcid_to_encoding(0x0415).unwrap(), encoding_rs::WINDOWS_1250);

        // Czech
        assert_eq!(lcid_to_encoding(0x0405).unwrap(), encoding_rs::WINDOWS_1250);

        // Hungarian
        assert_eq!(lcid_to_encoding(0x040E).unwrap(), encoding_rs::WINDOWS_1250);
    }

    #[test]
    fn test_cyrillic_lcids() {
        // Russian
        assert_eq!(lcid_to_encoding(0x0419).unwrap(), encoding_rs::WINDOWS_1251);

        // Ukrainian
        assert_eq!(lcid_to_encoding(0x0422).unwrap(), encoding_rs::WINDOWS_1251);

        // Bulgarian
        assert_eq!(lcid_to_encoding(0x0402).unwrap(), encoding_rs::WINDOWS_1251);
    }

    #[test]
    fn test_other_lcids() {
        // Greek
        assert_eq!(lcid_to_encoding(0x0408).unwrap(), encoding_rs::WINDOWS_1253);

        // Turkish
        assert_eq!(lcid_to_encoding(0x041F).unwrap(), encoding_rs::WINDOWS_1254);

        // Hebrew
        assert_eq!(lcid_to_encoding(0x040D).unwrap(), encoding_rs::WINDOWS_1255);

        // Arabic
        assert_eq!(lcid_to_encoding(0x0401).unwrap(), encoding_rs::WINDOWS_1256);

        // Baltic (Estonian)
        assert_eq!(lcid_to_encoding(0x0425).unwrap(), encoding_rs::WINDOWS_1257);

        // Vietnamese
        assert_eq!(lcid_to_encoding(0x042A).unwrap(), encoding_rs::WINDOWS_1258);

        // Thai
        assert_eq!(lcid_to_encoding(0x041E).unwrap(), encoding_rs::WINDOWS_874);
    }

    #[test]
    fn test_unsupported_lcid() {
        // Unsupported LCID should return error
        assert!(lcid_to_encoding(0xFFFF).is_err());
        assert!(lcid_to_encoding(0x0000).is_err());
    }

    #[test]
    fn lcid_to_code_page_covers_the_full_locale_map() {
        // Entries the narrow encoding table also carries.
        assert_eq!(lcid_to_code_page(0x0409), Some(1252));
        assert_eq!(lcid_to_code_page(0x0419), Some(1251));
        assert_eq!(lcid_to_code_page(0x0411), Some(932));
        // Entries only the server locale map carries (absent from encoding table).
        assert_eq!(lcid_to_code_page(0x0452), Some(1252)); // Welsh
        assert_eq!(lcid_to_code_page(0x042E), Some(1252)); // Upper Sorbian
        // A full-text extended LCID, reachable after the 20-bit mask.
        assert_eq!(lcid_to_code_page(0x1_0407), Some(1252));
        // LCIDs the locale map does not list.
        assert_eq!(lcid_to_code_page(0x0000), None);
        assert_eq!(lcid_to_code_page(0x000F_FFFF), None);
    }
}
