// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Where output goes.
//!
//! sqlcmd has two independently redirectable streams — results (`:out`) and
//! messages (`:error`) — each of which may point at stdout, stderr or a file.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// Encoding applied on the way out. `-u` asks for UTF-16LE; `-f o:<cp>` picks a
/// code page; otherwise bytes are written as UTF-8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputEncoding {
    Utf8,
    Utf16Le,
    CodePage(&'static encoding_rs::Encoding),
}

impl OutputEncoding {
    fn encode(self, text: &str) -> Vec<u8> {
        match self {
            OutputEncoding::Utf8 => text.as_bytes().to_vec(),
            OutputEncoding::Utf16Le => {
                let mut bytes = Vec::with_capacity(text.len() * 2);
                for unit in text.encode_utf16() {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
                bytes
            }
            OutputEncoding::CodePage(encoding) => encoding.encode(text).0.into_owned(),
        }
    }

    fn preamble(self) -> &'static [u8] {
        match self {
            // The reference writes a BOM ahead of UTF-16LE output.
            OutputEncoding::Utf16Le => &[0xFF, 0xFE],
            _ => &[],
        }
    }
}

enum Target {
    Stdout,
    Stderr,
    File(File),
}

/// One redirectable stream.
pub struct Sink {
    target: Target,
    encoding: OutputEncoding,
}

impl Sink {
    pub fn stdout(encoding: OutputEncoding) -> Self {
        Self {
            target: Target::Stdout,
            encoding,
        }
    }

    pub fn stderr() -> Self {
        Self {
            target: Target::Stderr,
            encoding: OutputEncoding::Utf8,
        }
    }

    pub fn file(path: &Path, encoding: OutputEncoding, append: bool) -> io::Result<Self> {
        let mut file = File::options()
            .write(true)
            .create(true)
            .append(append)
            .truncate(!append)
            .open(path)?;
        if !append || file.metadata()?.len() == 0 {
            file.write_all(encoding.preamble())?;
        }
        Ok(Self {
            target: Target::File(file),
            encoding,
        })
    }

    pub fn write(&mut self, text: &str) {
        let bytes = self.encoding.encode(text);
        let result = match &mut self.target {
            Target::Stdout => io::stdout().write_all(&bytes),
            Target::Stderr => io::stderr().write_all(&bytes),
            Target::File(file) => file.write_all(&bytes),
        };
        // A closed pipe is the reader's business, not ours.
        let _ = result;
    }

    pub fn flush(&mut self) {
        let _ = match &mut self.target {
            Target::Stdout => io::stdout().flush(),
            Target::Stderr => io::stderr().flush(),
            Target::File(file) => file.flush(),
        };
    }
}

/// Resolves the destination word accepted by `:out` and `:error`.
pub enum Destination {
    Stdout,
    Stderr,
    File(String),
}

impl Destination {
    pub fn parse(word: &str) -> Self {
        match word.trim() {
            "stdout" => Destination::Stdout,
            "stderr" => Destination::Stderr,
            other => Destination::File(other.to_string()),
        }
    }
}

/// Maps a code page number onto an encoding. `65001` is UTF-8.
pub fn encoding_for_code_page(code_page: u32) -> Option<&'static encoding_rs::Encoding> {
    let label = match code_page {
        65001 => "utf-8",
        1200 => "utf-16le",
        1250 => "windows-1250",
        1251 => "windows-1251",
        1252 => "windows-1252",
        1253 => "windows-1253",
        1254 => "windows-1254",
        1255 => "windows-1255",
        1256 => "windows-1256",
        1257 => "windows-1257",
        1258 => "windows-1258",
        932 => "shift_jis",
        936 => "gbk",
        949 => "euc-kr",
        950 => "big5",
        874 => "windows-874",
        20127 => "windows-1252",
        28591 => "windows-1252",
        28592 => "iso-8859-2",
        _ => return None,
    };
    encoding_rs::Encoding::for_label(label.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_output_is_little_endian_and_starts_with_a_bom() {
        assert_eq!(OutputEncoding::Utf16Le.preamble(), &[0xFF, 0xFE]);
        assert_eq!(OutputEncoding::Utf16Le.encode("hi"), vec![b'h', 0, b'i', 0]);
    }

    #[test]
    fn utf8_output_has_no_preamble() {
        assert!(OutputEncoding::Utf8.preamble().is_empty());
        assert_eq!(OutputEncoding::Utf8.encode("hi"), b"hi".to_vec());
    }

    #[test]
    fn code_pages_map_onto_encodings() {
        assert!(encoding_for_code_page(65001).is_some());
        assert!(encoding_for_code_page(1252).is_some());
        assert!(encoding_for_code_page(9999).is_none());
    }

    #[test]
    fn destination_words_are_recognised_before_filenames() {
        assert!(matches!(Destination::parse("stdout"), Destination::Stdout));
        assert!(matches!(Destination::parse("stderr"), Destination::Stderr));
        assert!(matches!(Destination::parse("out.txt"), Destination::File(f) if f == "out.txt"));
    }
}
