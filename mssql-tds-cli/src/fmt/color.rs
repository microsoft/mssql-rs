// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `SQLCMDCOLORSCHEME` — colouring the results, messages and echoed statements.
//!
//! This is a go-sqlcmd feature; ODBC `sqlcmd` has nothing like it. The variable
//! names a scheme, and when it is set **and** the destination is a terminal,
//! output carries 24-bit ANSI colour. Anything redirected is left plain, so a
//! script capturing output never sees escape sequences — that gating is
//! go-sqlcmd's and is reproduced exactly.
//!
//! A name that matches no scheme still colours, because chroma answers an
//! unknown name with its fallback style rather than an error.

use super::schemes::SCHEMES;
use crate::messages::EOL;

/// What chroma returns for a name it does not know.
const FALLBACK_SCHEME: &str = "swapoff";

/// How one kind of text is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Face {
    /// 24-bit foreground, or `None` to leave the terminal's own.
    pub rgb: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Face {
    /// Whether this face would change anything.
    fn is_plain(&self) -> bool {
        self.rgb.is_none() && !self.bold && !self.italic && !self.underline
    }

    /// Wraps `text` in the escape sequences for this face.
    ///
    /// Emphasis and colour go in separate sequences rather than one combined
    /// one, and a single reset closes them — which is what chroma's
    /// `terminal16m` formatter emits, verified by capturing the reference
    /// through a PTY.
    fn apply(&self, text: &str) -> String {
        if self.is_plain() || text.is_empty() {
            return text.to_string();
        }
        let mut out = String::new();
        if self.bold {
            out.push_str("\u{1b}[1m");
        }
        if self.underline {
            out.push_str("\u{1b}[4m");
        }
        if self.italic {
            out.push_str("\u{1b}[3m");
        }
        if let Some(rgb) = self.rgb {
            out.push_str(&format!(
                "\u{1b}[38;2;{};{};{}m",
                (rgb >> 16) & 0xFF,
                (rgb >> 8) & 0xFF,
                rgb & 0xFF
            ));
        }
        out.push_str(text);
        out.push_str("\u{1b}[0m");
        out
    }
}

/// The kinds of text that are coloured differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextType {
    /// A result-set value.
    Cell,
    /// A column heading.
    Header,
    /// The rule under the headings, and the column separator.
    Separator,
    /// A message of severity above 10.
    Error,
    /// A message of severity 10 or below, including `PRINT`.
    Warning,
}

impl TextType {
    fn index(self) -> usize {
        match self {
            TextType::Cell => 0,
            TextType::Header => 1,
            TextType::Separator => 2,
            TextType::Error => 3,
            TextType::Warning => 4,
        }
    }
}

/// A resolved scheme, or `None` when nothing should be coloured.
#[derive(Debug, Clone, Copy, Default)]
pub struct Colorizer {
    faces: Option<[Face; 5]>,
}

impl Colorizer {
    /// Resolves `scheme` against the destination.
    ///
    /// `to_terminal` says whether the stream this will be written to is a
    /// terminal. An unrecognised name is not an error: chroma hands back its
    /// own fallback style, so go-sqlcmd still colours the output.
    pub fn new(scheme: &str, to_terminal: bool) -> Self {
        if scheme.is_empty() || !to_terminal {
            return Colorizer { faces: None };
        }
        let find = |name: &str| {
            SCHEMES
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, faces)| *faces)
        };
        Colorizer {
            faces: find(scheme).or_else(|| find(FALLBACK_SCHEME)),
        }
    }

    /// Whether anything will actually be coloured.
    pub fn is_active(&self) -> bool {
        self.faces.is_some()
    }

    /// Colours `text` as `kind`, or returns it unchanged.
    pub fn paint(&self, text: &str, kind: TextType) -> String {
        match &self.faces {
            Some(faces) => faces[kind.index()].apply(text),
            None => text.to_string(),
        }
    }

    /// Colours each line of `text` separately, leaving the terminators outside
    /// the escapes. A multi-line message is written that way by go-sqlcmd, so
    /// a reset lands at the end of every line rather than once at the end.
    pub fn paint_lines(&self, text: &str, kind: TextType) -> String {
        if !self.is_active() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find(EOL) {
            let (line, tail) = rest.split_at(at);
            if !line.is_empty() {
                out.push_str(&self.paint(line, kind));
            }
            out.push_str(EOL);
            rest = &tail[EOL.len()..];
        }
        if !rest.is_empty() {
            out.push_str(&self.paint(rest, kind));
        }
        out
    }

    /// The scheme names, sorted, as `:list color` reports them.
    pub fn names() -> Vec<&'static str> {
        let mut names: Vec<&'static str> = SCHEMES.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names
    }
}

/// Whether the process's stdout is a terminal rather than a file or a pipe.
pub fn stdout_is_terminal() -> bool {
    #[cfg(windows)]
    {
        // `GetConsoleMode` succeeds only for a console handle. The signature
        // matches the one `session::console_mode` uses, since a second
        // declaration with a different one is refused.
        unsafe extern "system" {
            fn GetStdHandle(n: i32) -> isize;
            fn GetConsoleMode(handle: isize, mode: *mut u32) -> i32;
        }
        const STD_OUTPUT_HANDLE: i32 = -11;
        let mut mode = 0u32;
        // SAFETY: the handle belongs to the process and `mode` is a local whose
        // value is not read unless the call reports success.
        unsafe { GetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), &mut mode) != 0 }
    }
    #[cfg(not(windows))]
    {
        unsafe extern "C" {
            fn isatty(fd: i32) -> i32;
        }
        // SAFETY: `isatty` only inspects the descriptor.
        unsafe { isatty(1) == 1 }
    }
}
