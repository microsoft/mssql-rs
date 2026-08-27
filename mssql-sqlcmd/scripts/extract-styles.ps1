# Extracts the token colours go-sqlcmd actually uses from chroma's style files,
# and emits them as a Rust table.
#
# go-sqlcmd maps its own text types onto five chroma tokens:
#   cell      -> StringOther     header  -> GenericHeading
#   separator -> StringDelimiter error   -> GenericError
#   warning   -> GenericEmph
#
# Chroma resolves a token with no entry of its own by walking up its parent
# chain, so the same walk is done here.

$root = Get-ChildItem -Recurse -Path "C:\tools\gopath\pkg\mod\github.com\alecthomas" `
    -Filter "*.xml" | Where-Object { $_.FullName -match "chroma.*styles" }

# Parent chains, innermost first, ending at the style-wide default.
#
# `GenericError` does NOT inherit from `Error`: monokai defines Error as
# #960050 yet draws error messages in #f8f8f2, its Text colour. Verified by
# capturing the reference through a PTY.
$chains = @{
    "StringOther"     = @("StringOther", "LiteralStringOther", "LiteralString", "Literal", "Text", "Background")
    "GenericHeading"  = @("GenericHeading", "Generic", "Text", "Background")
    "StringDelimiter" = @("StringDelimiter", "LiteralStringDelimiter", "LiteralString", "Literal", "Text", "Background")
    "GenericError"    = @("GenericError", "Generic", "Text", "Background")
    "GenericEmph"     = @("GenericEmph", "Generic", "Text", "Background")
}
$order = @("StringOther", "GenericHeading", "StringDelimiter", "GenericError", "GenericEmph")

$rows = @()
foreach ($file in $root) {
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
    $default = ""
    foreach ($src in @("Text", "Background")) {
        if ($default) { break }
        if ($entries.ContainsKey($src)) {
            foreach ($part in ($entries[$src] -split '\s+')) {
                if ($part -match '^#([0-9a-fA-F]{6})$') { $default = $Matches[1].ToLower() }
            }
        }
    }

    $values = foreach ($want in $order) {
        $found = ""
        foreach ($step in $chains[$want]) {
            if ($entries.ContainsKey($step)) { $found = $entries[$step]; break }
        }
        # A style string may carry bold/italic/underline and a bg colour; only
        # the foreground and the emphasis flags are used here.
        $fg = ""
        $bold = "false"; $italic = "false"; $underline = "false"
        foreach ($part in ($found -split '\s+')) {
            if ($part -match '^#([0-9a-fA-F]{6})$') { $fg = $Matches[1].ToLower() }
            elseif ($part -eq "bold") { $bold = "true" }
            elseif ($part -eq "italic") { $italic = "true" }
            elseif ($part -eq "underline") { $underline = "true" }
        }
        if (-not $fg) { $fg = $default }
        "Face { rgb: $(if ($fg) { "Some(0x$fg)" } else { "None" }), bold: $bold, italic: $italic, underline: $underline }"
    }

    $rows += '    ("' + $name + '", [' + "`n        " + ($values -join ",`n        ") + "`n    ]),"
}

"// {0} styles" -f $rows.Count
$rows -join "`n" | Out-File -Encoding utf8 "$env:TEMP\chroma-table.rs"
"written to $env:TEMP\chroma-table.rs"
(Get-Item "$env:TEMP\chroma-table.rs").Length
