# Wraps the extracted chroma table in a Rust module.
$table = (Get-Content "$env:TEMP\chroma-table.rs" -Raw).TrimEnd()
$out = "c:\mssql-rs1\mssql-rs\mssql-sqlcmd\src\fmt\schemes.rs"

$header = @'
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The colour of each text type under every scheme go-sqlcmd accepts.
//!
//! go-sqlcmd colours through the `chroma` library, whose styles are XML files
//! shipped with it. Rather than take a syntax-highlighting dependency for five
//! colours per style, the five are extracted here — resolved through chroma's
//! own token inheritance, so a style defining only `LiteralString` gives the
//! same answer it would there.
//!
//! Generated from chroma v2.27.0 by `scripts/extract-styles.ps1`. A scheme
//! chroma knows and this table does not would simply not colour, which is the
//! same as naming a scheme that does not exist.

use super::color::Face;

/// Foreground colours in the order [cell, header, separator, error, warning].
pub const SCHEMES: &[(&str, [Face; 5])] = &[
'@

Set-Content -Path $out -Value ($header + "`n" + $table + "`n];`n") -Encoding UTF8
"written: " + (Get-Item $out).Length + " bytes"
