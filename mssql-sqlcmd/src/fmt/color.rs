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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_scheme_colours_nothing() {
        let plain = Colorizer::new("", true);
        assert!(!plain.is_active());
        assert_eq!(plain.paint("x", TextType::Cell), "x");
    }

    #[test]
    fn a_redirected_stream_is_never_coloured() {
        // The whole point of the gate: a script capturing output must not find
        // escape sequences in it.
        let piped = Colorizer::new("monokai", false);
        assert!(!piped.is_active());
        assert_eq!(piped.paint("x", TextType::Cell), "x");
    }

    #[test]
    fn a_scheme_that_does_not_exist_falls_back_rather_than_failing() {
        // chroma answers an unknown name with its `swapoff` fallback, so the
        // reference still colours. Measured through a PTY with
        // SQLCMDCOLORSCHEME=nosuchscheme.
        let unknown = Colorizer::new("no-such-scheme", true);
        assert!(unknown.is_active());
        assert_eq!(
            unknown.paint("hello", TextType::Warning),
            "\u{1b}[38;2;229;229;229mhello\u{1b}[0m"
        );
        assert_eq!(
            unknown.paint("hello", TextType::Warning),
            Colorizer::new(FALLBACK_SCHEME, true).paint("hello", TextType::Warning)
        );
    }

    #[test]
    fn a_multi_line_message_resets_at_the_end_of_every_line() {
        // A `Msg ...` header and its text arrive as one string, and the
        // reference closes the escape on each line rather than once at the end.
        let monokai = Colorizer::new("monokai", true);
        assert_eq!(
            monokai.paint_lines(
                &format!("Msg 208{EOL}Invalid object name.{EOL}"),
                TextType::Error
            ),
            format!(
                "\u{1b}[38;2;248;248;242mMsg 208\u{1b}[0m{EOL}\
                 \u{1b}[38;2;248;248;242mInvalid object name.\u{1b}[0m{EOL}"
            )
        );
    }

    #[test]
    fn a_blank_line_inside_a_message_is_left_alone() {
        let monokai = Colorizer::new("monokai", true);
        assert_eq!(
            monokai.paint_lines(&format!("boom{EOL}{EOL}"), TextType::Error),
            format!("\u{1b}[38;2;248;248;242mboom\u{1b}[0m{EOL}{EOL}")
        );
    }

    #[test]
    fn a_known_scheme_emits_true_colour() {
        let monokai = Colorizer::new("monokai", true);
        assert!(monokai.is_active());
        // Measured from the reference through a PTY: monokai draws a value in
        // #e6db74, its LiteralString colour.
        assert_eq!(
            monokai.paint("x", TextType::Cell),
            "\u{1b}[38;2;230;219;116mx\u{1b}[0m"
        );
    }

    #[test]
    fn the_measured_faces_match_the_reference() {
        // Every one of these came out of a PTY capture of go-sqlcmd with
        // SQLCMDCOLORSCHEME=monokai.
        let monokai = Colorizer::new("monokai", true);
        assert_eq!(
            monokai.paint("a", TextType::Header),
            "\u{1b}[38;2;248;248;242ma\u{1b}[0m"
        );
        assert_eq!(
            monokai.paint("-", TextType::Separator),
            "\u{1b}[38;2;230;219;116m-\u{1b}[0m"
        );
        // Not #960050: `GenericError` does not inherit monokai's `Error` entry.
        assert_eq!(
            monokai.paint("Msg 208", TextType::Error),
            "\u{1b}[38;2;248;248;242mMsg 208\u{1b}[0m"
        );
        // Italic and colour arrive as two sequences, emphasis first.
        assert_eq!(
            monokai.paint("hello", TextType::Warning),
            "\u{1b}[3m\u{1b}[38;2;248;248;242mhello\u{1b}[0m"
        );
    }

    #[test]
    fn each_text_type_is_drawn_from_its_own_entry() {
        let monokai = Colorizer::new("monokai", true);
        let cell = monokai.paint("x", TextType::Cell);
        let error = monokai.paint("x", TextType::Error);
        assert_ne!(cell, error, "an error should not look like a value");
    }

    #[test]
    fn emphasis_flags_come_through() {
        // algol draws strings italic rather than coloured.
        let algol = Colorizer::new("algol", true);
        assert!(algol.paint("x", TextType::Cell).starts_with("\u{1b}[3m"));
    }

    #[test]
    fn empty_text_is_left_alone() {
        // Wrapping nothing in escapes would pad the output with invisible
        // characters that still count against a column's width.
        assert_eq!(
            Colorizer::new("monokai", true).paint("", TextType::Cell),
            ""
        );
    }

    #[test]
    fn every_scheme_the_reference_knows_is_present() {
        let names = Colorizer::names();
        assert_eq!(names.len(), 74, "chroma v2.27.0 ships 74 styles");
        for expected in ["monokai", "github", "vim", "emacs", "native", "friendly"] {
            assert!(names.contains(&expected), "{expected} is missing");
        }
    }

    #[test]
    fn names_are_sorted() {
        let names = Colorizer::names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }
}
