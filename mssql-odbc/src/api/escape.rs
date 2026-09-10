// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ODBC escape-sequence translation (AB#46384).
//!
//! ODBC lets applications write vendor-neutral constructs in braces — `{fn …}`,
//! `{d …}`, `{t …}`, `{ts …}`, `{oj …}`, `{guid …}`, `{escape …}`,
//! `{interval …}`, `{encrypt …}` and `{call …}` — which the driver is expected
//! to translate before sending, unless `SQL_ATTR_NOSCAN` is on.
//!
//! Most of them need no translation at all against SQL Server. The server
//! parses `{fn}`, `{d}`, `{t}`, `{ts}`, `{oj}`, `{escape}` and `{guid}`
//! natively, and msodbcsql passes them through untouched — `ProcessDTI`
//! short-circuits with `*pfPassthru = TRUE` for the datetime family
//! (`sqlcmisc.cpp:7892`, "Sphinx has canonical datetime support"), and the
//! `ECODE_FUNCTION` / `ECODE_OUTERJOIN` / `ECODE_GUID` arms of
//! `SubstituteECodes` do the same (`sqlcmisc.cpp:4715`, `:4757`, `:4780`). Only
//! four constructs are genuinely rewritten: `{escape}`, `{interval}`,
//! `{encrypt}` and `{call}`.
//!
//! Translation deliberately does **not** rewrite `?` parameter markers. That is
//! a separate phase ([`super::util::rewrite_param_markers`]) which only the
//! execution path runs, because `SQLNativeSql` must return the markers intact —
//! msodbcsql answers `{? = call sp_who(?)}` with ` EXEC ?=sp_who ?  `. Both
//! phases walk the same lexer ([`CodeScan`]) so they cannot disagree about what
//! counts as a comment or a literal.
//!
//! See `mssql-odbc/docs/odbc-escape-sequences-plan.md` for the measured
//! msodbcsql behaviour this file reproduces.

use std::fmt;

use super::sqlstate::{SQLSTATE_22001, SQLSTATE_22018, SQLSTATE_42000};

/// A malformed or unrecognised escape sequence.
///
/// Most of these are msodbcsql's `IDS_37_000`, which surfaces as SQLSTATE
/// `42000` ("Syntax error, permission violation, or other nonspecific error").
/// `{interval …}` is the exception: it runs through the type converter rather
/// than the escape parser, so a bad value reports `22018` and a fractional
/// second that does not fit the declared scale reports `22001` — both measured
/// against msodbcsql 18.6.2.1. In every case the statement is not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EscapeError {
    state: [u8; 5],
    message: String,
}

impl EscapeError {
    /// SQLSTATE `42000` — the syntax error every escape but `{interval}` uses.
    fn syntax(message: impl Into<String>) -> Self {
        Self {
            state: SQLSTATE_42000,
            message: message.into(),
        }
    }

    /// SQLSTATE `22018` — `CVT_ERROR` out of `ParseInterval`.
    fn interval_value(message: impl Into<String>) -> Self {
        Self {
            state: SQLSTATE_22018,
            message: message.into(),
        }
    }

    /// SQLSTATE `22001` — `CVT_FRACT_TRUNC` promoted from `01S07` for an
    /// ODBC 3.x application (`ProcessDTI`, `sqlcmisc.cpp:7945`).
    fn interval_truncation(message: impl Into<String>) -> Self {
        Self {
            state: SQLSTATE_22001,
            message: message.into(),
        }
    }

    pub(crate) fn state(&self) -> [u8; 5] {
        self.state
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for EscapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// One argument of a canonical `{call …}` escape.
///
/// Each variant keeps the argument's original text, trimmed. msodbcsql writes
/// arguments through verbatim when it builds the `EXEC` form — `{call p(@a = ?)}`
/// comes back as `EXEC p @a = ?`, spaces and all — so re-rendering from the
/// parsed pieces would diverge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallArg {
    /// A `?` parameter marker, optionally named as `@name = ?`.
    Marker { name: Option<String>, text: String },
    /// An omitted argument (`,,`) or a literal `DEFAULT`.
    Default,
    /// Anything else — a literal, an expression, or a nested escape. These
    /// force the textual `EXEC` path because there is no bound parameter to
    /// carry them over RPC.
    Text(String),
}

/// A canonical procedure call recovered from `{[? =] call name(args…)}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallSite {
    /// The procedure name exactly as written, already validated as a
    /// multi-part identifier by [`validate_procedure_name`].
    pub(crate) proc_name: String,
    /// True for the `{? = call …}` form, where the first bound parameter
    /// receives the procedure's return status.
    pub(crate) returns_status: bool,
    pub(crate) args: Vec<CallArg>,
}

impl CallSite {
    /// True when every argument can be carried as an RPC parameter, i.e. the
    /// call needs no `EXEC` text at all.
    pub(crate) fn is_rpc_eligible(&self) -> bool {
        self.args
            .iter()
            .all(|a| matches!(a, CallArg::Marker { .. } | CallArg::Default))
    }
}

/// Result of phase 1 — escape translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Translated {
    /// The statement with every escape translated. `?` markers are untouched.
    pub(crate) sql: String,
    /// Set when the whole statement is a single canonical call, which is the
    /// only shape the RPC path accepts.
    pub(crate) call: Option<CallSite>,
}

/// Walks SQL text and reports which characters are *code*, i.e. outside string
/// literals, quoted and bracketed identifiers, and comments.
///
/// This is the lexer `rewrite_param_markers` has always used, lifted out so
/// escape translation and marker rewriting share one definition of "inside a
/// comment". It keeps msodbcsql's two deliberate quirks:
///
/// - **Block comments do not nest** — the first `*/` closes.
/// - **`--(* … *)--` is not a comment** — a `--` that opens `--(*`, or that
///   immediately follows `*)`, stays code. A shared consequence is that
///   `COUNT(*)--…` is not treated as a line comment.
pub(crate) struct CodeScan<'a> {
    iter: std::iter::Peekable<std::str::CharIndices<'a>>,
    state: State,
    prev1: Option<char>,
    prev2: Option<char>,
}

#[derive(PartialEq, Clone, Copy)]
enum State {
    Normal,
    SingleQuote,
    DoubleQuote,
    Bracket,
    LineComment,
    BlockComment,
}

/// One step of [`CodeScan`]: the byte range consumed, and whether it was code.
pub(crate) struct Step {
    /// Byte offset of the first character consumed.
    pub(crate) start: usize,
    /// Byte offset one past the last character consumed.
    pub(crate) end: usize,
    /// The first character consumed.
    pub(crate) ch: char,
    /// True when `ch` was outside every literal and comment.
    pub(crate) code: bool,
}

impl<'a> CodeScan<'a> {
    pub(crate) fn new(src: &'a str) -> Self {
        Self {
            iter: src.char_indices().peekable(),
            state: State::Normal,
            prev1: None,
            prev2: None,
        }
    }

    fn peek_char(&mut self) -> Option<char> {
        self.iter.peek().map(|&(_, c)| c)
    }

    /// Consumes one lexical step. Returns `None` at end of input.
    pub(crate) fn next_step(&mut self) -> Option<Step> {
        let (start, c) = self.iter.next()?;
        let mut end = start + c.len_utf8();
        let code = self.state == State::Normal;

        match self.state {
            State::Normal => match c {
                '\'' => self.state = State::SingleQuote,
                '"' => self.state = State::DoubleQuote,
                '[' => self.state = State::Bracket,
                '-' if self.peek_char() == Some('-') => {
                    let mut lookahead = self.iter.clone();
                    lookahead.next();
                    let starts_canonical_extension = matches!(
                        (
                            lookahead.next().map(|(_, c)| c),
                            lookahead.next().map(|(_, c)| c)
                        ),
                        (Some('('), Some('*'))
                    );
                    let ends_canonical_extension =
                        matches!(self.prev2, Some('*')) && matches!(self.prev1, Some(')'));
                    if !starts_canonical_extension && !ends_canonical_extension {
                        if let Some((i, n)) = self.iter.next() {
                            end = i + n.len_utf8();
                        }
                        self.state = State::LineComment;
                    }
                }
                '/' if self.peek_char() == Some('*') => {
                    if let Some((i, n)) = self.iter.next() {
                        end = i + n.len_utf8();
                    }
                    self.state = State::BlockComment;
                }
                _ => {}
            },
            State::SingleQuote | State::DoubleQuote | State::Bracket => {
                let closer = match self.state {
                    State::SingleQuote => '\'',
                    State::DoubleQuote => '"',
                    _ => ']',
                };
                if c == closer {
                    // A doubled delimiter is an escaped one, not the end.
                    if self.peek_char() == Some(closer) {
                        if let Some((i, n)) = self.iter.next() {
                            end = i + n.len_utf8();
                        }
                    } else {
                        self.state = State::Normal;
                    }
                }
            }
            State::LineComment => {
                if c == '\n' || c == '\r' {
                    self.state = State::Normal;
                }
            }
            State::BlockComment => {
                if c == '*' && self.peek_char() == Some('/') {
                    if let Some((i, n)) = self.iter.next() {
                        end = i + n.len_utf8();
                    }
                    self.state = State::Normal;
                }
            }
        }

        self.prev2 = self.prev1;
        self.prev1 = Some(c);
        Some(Step {
            start,
            end,
            ch: c,
            code,
        })
    }
}

/// Finds the next escape sequence at or after `from`, returning the byte range
/// of the whole `{…}` including both braces.
///
/// An unmatched `{` is not an error: msodbcsql's `FindECode`
/// (`sqlcmisc.cpp:4962`) reports "no escape found" when it cannot pair the
/// braces, and the text goes to the server as written.
fn find_escape(sql: &str, from: usize) -> Option<(usize, usize)> {
    let mut scan = CodeScan::new(&sql[from..]);
    let mut open: Option<usize> = None;
    let mut depth = 0usize;
    while let Some(step) = scan.next_step() {
        if !step.code {
            continue;
        }
        match step.ch {
            '{' => {
                if open.is_none() {
                    open = Some(from + step.start);
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some((open?, from + step.end));
                }
            }
            _ => {}
        }
    }
    None
}

/// The escape kinds this driver recognises, keyed the way msodbcsql's
/// `ParseECodeType` keys them (`sqlcmisc.cpp:7784`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeKind {
    /// `{fn …}` — scalar function; SQL Server parses it natively.
    Fn,
    /// `{d …}` / `{t …}` / `{ts …}` / `{guid …}` / `{oj …}` — all native.
    PassThrough,
    Escape,
    Interval,
    Encrypt,
    Call {
        returns_status: bool,
    },
}

/// Splits an escape body into its leading tag and the remainder.
///
/// Returns `None` when the body has no tag at all (`{}` or `{   }`).
fn split_tag(body: &str) -> Option<(&str, &str)> {
    let trimmed = body.trim_start();
    let lead = body.len() - trimmed.len();
    let tag_len = trimmed
        .find(|c: char| c.is_whitespace() || c == '(' || c == '\'' || c == '"' || c == '{')
        .unwrap_or(trimmed.len());
    if tag_len == 0 {
        // A body that starts with a delimiter has no identifier tag, except the
        // `?=call` form which starts with `?`.
        return None;
    }
    Some((&trimmed[..tag_len], &body[lead + tag_len..]))
}

/// Classifies an escape body by its tag.
fn classify(body: &str) -> Result<(EscapeKind, &str), EscapeError> {
    let trimmed = body.trim_start();

    // `{? = call proc(…)}` — the only form whose tag is not an identifier.
    if let Some(rest) = trimmed.strip_prefix('?') {
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('=').ok_or_else(|| {
            EscapeError::syntax("Malformed ODBC escape: expected '=' after '?' in {? = call ...}")
        })?;
        let rest = rest.trim_start();
        let (tag, remainder) = split_tag(rest).ok_or_else(|| {
            EscapeError::syntax("Malformed ODBC escape: expected 'call' after '?=' ")
        })?;
        if !tag.eq_ignore_ascii_case("call") {
            return Err(EscapeError::syntax(format!(
                "Malformed ODBC escape: expected 'call' after '?=', found '{tag}'"
            )));
        }
        return Ok((
            EscapeKind::Call {
                returns_status: true,
            },
            remainder,
        ));
    }

    let (tag, remainder) = split_tag(body)
        .ok_or_else(|| EscapeError::syntax("Malformed ODBC escape: the sequence has no keyword"))?;

    let kind = match tag.len() {
        1 if tag.eq_ignore_ascii_case("d") || tag.eq_ignore_ascii_case("t") => {
            EscapeKind::PassThrough
        }
        2 if tag.eq_ignore_ascii_case("fn") => EscapeKind::Fn,
        2 if tag.eq_ignore_ascii_case("oj") || tag.eq_ignore_ascii_case("ts") => {
            EscapeKind::PassThrough
        }
        4 if tag.eq_ignore_ascii_case("call") => EscapeKind::Call {
            returns_status: false,
        },
        4 if tag.eq_ignore_ascii_case("guid") => EscapeKind::PassThrough,
        6 if tag.eq_ignore_ascii_case("escape") => EscapeKind::Escape,
        7 if tag.eq_ignore_ascii_case("encrypt") => EscapeKind::Encrypt,
        8 if tag.eq_ignore_ascii_case("interval") => EscapeKind::Interval,
        _ => {
            return Err(EscapeError::syntax(format!(
                "Unrecognized ODBC escape sequence '{tag}'"
            )));
        }
    };
    Ok((kind, remainder))
}

/// Validates escapes nested inside another escape's body.
///
/// msodbcsql processes the innermost escape first, so a malformed nested
/// sequence fails the whole statement even when the outer one is passed
/// through. `{fn …}` inside a canonical call is rejected outright
/// (`SubstituteECodes`, `sqlcmisc.cpp:4712` — `ECODE_FUNCTION` with
/// `fNestedInCall` posts `IDS_37_000`).
fn validate_nested(body: &str, inside_call: bool) -> Result<(), EscapeError> {
    let mut at = 0usize;
    while let Some((open, close)) = find_escape(body, at) {
        let inner = &body[open + 1..close - 1];
        let (kind, _) = classify(inner)?;
        if inside_call && kind == EscapeKind::Fn {
            return Err(EscapeError::syntax(
                "The {fn ...} escape is not allowed inside a canonical procedure call",
            ));
        }
        validate_nested(inner, inside_call)?;
        at = close;
    }
    Ok(())
}

/// Phase 1 — translate ODBC escape sequences, leaving `?` markers alone.
pub(crate) fn translate_escapes(sql: &str) -> Result<Translated, EscapeError> {
    let mut out = String::with_capacity(sql.len() + 16);
    let mut at = 0usize;
    let mut only_call: Option<CallSite> = None;
    let mut escape_count = 0usize;
    let mut outside_is_blank = true;

    while let Some((open, close)) = find_escape(sql, at) {
        let before = &sql[at..open];
        out.push_str(before);
        if !is_blank_or_separator(before) {
            outside_is_blank = false;
        }

        let body = &sql[open + 1..close - 1];
        let (kind, remainder) = classify(body)?;
        escape_count += 1;

        match kind {
            EscapeKind::Fn | EscapeKind::PassThrough => {
                validate_nested(body, false)?;
                // Native to SQL Server: emit the escape exactly as written.
                out.push_str(&sql[open..close]);
            }
            EscapeKind::Escape => {
                push_translated(&mut out, &translate_escape_clause(remainder));
            }
            EscapeKind::Interval => {
                push_translated(&mut out, &translate_interval(body)?);
            }
            EscapeKind::Encrypt => {
                push_translated(&mut out, &translate_encrypt(remainder)?);
            }
            EscapeKind::Call { returns_status } => {
                validate_nested(remainder, true)?;
                let call = parse_call(remainder, returns_status)?;
                push_translated(&mut out, &call_to_exec_text(&call));
                if escape_count == 1 {
                    only_call = Some(call);
                }
            }
        }
        at = close;
    }

    let tail = &sql[at..];
    out.push_str(tail);
    if !is_blank_or_separator(tail) {
        outside_is_blank = false;
    }

    // The RPC path takes only the strict single-call shape: one canonical call
    // and nothing else but whitespace and an optional statement separator.
    let call = match only_call {
        Some(call) if escape_count == 1 && outside_is_blank => Some(call),
        _ => None,
    };

    Ok(Translated { sql: out, call })
}

/// Runs both phases for the execution path: escape translation (unless
/// `SQL_ATTR_NOSCAN` is on) followed by `?` -> `@P1..@Pn` rewriting.
///
/// With `NOSCAN` on the text goes to the server as written, which is what the
/// attribute is for; the recovered call site is dropped with it, so `{call ...}`
/// is no longer special-cased either. That matches msodbcsql, where
/// `DoSubstitutions` skips `SubstituteECodes` entirely and nothing sets
/// `CANONICAL_CALL` (`sqlcmisc.cpp:4553-4566`).
pub(crate) fn translate_and_rewrite(
    sql: &str,
    noscan: bool,
) -> Result<(String, usize, Option<CallSite>), EscapeError> {
    if noscan {
        let (rewritten, count) = super::util::rewrite_param_markers(sql);
        return Ok((rewritten, count, None));
    }
    let translated = translate_escapes(sql)?;
    let (rewritten, count) = super::util::rewrite_param_markers(&translated.sql);
    Ok((rewritten, count, translated.call))
}

/// msodbcsql surrounds every *translated* escape with a space on each side —
/// `WriteCharToExtBuffer(… L' ' …)` before the replacement text
/// (`sqlcmisc.cpp:4653`) and again before splicing it in (`:4851`). Passed-
/// through escapes get no such padding.
fn push_translated(out: &mut String, text: &str) {
    out.push(' ');
    out.push_str(text);
    out.push(' ');
}

fn is_blank_or_separator(text: &str) -> bool {
    text.chars().all(|c| c.is_whitespace() || c == ';')
}

/// `{escape 'c'}` → `ESCAPE 'c'`.
///
/// `ProcessEscape` (`sqlcmisc.cpp:8654`) writes the six characters `ESCAPE`
/// and then the body after the tag verbatim, so the space in `{escape '\'}`
/// comes from the original text.
fn translate_escape_clause(remainder: &str) -> String {
    format!("ESCAPE{remainder}")
}

/// `{encrypt N'…'}` → `0x…`.
///
/// `ProcessEncrypt` (`sqlcmisc.cpp:8673`) requires the `N` prefix, applies the
/// TDS LOGIN7 password scrambler to the UTF-16LE bytes of the literal, and
/// emits uppercase hex.
fn translate_encrypt(remainder: &str) -> Result<String, EscapeError> {
    let rest = remainder.trim_start();
    let rest = rest
        .strip_prefix('N')
        .or_else(|| rest.strip_prefix('n'))
        .ok_or_else(|| {
            EscapeError::syntax(
                "Malformed {encrypt ...} escape: the literal must be prefixed with N",
            )
        })?;
    let rest = rest.strip_prefix('\'').ok_or_else(|| {
        EscapeError::syntax("Malformed {encrypt ...} escape: expected a quoted literal")
    })?;

    let mut literal = String::new();
    let mut chars = rest.chars().peekable();
    let mut closed = false;
    while let Some(c) = chars.next() {
        if c == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                literal.push('\'');
            } else {
                closed = true;
                break;
            }
        } else {
            literal.push(c);
        }
    }
    if !closed {
        return Err(EscapeError::syntax(
            "Malformed {encrypt ...} escape: unterminated literal",
        ));
    }
    if chars.any(|c| !c.is_whitespace()) {
        return Err(EscapeError::syntax(
            "Malformed {encrypt ...} escape: unexpected text after the literal",
        ));
    }

    let mut hex = String::from("0x");
    for unit in literal.encode_utf16() {
        for byte in unit.to_le_bytes() {
            hex.push_str(&format!("{:02X}", scramble_login_byte(byte)));
        }
    }
    Ok(hex)
}

/// The TDS LOGIN7 password scrambler (`EncryptPWD`, `TdsParser.h:5031`).
///
/// This is obfuscation, not encryption: it is a fixed nibble swap and XOR with
/// no key, and provides no confidentiality. It exists here only so `{encrypt}`
/// matches msodbcsql byte for byte.
fn scramble_login_byte(b: u8) -> u8 {
    (((b & 0x0f) << 4) | (b >> 4)) ^ 0xa5
}

/// Builds the text `sp_describe_undeclared_parameters` should be asked about.
///
/// Describe has to translate escapes even when `SQL_ATTR_NOSCAN` is on, because
/// the server metadata RPC cannot parse `{call ...}` at all — msodbcsql does
/// the same, calling `DoSubstitutions` with no statement handle before it asks
/// (`AutoFillIPD`, `sqlcdesc.cpp:9355`).
///
/// The `{? = call ...}` form needs one extra step: the return-status marker has
/// no place in `EXEC`, and `EXEC ?=proc ?` is a syntax error to the metadata
/// RPC (measured). It is dropped here and described by the caller as an
/// `SQL_INTEGER`, which is what msodbcsql reports for it.
///
/// Returns the text to describe and whether a return-status parameter was
/// dropped from the front.
pub(crate) fn describe_text(original_sql: &str) -> Result<(String, bool), EscapeError> {
    let translated = translate_escapes(original_sql)?;
    match translated.call.as_ref().filter(|c| c.returns_status) {
        Some(call) => {
            let without_status = CallSite {
                proc_name: call.proc_name.clone(),
                returns_status: false,
                args: call.args.clone(),
            };
            let text = format!(" {} ", call_to_exec_text(&without_status));
            Ok((super::util::rewrite_param_markers(&text).0, true))
        }
        None => Ok((super::util::rewrite_param_markers(&translated.sql).0, false)),
    }
}

/// Renders a parsed call as the textual `EXEC` form msodbcsql produces when it
/// cannot use RPC (`ProcessCanonicalCall`, `sqlcmisc.cpp:7952`).
///
/// The argument list is comma-separated with no spaces, and the final separator
/// becomes a space — msodbcsql overwrites it in place (`sqlcmisc.cpp:8560`).
fn call_to_exec_text(call: &CallSite) -> String {
    let mut text = String::from("EXEC ");
    if call.returns_status {
        text.push_str("?=");
    }
    text.push_str(&call.proc_name);
    text.push(' ');
    for (i, arg) in call.args.iter().enumerate() {
        if i > 0 {
            text.push(',');
        }
        match arg {
            CallArg::Marker { text: t, .. } | CallArg::Text(t) => text.push_str(t),
            CallArg::Default => text.push_str("DEFAULT"),
        }
    }
    // msodbcsql writes "arg," per argument and then overwrites the final comma
    // with a space (sqlcmisc.cpp:8560), so an argument list ends in a space and
    // an empty one does not.
    if !call.args.is_empty() {
        text.push(' ');
    }
    text
}

/// Parses `proc[;n] [(arg[,arg…])]` — everything after the `call` keyword.
fn parse_call(remainder: &str, returns_status: bool) -> Result<CallSite, EscapeError> {
    let rest = remainder.trim_start();
    // Scanned with delimiter awareness rather than "up to the first space", so
    // a quoted name keeps its spaces: msodbcsql answers {call [my proc](?)}
    // with EXEC [my proc] ?.
    let (raw_name, rest) = take_proc_name(rest).ok_or_else(|| {
        EscapeError::syntax("Malformed {call ...} escape: the procedure name is missing")
    })?;
    let proc_name = validate_procedure_name(raw_name)?;

    let rest = rest.trim_start();
    if rest.is_empty() {
        return Ok(CallSite {
            proc_name,
            returns_status,
            args: Vec::new(),
        });
    }

    let inner = rest
        .strip_prefix('(')
        .ok_or_else(|| {
            EscapeError::syntax(
                "Malformed {call ...} escape: expected '(' or end of sequence after the \
                 procedure name",
            )
        })?
        .trim_end();
    let inner = inner.strip_suffix(')').ok_or_else(|| {
        EscapeError::syntax("Malformed {call ...} escape: the argument list is not closed")
    })?;

    let args = split_call_args(inner)?
        .into_iter()
        .map(|a| classify_call_arg(&a))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CallSite {
        proc_name,
        returns_status,
        args,
    })
}

/// Splits a call's argument list on top-level commas, honouring literals,
/// comments, parentheses and nested escapes.
fn split_call_args(inner: &str) -> Result<Vec<String>, EscapeError> {
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut args = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut scan = CodeScan::new(inner);
    while let Some(step) = scan.next_step() {
        let text = &inner[step.start..step.end];
        if step.code {
            match step.ch {
                '(' | '{' => depth += 1,
                ')' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    args.push(std::mem::take(&mut current));
                    continue;
                }
                _ => {}
            }
        }
        current.push_str(text);
    }
    if depth != 0 {
        return Err(EscapeError::syntax(
            "Malformed {call ...} escape: unbalanced parentheses in the argument list",
        ));
    }
    args.push(current);
    Ok(args)
}

/// Classifies one call argument.
///
/// An empty argument and a literal `DEFAULT` are the same thing to SQL Server
/// (`ProcessCanonicalCall` writes `DEFAULT,` for both, `sqlcmisc.cpp:8430`).
fn classify_call_arg(arg: &str) -> Result<CallArg, EscapeError> {
    let trimmed = arg.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("DEFAULT") {
        return Ok(CallArg::Default);
    }
    if trimmed == "?" {
        return Ok(CallArg::Marker {
            name: None,
            text: trimmed.to_string(),
        });
    }
    // `@name = ?` — a named parameter binding.
    if let Some(rest) = trimmed.strip_prefix('@')
        && let Some((name, value)) = rest.split_once('=')
    {
        let name = name.trim();
        if value.trim() == "?" && !name.is_empty() && is_regular_identifier(name) {
            return Ok(CallArg::Marker {
                name: Some(format!("@{name}")),
                text: trimmed.to_string(),
            });
        }
    }
    Ok(CallArg::Text(trimmed.to_string()))
}

/// Validates a procedure name taken from application SQL.
///
/// `{call}` is the first path that lets an application put arbitrary text where
/// a procedure name goes, and that name reaches `execute_stored_procedure` and,
/// on Always Encrypted connections, a T-SQL `EXEC` string. Only a multi-part
/// identifier is accepted — each part either a regular identifier or a
/// `[bracketed]` / `"quoted"` one — plus msodbcsql's optional `;n` procedure
/// group number (`sqlcmisc.cpp:8215`). Anything else is a syntax error, so no
/// separator, comment or statement terminator can ride along.
pub(crate) fn validate_procedure_name(raw: &str) -> Result<String, EscapeError> {
    let invalid = || EscapeError::syntax(format!("Invalid procedure name '{raw}' in {{call ...}}"));

    let (name, group) = match raw.split_once(';') {
        Some((name, group)) => {
            if group.is_empty() || !group.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid());
            }
            (name, Some(group))
        }
        None => (raw, None),
    };
    if name.is_empty() {
        return Err(invalid());
    }

    let mut parts = 0usize;
    let mut rest = name;
    loop {
        let (part, remainder) = take_identifier(rest).ok_or_else(invalid)?;
        if part.is_empty() && parts == 0 {
            return Err(invalid());
        }
        parts += 1;
        match remainder.strip_prefix('.') {
            Some(next) => rest = next,
            None => {
                if !remainder.is_empty() {
                    return Err(invalid());
                }
                break;
            }
        }
    }
    // server.database.schema.procedure is the longest legal form.
    if parts > 4 {
        return Err(invalid());
    }

    Ok(match group {
        Some(group) => format!("{name};{group}"),
        None => name.to_string(),
    })
}

/// Takes one identifier — regular, `[bracketed]`, or `"quoted"` — off the front.
fn take_identifier(text: &str) -> Option<(&str, &str)> {
    let mut chars = text.char_indices();
    match chars.next() {
        Some((_, '[')) => {
            let mut rest = &text[1..];
            let mut consumed = 1usize;
            loop {
                let close = rest.find(']')?;
                consumed += close + 1;
                if rest[close + 1..].starts_with(']') {
                    consumed += 1;
                    rest = &rest[close + 2..];
                } else {
                    return Some((&text[..consumed], &text[consumed..]));
                }
            }
        }
        Some((_, '"')) => {
            let mut rest = &text[1..];
            let mut consumed = 1usize;
            loop {
                let close = rest.find('"')?;
                consumed += close + 1;
                if rest[close + 1..].starts_with('"') {
                    consumed += 1;
                    rest = &rest[close + 2..];
                } else {
                    return Some((&text[..consumed], &text[consumed..]));
                }
            }
        }
        Some((_, c)) if is_identifier_start(c) => {
            let end = text
                .find(|c: char| !is_identifier_part(c))
                .unwrap_or(text.len());
            Some((&text[..end], &text[end..]))
        }
        // An empty part is legal in the middle of a qualified name (`db..proc`).
        Some((_, '.')) => Some((&text[..0], text)),
        _ => None,
    }
}

fn is_identifier_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '#' || c == '@'
}

fn is_identifier_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '#' || c == '@' || c == '$'
}

fn is_regular_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if is_identifier_start(c)) && chars.all(is_identifier_part)
}

/// Takes a whole `[db.][schema.]name[;n]` off the front of a `{call …}` body.
///
/// Delimited parts may contain spaces and parentheses, so the extent cannot be
/// found by searching for the first space.
fn take_proc_name(text: &str) -> Option<(&str, &str)> {
    let mut rest = text;
    let mut consumed = 0usize;
    loop {
        let (part, remainder) = take_identifier(rest)?;
        consumed += part.len();
        rest = remainder;
        match rest.strip_prefix('.') {
            Some(next) => {
                consumed += 1;
                rest = next;
            }
            None => break,
        }
    }
    if consumed == 0 {
        return None;
    }
    // Optional procedure group number (`proc;2`, sqlcmisc.cpp:8215).
    if let Some(after) = rest.strip_prefix(';') {
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 {
            consumed += 1 + digits;
            rest = &after[digits..];
        }
    }
    Some((&text[..consumed], rest))
}

// ---------------------------------------------------------------------------
// {interval ...}
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntervalField {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

impl IntervalField {
    fn parse(word: &str) -> Option<Self> {
        Some(match word.to_ascii_uppercase().as_str() {
            "YEAR" => Self::Year,
            "MONTH" => Self::Month,
            "DAY" => Self::Day,
            "HOUR" => Self::Hour,
            "MINUTE" => Self::Minute,
            "SECOND" => Self::Second,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Year => "YEAR",
            Self::Month => "MONTH",
            Self::Day => "DAY",
            Self::Hour => "HOUR",
            Self::Minute => "MINUTE",
            Self::Second => "SECOND",
        }
    }
}

const MAX_INTERVAL_PRECISION: u32 = 9;
const DEFAULT_INTERVAL_PRECISION: u32 = 2;
const DEFAULT_INTERVAL_SCALE: u32 = 6;

/// `{interval …}` → a T-SQL string literal.
///
/// The server has no interval type, so msodbcsql emits the ODBC interval
/// literal *as a quoted string*: `{interval '1' DAY}` becomes
/// `'INTERVAL +''1'' DAY(2)'`. The format strings are taken verbatim from
/// `sqlcstr.cpp:383-395` and the value is normalised through
/// `ParseInterval` / `ConvertToInterval` (`sqlccnvt.cpp:6489`, `:2726`).
///
/// Returns the replacement text and whether fractional seconds were truncated.
fn translate_interval(body: &str) -> Result<String, EscapeError> {
    let malformed = || {
        EscapeError::interval_value(format!(
            "Invalid interval value in ODBC escape '{{{body}}}'"
        ))
    };

    let rest = body.trim_start();
    let rest = rest
        .get(..8)
        .filter(|w| w.eq_ignore_ascii_case("interval"))
        .map(|_| &rest[8..])
        .ok_or_else(malformed)?;

    let rest = rest.trim_start();
    let (negative, rest) = match rest.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, rest.strip_prefix('+').unwrap_or(rest)),
    };

    let rest = rest.trim_start();
    let rest = rest.strip_prefix('\'').ok_or_else(malformed)?;
    let (value, rest) = rest.split_once('\'').ok_or_else(malformed)?;

    let qualifier = parse_interval_qualifier(rest).ok_or_else(malformed)?;
    let IntervalQualifier {
        leading,
        trailing,
        precision,
        scale,
    } = qualifier;

    if precision > MAX_INTERVAL_PRECISION || scale > MAX_INTERVAL_PRECISION {
        return Err(malformed());
    }

    let parts = parse_interval_value(value.trim(), leading, trailing).ok_or_else(malformed)?;

    // Leading-field overflow: msodbcsql rejects a value that does not fit the
    // declared precision (`sqlccnvt.cpp:6741`).
    if parts.leading >= 10u64.pow(precision) {
        return Err(malformed());
    }
    if trailing == Some(IntervalField::Month) && parts.month > 11 {
        return Err(malformed());
    }
    if parts.hour > 23 || parts.minute > 59 || parts.second > 59 {
        return Err(malformed());
    }

    // Digits beyond the declared scale are a hard error for an ODBC 3.x
    // application: ProcessDTI promotes CVT_FRACT_TRUNC from 01S07 to 22001
    // (sqlcmisc.cpp:7945). Measured: {interval '30.1234' SECOND(2,3)} -> 22001.
    let divisor = 10u32.pow(MAX_INTERVAL_PRECISION - scale);
    if parts.fraction % divisor != 0 {
        return Err(EscapeError::interval_truncation(format!(
            "Fractional seconds in ODBC escape '{{{body}}}' exceed the declared scale of {scale}"
        )));
    }
    let fraction = if scale == 0 {
        String::new()
    } else {
        format!(".{:09}", parts.fraction)[..=scale as usize].to_string()
    };

    let sign = if negative { '-' } else { '+' };
    let text = match (leading, trailing) {
        (f, None) if f != IntervalField::Second => format!(
            "'INTERVAL {sign}''{}'' {}({precision})'",
            parts.leading,
            f.name()
        ),
        (IntervalField::Second, None) => format!(
            "'INTERVAL {sign}''{}{fraction}'' SECOND({precision},{scale})'",
            parts.leading
        ),
        (IntervalField::Year, Some(IntervalField::Month)) => format!(
            "'INTERVAL {sign}''{}-{:02}'' YEAR({precision}) TO MONTH'",
            parts.leading, parts.month
        ),
        (IntervalField::Day, Some(IntervalField::Hour)) => format!(
            "'INTERVAL {sign}''{} {:02}'' DAY({precision}) TO HOUR'",
            parts.leading, parts.hour
        ),
        (IntervalField::Day, Some(IntervalField::Minute)) => format!(
            "'INTERVAL {sign}''{} {:02}:{:02}'' DAY({precision}) TO MINUTE'",
            parts.leading, parts.hour, parts.minute
        ),
        (IntervalField::Day, Some(IntervalField::Second)) => format!(
            "'INTERVAL {sign}''{} {:02}:{:02}:{:02}{fraction}'' DAY({precision}) TO SECOND({scale})'",
            parts.leading, parts.hour, parts.minute, parts.second
        ),
        (IntervalField::Hour, Some(IntervalField::Minute)) => format!(
            "'INTERVAL {sign}''{}:{:02}'' HOUR({precision}) TO MINUTE'",
            parts.leading, parts.minute
        ),
        (IntervalField::Hour, Some(IntervalField::Second)) => format!(
            "'INTERVAL {sign}''{}:{:02}:{:02}{fraction}'' HOUR({precision}) TO SECOND({scale})'",
            parts.leading, parts.minute, parts.second
        ),
        (IntervalField::Minute, Some(IntervalField::Second)) => format!(
            "'INTERVAL {sign}''{}:{:02}{fraction}'' MINUTE({precision}) TO SECOND({scale})'",
            parts.leading, parts.second
        ),
        _ => return Err(malformed()),
    };

    Ok(text)
}

struct IntervalQualifier {
    leading: IntervalField,
    trailing: Option<IntervalField>,
    precision: u32,
    scale: u32,
}

/// Parses `FIELD [(p[,s])] [TO FIELD [(s)]]`.
fn parse_interval_qualifier(text: &str) -> Option<IntervalQualifier> {
    let mut precision = DEFAULT_INTERVAL_PRECISION;
    let mut scale = DEFAULT_INTERVAL_SCALE;

    let rest = text.trim();
    let (leading_word, rest) = take_word(rest)?;
    let leading = IntervalField::parse(leading_word)?;

    let mut rest = rest.trim_start();
    if let Some(inner) = rest.strip_prefix('(') {
        let (spec, remainder) = inner.split_once(')')?;
        let mut nums = spec.split(',');
        precision = nums.next()?.trim().parse().ok()?;
        if let Some(s) = nums.next() {
            scale = s.trim().parse().ok()?;
        }
        if nums.next().is_some() {
            return None;
        }
        rest = remainder.trim_start();
    }

    if rest.is_empty() {
        // A bare SECOND qualifier keeps the default scale; every other field
        // has no fractional part at all.
        if leading != IntervalField::Second {
            scale = 0;
        }
        return Some(IntervalQualifier {
            leading,
            trailing: None,
            precision,
            scale,
        });
    }

    let (to_word, rest) = take_word(rest)?;
    if !to_word.eq_ignore_ascii_case("TO") {
        return None;
    }
    let (trailing_word, rest) = take_word(rest.trim_start())?;
    let trailing = IntervalField::parse(trailing_word)?;

    let mut rest = rest.trim_start();
    if let Some(inner) = rest.strip_prefix('(') {
        let (spec, remainder) = inner.split_once(')')?;
        scale = spec.trim().parse().ok()?;
        rest = remainder.trim_start();
    } else if trailing != IntervalField::Second {
        scale = 0;
    }
    if !rest.is_empty() {
        return None;
    }
    if trailing != IntervalField::Second {
        scale = 0;
    }

    Some(IntervalQualifier {
        leading,
        trailing: Some(trailing),
        precision,
        scale,
    })
}

fn take_word(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let end = text
        .find(|c: char| !c.is_alphanumeric())
        .unwrap_or(text.len());
    if end == 0 {
        return None;
    }
    Some((&text[..end], &text[end..]))
}

#[derive(Default)]
struct IntervalParts {
    leading: u64,
    month: u64,
    hour: u64,
    minute: u64,
    second: u64,
    /// Nanoseconds, so the `.%09lu` formatting msodbcsql uses can be applied.
    fraction: u32,
}

/// Parses the quoted value against the shape its qualifier implies.
fn parse_interval_value(
    value: &str,
    leading: IntervalField,
    trailing: Option<IntervalField>,
) -> Option<IntervalParts> {
    let mut parts = IntervalParts::default();

    // Split off the fractional seconds first; only second-bearing qualifiers
    // may carry one.
    let (value, fraction_text) = match value.split_once('.') {
        Some((v, f)) => (v, Some(f)),
        None => (value, None),
    };
    if let Some(f) = fraction_text {
        let ends_in_second = trailing == Some(IntervalField::Second)
            || (trailing.is_none() && leading == IntervalField::Second);
        if !ends_in_second || f.is_empty() || f.len() > MAX_INTERVAL_PRECISION as usize {
            return None;
        }
        if !f.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let digits: u32 = f.parse().ok()?;
        parts.fraction = digits * 10u32.pow(MAX_INTERVAL_PRECISION - f.len() as u32);
    }

    let fields: Vec<&str> = match (leading, trailing) {
        (_, None) => vec![value],
        (IntervalField::Year, Some(IntervalField::Month)) => value.split('-').collect(),
        (IntervalField::Day, Some(IntervalField::Hour)) => value.split(' ').collect(),
        (IntervalField::Day, Some(IntervalField::Minute))
        | (IntervalField::Day, Some(IntervalField::Second)) => {
            let (day, time) = value.split_once(' ')?;
            let mut v = vec![day];
            v.extend(time.split(':'));
            v
        }
        (IntervalField::Hour, Some(IntervalField::Minute))
        | (IntervalField::Hour, Some(IntervalField::Second))
        | (IntervalField::Minute, Some(IntervalField::Second)) => value.split(':').collect(),
        _ => return None,
    };

    let expected = match (leading, trailing) {
        (_, None) => 1,
        (IntervalField::Day, Some(IntervalField::Minute)) => 3,
        (IntervalField::Day, Some(IntervalField::Second)) => 4,
        (IntervalField::Hour, Some(IntervalField::Second)) => 3,
        _ => 2,
    };
    if fields.len() != expected {
        return None;
    }
    for f in &fields {
        if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }

    let mut nums = fields.iter().map(|f| f.parse::<u64>());
    parts.leading = nums.next()?.ok()?;
    match (leading, trailing) {
        (_, None) => {}
        (IntervalField::Year, Some(IntervalField::Month)) => parts.month = nums.next()?.ok()?,
        (IntervalField::Day, Some(IntervalField::Hour)) => parts.hour = nums.next()?.ok()?,
        (IntervalField::Day, Some(IntervalField::Minute)) => {
            parts.hour = nums.next()?.ok()?;
            parts.minute = nums.next()?.ok()?;
        }
        (IntervalField::Day, Some(IntervalField::Second)) => {
            parts.hour = nums.next()?.ok()?;
            parts.minute = nums.next()?.ok()?;
            parts.second = nums.next()?.ok()?;
        }
        (IntervalField::Hour, Some(IntervalField::Minute)) => parts.minute = nums.next()?.ok()?,
        (IntervalField::Hour, Some(IntervalField::Second)) => {
            parts.minute = nums.next()?.ok()?;
            parts.second = nums.next()?.ok()?;
        }
        (IntervalField::Minute, Some(IntervalField::Second)) => parts.second = nums.next()?.ok()?,
        _ => return None,
    }

    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(sql: &str) -> String {
        translate_escapes(sql)
            .expect("translation should succeed")
            .sql
    }

    fn err(sql: &str) -> EscapeError {
        translate_escapes(sql).expect_err("translation should fail")
    }

    // -- pass-through -------------------------------------------------------
    //
    // SQL Server parses these natively, so msodbcsql returns them byte for
    // byte. Measured against msodbcsql 18.6.2.1 via SQLNativeSqlW.

    #[test]
    fn native_escapes_are_passed_through_verbatim() {
        for sql in [
            "SELECT {fn UCASE('abc')}",
            "SELECT {fn CONVERT(123, SQL_VARCHAR)}",
            "SELECT {d '2020-01-02'}",
            "SELECT {t '13:14:15'}",
            "SELECT {ts '2020-01-02 13:14:15'}",
            "SELECT {guid '6F9619FF-8B86-D011-B42D-00C04FC964FF'}",
            "SELECT COUNT(*) FROM {oj a LEFT OUTER JOIN b ON a.id = b.id}",
        ] {
            assert_eq!(tr(sql), sql, "{sql}");
        }
    }

    /// Escapes inside literals and comments are text, not escapes.
    #[test]
    fn literals_and_comments_are_not_scanned() {
        let sql = "SELECT 1 /* {fn UCASE('x')} */ , '{d ''2020-01-02''}'";
        assert_eq!(tr(sql), sql);
        assert_eq!(tr("SELECT '{bogus 1}'"), "SELECT '{bogus 1}'");
        assert_eq!(tr("SELECT 1 -- {bogus 1}"), "SELECT 1 -- {bogus 1}");
        assert_eq!(tr("SELECT [{bogus 1}]"), "SELECT [{bogus 1}]");
    }

    /// An unmatched brace is not an escape: msodbcsql's FindECode reports "not
    /// found" and the text goes to the server as written.
    #[test]
    fn unmatched_brace_is_left_alone() {
        assert_eq!(tr("SELECT '{' + x"), "SELECT '{' + x");
        assert_eq!(tr("SELECT {fn UCASE('a')"), "SELECT {fn UCASE('a')");
        assert_eq!(tr("SELECT } FROM t"), "SELECT } FROM t");
    }

    #[test]
    fn unknown_escape_is_a_syntax_error() {
        assert!(err("SELECT {bogus 1}").message().contains("bogus"));
        assert!(err("SELECT {}").message().contains("no keyword"));
    }

    /// Phase separation: translation must never touch a parameter marker,
    /// because SQLNativeSql has to return them intact.
    #[test]
    fn markers_survive_translation() {
        for sql in [
            "SELECT ?, ?",
            "SELECT {fn UCASE(?)}",
            "{call p(?,?)}",
            "{? = call p(?)}",
            "SELECT {interval '1' DAY}, ?",
        ] {
            let out = tr(sql);
            assert_eq!(
                out.matches('?').count(),
                sql.matches('?').count(),
                "marker count changed for {sql} -> {out}"
            );
            assert!(!out.contains("@P"), "{sql} -> {out}");
        }
    }

    // -- {escape} -----------------------------------------------------------

    #[test]
    fn escape_clause_loses_its_braces() {
        assert_eq!(
            tr(r"SELECT COUNT(*) FROM sys.objects WHERE name LIKE 'a\_b' {escape '\'}"),
            r"SELECT COUNT(*) FROM sys.objects WHERE name LIKE 'a\_b'  ESCAPE '\' "
        );
    }

    // -- {encrypt} ----------------------------------------------------------

    #[test]
    fn encrypt_matches_the_login7_scrambler() {
        assert_eq!(tr("SELECT {encrypt N'abc'}"), "SELECT  0xB3A583A593A5 ");
        // Embedded doubled quote.
        assert_eq!(tr("SELECT {encrypt N'a''b'}"), "SELECT  0xB3A5D7A583A5 ");
    }

    #[test]
    fn encrypt_requires_the_n_prefix() {
        assert!(
            err("SELECT {encrypt 'abc'}")
                .message()
                .contains("prefixed with N")
        );
        assert!(
            err("SELECT {encrypt N'abc' junk}")
                .message()
                .contains("unexpected text")
        );
    }

    /// An unterminated literal swallows the closing brace, so there is no
    /// escape to translate and the text goes to the server as written --
    /// measured: msodbcsql returns {encrypt N'abc} unchanged.
    #[test]
    fn encrypt_with_an_unterminated_literal_is_not_an_escape() {
        assert_eq!(tr("SELECT {encrypt N'abc}"), "SELECT {encrypt N'abc}");
    }

    // -- {interval} ---------------------------------------------------------
    //
    // Golden values measured against msodbcsql 18.6.2.1 via SQLNativeSqlW; see
    // docs/odbc-escape-sequences-plan.md section 5.3.

    #[test]
    fn interval_matches_msodbcsql() {
        let cases = [
            ("{interval '1' DAY}", "'INTERVAL +''1'' DAY(2)'"),
            ("{interval -'1' DAY}", "'INTERVAL -''1'' DAY(2)'"),
            ("{interval '1' DAY(3)}", "'INTERVAL +''1'' DAY(3)'"),
            ("{interval '10' YEAR}", "'INTERVAL +''10'' YEAR(2)'"),
            (
                "{interval '1-2' YEAR TO MONTH}",
                "'INTERVAL +''1-02'' YEAR(2) TO MONTH'",
            ),
            (
                "{interval '1 12' DAY TO HOUR}",
                "'INTERVAL +''1 12'' DAY(2) TO HOUR'",
            ),
            (
                "{interval '1 12:30' DAY TO MINUTE}",
                "'INTERVAL +''1 12:30'' DAY(2) TO MINUTE'",
            ),
            (
                "{interval '1 12:30:45.123' DAY TO SECOND(3)}",
                "'INTERVAL +''1 12:30:45.123'' DAY(2) TO SECOND(3)'",
            ),
            (
                "{interval '30' SECOND}",
                "'INTERVAL +''30.000000'' SECOND(2,6)'",
            ),
            (
                "{interval '30.5' SECOND(2,1)}",
                "'INTERVAL +''30.5'' SECOND(2,1)'",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(tr(input), format!(" {expected} "), "{input}");
        }
    }

    /// Every interval failure reports 22018, not the 42000 the other escapes
    /// use, because msodbcsql routes it through the type converter
    /// (CVT_ERROR out of ParseInterval). Measured against msodbcsql 18.6.2.1.
    #[test]
    fn interval_rejects_out_of_range_values_with_22018() {
        for sql in [
            // 100 does not fit the default precision of 2.
            "{interval '100' DAY}",
            // Field bounds.
            "{interval '1-12' YEAR TO MONTH}",
            "{interval '1 24' DAY TO HOUR}",
            "{interval '1 12:60' DAY TO MINUTE}",
            "{interval '1:2:60' HOUR TO SECOND}",
            // Precision beyond 9.
            "{interval '1' DAY(10)}",
            // Trailing junk, missing pieces, unquoted value, shape mismatch.
            "{interval '1' DAY junk}",
            "{interval '1'}",
            "{interval 1 DAY}",
            "{interval 'x' DAY}",
            "{interval '1' YEAR TO MONTH}",
        ] {
            let e = err(sql);
            assert_eq!(e.state(), SQLSTATE_22018, "{sql}: {}", e.message());
        }
    }

    /// Fractional digits beyond the declared scale are 22001 for an ODBC 3.x
    /// application -- ProcessDTI promotes 01S07 to 22001 (sqlcmisc.cpp:7945).
    /// Measured: {interval '30.1234' SECOND(2,3)} -> 22001.
    #[test]
    fn interval_rejects_excess_fractional_digits_with_22001() {
        let e = err("{interval '30.1234' SECOND(2,3)}");
        assert_eq!(e.state(), SQLSTATE_22001);
        // Exactly at the declared scale is fine.
        assert!(translate_escapes("{interval '30.5' SECOND(2,1)}").is_ok());
    }

    // -- {call} -------------------------------------------------------------

    /// Golden values measured against msodbcsql 18.6.2.1 via SQLNativeSqlW.
    /// A translated escape is replaced by " " + text + " ", and the argument
    /// list's final separator becomes a space, so a call ends in two spaces.
    #[test]
    fn call_matches_msodbcsql_text_form() {
        assert_eq!(tr("{call sp_who}"), " EXEC sp_who  ");
        assert_eq!(tr("{? = call sp_who(?)}"), " EXEC ?=sp_who ?  ");
        assert_eq!(
            tr("{call dbo.myproc(?, DEFAULT, {ts '2020-01-02 13:14:15'})}"),
            " EXEC dbo.myproc ?,DEFAULT,{ts '2020-01-02 13:14:15'}  "
        );
        assert_eq!(tr("{CALL P}"), " EXEC P  ");
        assert_eq!(tr("{ call p }"), " EXEC p  ");
        assert_eq!(tr("{call p(?, 'lit')}"), " EXEC p ?,'lit'  ");
        assert_eq!(tr("{call [my proc](?)}"), " EXEC [my proc] ?  ");
        assert_eq!(tr("{call myproc;2(?)}"), " EXEC myproc;2 ?  ");
        assert_eq!(tr("{call p(?) } extra"), " EXEC p ?   extra");
    }

    #[test]
    fn call_site_is_recovered_for_the_rpc_path() {
        let t = translate_escapes("{call dbo.p(?,?)}").unwrap();
        let call = t.call.expect("single call should be RPC eligible");
        assert_eq!(call.proc_name, "dbo.p");
        assert!(!call.returns_status);
        assert!(call.is_rpc_eligible());

        let t = translate_escapes("{? = call dbo.p(?, DEFAULT)}").unwrap();
        let call = t.call.unwrap();
        assert!(call.returns_status);
        assert!(call.is_rpc_eligible());
    }

    /// Only a statement that is *nothing but* one canonical call can go over
    /// RPC; anything else falls back to the EXEC text.
    #[test]
    fn call_site_is_not_recovered_for_mixed_statements() {
        assert!(
            translate_escapes("SELECT 1; {call p}")
                .unwrap()
                .call
                .is_none()
        );
        assert!(
            translate_escapes("{call p}; {call p}")
                .unwrap()
                .call
                .is_none()
        );
        // Trailing separator and whitespace are allowed.
        assert!(translate_escapes("  {call p} ; ").unwrap().call.is_some());
    }

    /// A literal argument has no bound parameter to carry it, so the call is
    /// still recognised but is not RPC eligible.
    #[test]
    fn literal_arguments_block_the_rpc_path() {
        let call = translate_escapes("{call p(?, 'lit')}")
            .unwrap()
            .call
            .unwrap();
        assert!(!call.is_rpc_eligible());
    }

    #[test]
    fn call_accepts_named_markers_and_group_numbers() {
        let call = translate_escapes("{call p(@a = ?, @b = ?)}")
            .unwrap()
            .call
            .unwrap();
        assert_eq!(
            call.args,
            vec![
                CallArg::Marker {
                    name: Some("@a".into()),
                    text: "@a = ?".into()
                },
                CallArg::Marker {
                    name: Some("@b".into()),
                    text: "@b = ?".into()
                },
            ]
        );
        // msodbcsql writes the argument through verbatim, spaces and all.
        assert_eq!(tr("{call p(@a = ?)}"), " EXEC p @a = ?  ");

        let call = translate_escapes("{call myproc;2(?)}")
            .unwrap()
            .call
            .unwrap();
        assert_eq!(call.proc_name, "myproc;2");
    }

    #[test]
    fn empty_argument_is_default() {
        let call = translate_escapes("{call p(?,,?)}").unwrap().call.unwrap();
        assert_eq!(call.args[1], CallArg::Default);
        assert_eq!(tr("{call p(?,,?)}"), " EXEC p ?,DEFAULT,?  ");
    }

    #[test]
    fn call_rejects_malformed_forms() {
        assert!(translate_escapes("{call}").is_err());
        assert!(translate_escapes("{call p(?}").is_err());
        assert!(translate_escapes("{? call p}").is_err());
        assert!(translate_escapes("{? = exec p}").is_err());
        assert!(translate_escapes("{call p(?) junk}").is_err());
        assert!(translate_escapes("{call p(?) ; }").is_err());
    }

    /// msodbcsql rejects {fn ...} nested inside a canonical call
    /// (SubstituteECodes, sqlcmisc.cpp:4712).
    #[test]
    fn fn_escape_inside_a_call_is_rejected() {
        assert!(
            err("{call p({fn UCASE('a')})}")
                .message()
                .contains("not allowed inside")
        );
        // Datetime escapes nested in a call are fine.
        assert!(translate_escapes("{call p({ts '2020-01-02 00:00:00'})}").is_ok());
    }

    /// A malformed escape nested inside a passed-through one still fails the
    /// statement, because msodbcsql processes the innermost escape first.
    #[test]
    fn nested_malformed_escape_is_rejected() {
        assert!(translate_escapes("SELECT {fn UCASE({bogus 1})}").is_err());
    }

    // -- procedure name validation -----------------------------------------

    #[test]
    fn procedure_names_accept_legal_identifier_forms() {
        for name in [
            "p",
            "dbo.p",
            "db.dbo.p",
            "srv.db.dbo.p",
            "db..p",
            "[my proc]",
            "[db].[dbo].[my proc]",
            "\"quoted proc\"",
            "#temp_proc",
            "p;2",
            "[weird]]name]",
        ] {
            assert!(
                validate_procedure_name(name).is_ok(),
                "should accept {name}"
            );
        }
    }

    /// The procedure name is the one place application text reaches an
    /// interpolated T-SQL EXEC on Always Encrypted connections, so anything
    /// that could carry a second statement or a comment must be refused.
    #[test]
    fn procedure_names_reject_injection_attempts() {
        for name in [
            "p;DROP TABLE t",
            "p--comment",
            "p/*c*/",
            "p'x'",
            "p)",
            "p x",
            "",
            ";2",
            "p;",
            "p;2x",
            "a.b.c.d.e",
            "[unclosed",
            "p+q",
        ] {
            assert!(
                validate_procedure_name(name).is_err(),
                "should reject {name:?}"
            );
        }
    }

    // -- lexer --------------------------------------------------------------

    /// The shared lexer must classify exactly what rewrite_param_markers always
    /// classified, including msodbcsql's two quirks.
    #[test]
    fn code_scan_reproduces_the_marker_lexer_quirks() {
        // Block comments do not nest: the ? after the inner */ is code.
        let sql = "/* a /* b */ ? */";
        let mut scan = CodeScan::new(sql);
        let mut code_chars = String::new();
        while let Some(step) = scan.next_step() {
            if step.code {
                code_chars.push(step.ch);
            }
        }
        assert!(code_chars.contains('?'));

        // --(* ... *)-- is not a comment.
        let sql = "--(* Vendor(Microsoft), Product(ODBC) x *)-- ?";
        let mut scan = CodeScan::new(sql);
        let mut markers = 0;
        while let Some(step) = scan.next_step() {
            if step.code && step.ch == '?' {
                markers += 1;
            }
        }
        assert_eq!(markers, 1);
    }

    // -- phase 1 + phase 2 together ---------------------------------------

    #[test]
    fn execute_path_translates_then_rewrites_markers() {
        let (sql, count, call) = translate_and_rewrite("{call p(?,?)}", false).unwrap();
        assert_eq!(sql, " EXEC p @P1,@P2  ");
        assert_eq!(count, 2);
        assert!(call.is_some());
    }

    /// SQL_ATTR_NOSCAN suppresses translation entirely -- the text goes to the
    /// server as written -- but markers are still rewritten, because that is
    /// how parameters are bound at all.
    #[test]
    fn noscan_suppresses_translation_but_not_marker_rewriting() {
        let (sql, count, call) = translate_and_rewrite("{call p(?,?)}", true).unwrap();
        assert_eq!(sql, "{call p(@P1,@P2)}");
        assert_eq!(count, 2);
        assert!(call.is_none(), "NOSCAN must not produce an RPC call site");
    }

    /// With NOSCAN on, a malformed escape is the server's problem, not ours.
    #[test]
    fn noscan_does_not_reject_malformed_escapes() {
        assert!(translate_and_rewrite("SELECT {bogus 1}", true).is_ok());
        assert!(translate_and_rewrite("SELECT {bogus 1}", false).is_err());
    }

    /// Describe always translates, and drops the return-status marker because
    /// the metadata RPC cannot parse it.
    #[test]
    fn describe_text_drops_the_return_status_marker() {
        let (text, had_status) = describe_text("{? = call dbo.p(?,?)}").unwrap();
        assert_eq!(text, " EXEC dbo.p @P1,@P2  ");
        assert!(had_status);

        let (text, had_status) = describe_text("{call dbo.p(?,?)}").unwrap();
        assert_eq!(text, " EXEC dbo.p @P1,@P2  ");
        assert!(!had_status);

        let (text, had_status) = describe_text("SELECT ?, ?").unwrap();
        assert_eq!(text, "SELECT @P1, @P2");
        assert!(!had_status);
    }

    /// The invariants the fuzz target asserts, pinned over a corpus of the
    /// shapes that break hand-written lexers, so CI covers them without nightly.
    #[test]
    fn scanner_invariants_hold_over_awkward_input() {
        for input in [
            "",
            "{",
            "}",
            "{}",
            "{{{{",
            "}}}}",
            "'",
            "\"",
            "[",
            "/*",
            "--",
            "--(*",
            "*)--",
            "{d '",
            "'{d ''}'",
            "/* {call p} ",
            "{call p({call q({call r})})}",
            "?{?}?",
            "{fn UCASE(?)}",
            "N'{ts '' }'",
            "{escape",
            "{interval '1' DAY",
            "{ }",
            "select 1 -- {fn x}\n{fn UCASE('a')}",
        ] {
            let markers = input.matches('?').count();
            if let Ok(t) = translate_escapes(input) {
                assert_eq!(
                    t.sql.matches('?').count(),
                    markers,
                    "marker count changed for {input:?} -> {:?}",
                    t.sql
                );
            }
            let (noscan_sql, noscan_count, call) =
                translate_and_rewrite(input, true).expect("NOSCAN never fails");
            let (expected, expected_count) = super::super::util::rewrite_param_markers(input);
            assert_eq!(noscan_sql, expected, "{input:?}");
            assert_eq!(noscan_count, expected_count, "{input:?}");
            assert!(call.is_none(), "{input:?}");
            // Must terminate and never panic.
            let _ = translate_and_rewrite(input, false);
            let _ = describe_text(input);
        }
    }

    #[test]
    fn empty_input_is_handled() {
        let t = translate_escapes("").unwrap();
        assert_eq!(t.sql, "");
        assert!(t.call.is_none());
    }

    /// An escape with no body after the keyword is legal for {escape} and for
    /// the pass-through family; measured: {escape} -> " ESCAPE ", {d} unchanged.
    #[test]
    fn empty_escape_bodies_match_msodbcsql() {
        assert_eq!(tr("SELECT {escape}"), "SELECT  ESCAPE ");
        assert_eq!(tr("SELECT {d}"), "SELECT {d}");
        assert_eq!(tr("SELECT {fn}"), "SELECT {fn}");
        assert_eq!(tr("SELECT {oj}"), "SELECT {oj}");
    }

    #[test]
    fn syntax_errors_report_42000() {
        for sql in [
            "SELECT {bogus 1}",
            "SELECT {}",
            "SELECT {   }",
            "{call}",
            "{? call p}",
        ] {
            assert_eq!(err(sql).state(), SQLSTATE_42000, "{sql}");
        }
    }
}
