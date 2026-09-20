# Regenerates src/fmt/schemes.rs from chroma's style files.
#
# go-sqlcmd colours through chroma, whose styles are XML files shipped with it.
# Rather than take a syntax-highlighting dependency for five colours per style,
# the five are extracted here and emitted as a const table.
#
# go-sqlcmd maps its own text types onto five chroma tokens:
#   cell      -> StringOther     header  -> GenericHeading
#   separator -> StringDelimiter error   -> GenericError
#   warning   -> GenericEmph
#
# Chroma resolves a token with no entry of its own by walking up its parent
# chain, so the same walk is done here.
#
# Usage:
#   ./generate-schemes.ps1                                  # finds chroma in the Go module cache
#   ./generate-schemes.ps1 -ChromaPath <dir>                # explicit chroma checkout or module dir
#
# Requires a local copy of chroma. If go-sqlcmd has been built on this machine
# it is already in the module cache; otherwise clone
# https://github.com/alecthomas/chroma and pass -ChromaPath.

[CmdletBinding()]
param(
    # Directory to search for chroma's style XML. Defaults to the Go module cache.
    [string]$ChromaPath,

    # Where to write the generated module. Defaults to ../src/fmt/schemes.rs.
    [string]$OutFile
)

$ErrorActionPreference = 'Stop'

if (-not $OutFile) {
    $OutFile = Join-Path $PSScriptRoot '..\src\fmt\schemes.rs'
}

if (-not $ChromaPath) {
    $gopath = (& go env GOPATH 2>$null)
    if (-not $gopath) {
        throw "Go not found on PATH. Pass -ChromaPath with a chroma checkout instead."
    }
    $ChromaPath = Join-Path $gopath 'pkg\mod\github.com\alecthomas'
}

if (-not (Test-Path $ChromaPath)) {
    throw "Chroma path not found: $ChromaPath"
}

$styleFiles = Get-ChildItem -Recurse -Path $ChromaPath -Filter '*.xml' |
    Where-Object { $_.FullName -match 'chroma.*styles' }

if (-not $styleFiles) {
    throw "No chroma style XML found under $ChromaPath"
}

# Parent chains, innermost first, ending at the style-wide default.
#
# `GenericError` does NOT inherit from `Error`: monokai defines Error as
# #960050 yet draws error messages in #f8f8f2, its Text colour. Verified by
# capturing the reference through a PTY.
$chains = @{
    'StringOther'     = @('StringOther', 'LiteralStringOther', 'LiteralString', 'Literal', 'Text', 'Background')
    'GenericHeading'  = @('GenericHeading', 'Generic', 'Text', 'Background')
    'StringDelimiter' = @('StringDelimiter', 'LiteralStringDelimiter', 'LiteralString', 'Literal', 'Text', 'Background')
    'GenericError'    = @('GenericError', 'Generic', 'Text', 'Background')
    'GenericEmph'     = @('GenericEmph', 'Generic', 'Text', 'Background')
}
$order = @('StringOther', 'GenericHeading', 'StringDelimiter', 'GenericError', 'GenericEmph')

$rows = @()
foreach ($file in $styleFiles) {
    [xml]$doc = Get-Content $file.FullName -Raw
    # chroma registers under `strings.ToLower(style.Name)`, so `RPGLE` is
    # reachable and listed as `rpgle`.
    $name = $doc.style.name
    if (-not $name) { continue }
    $name = $name.ToLower()

    $entries = @{}
    foreach ($e in $doc.style.entry) { $entries[$e.type] = $e.style }

    # A style's own default foreground, used when the matched entry sets only
    # emphasis. monokai's GenericEmph is `italic` with no colour, and the
    # reference still draws it in #f8f8f2 -- which monokai carries on
    # `Background`, not `Text`, so both are consulted in that order.
    $default = ''
    foreach ($src in @('Text', 'Background')) {
        if ($default) { break }
        if ($entries.ContainsKey($src)) {
            foreach ($part in ($entries[$src] -split '\s+')) {
                if ($part -match '^#([0-9a-fA-F]{6})$') { $default = $Matches[1].ToLower() }
            }
        }
    }

    $values = foreach ($want in $order) {
        $found = ''
        foreach ($step in $chains[$want]) {
            if ($entries.ContainsKey($step)) { $found = $entries[$step]; break }
        }
        # A style string may carry bold/italic/underline and a bg colour; only
        # the foreground and the emphasis flags are used here.
        $fg = ''
        $bold = 'false'; $italic = 'false'; $underline = 'false'
        foreach ($part in ($found -split '\s+')) {
            if ($part -match '^#([0-9a-fA-F]{6})$') { $fg = $Matches[1].ToLower() }
            elseif ($part -eq 'bold') { $bold = 'true' }
            elseif ($part -eq 'italic') { $italic = 'true' }
            elseif ($part -eq 'underline') { $underline = 'true' }
        }
        if (-not $fg) { $fg = $default }
        "Face { rgb: $(if ($fg) { "Some(0x$fg)" } else { 'None' }), bold: $bold, italic: $italic, underline: $underline }"
    }

    $rows += '    ("' + $name + '", [' + "`n        " + ($values -join ",`n        ") + "`n    ]),"
}

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
//! Generated from chroma v2.27.0 by `scripts/generate-schemes.ps1`. A scheme
//! chroma knows and this table does not would simply not colour, which is the
//! same as naming a scheme that does not exist.

use super::color::Face;

/// Foreground colours in the order [cell, header, separator, error, warning].
pub const SCHEMES: &[(&str, [Face; 5])] = &[
'@

$body = ($rows -join "`n")
Set-Content -Path $OutFile -Value ($header + "`n" + $body + "`n];`n") -Encoding UTF8

"{0} schemes written to {1}" -f $rows.Count, (Resolve-Path $OutFile)
