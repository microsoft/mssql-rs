// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The colour of each text type under every scheme go-sqlcmd accepts.
//!
//! go-sqlcmd colours through the `chroma` library, whose styles are XML files
//! shipped with it. Rather than take a syntax-highlighting dependency for five
//! colours per style, the five are extracted here â€” resolved through chroma's
//! own token inheritance, so a style defining only `LiteralString` gives the
//! same answer it would there.
//!
//! Generated from chroma v2.27.0 by `scripts/extract-styles.ps1`. A scheme
//! chroma knows and this table does not would simply not colour, which is the
//! same as naming a scheme that does not exist.

use super::color::Face;

/// Foreground colours in the order [cell, header, separator, error, warning].
pub const SCHEMES: &[(&str, [Face; 5])] = &[
    (
        "abap",
        [
            Face {
                rgb: Some(0x55aa22),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x55aa22),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "algol",
        [
            Face {
                rgb: Some(0x666666),
                bold: false,
                italic: true,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x666666),
                bold: false,
                italic: true,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "algol_nu",
        [
            Face {
                rgb: Some(0x666666),
                bold: false,
                italic: true,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x666666),
                bold: false,
                italic: true,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "arduino",
        [
            Face {
                rgb: Some(0x7f8c8d),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x7f8c8d),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "ashen",
        [
            Face {
                rgb: Some(0xdf6464),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xdf6464),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xdf6464),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc53030),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xb4b4b4),
                bold: false,
                italic: false,
                underline: true,
            },
        ],
    ),
    (
        "aura-theme-dark-soft",
        [
            Face {
                rgb: Some(0x54c59f),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x8464c6),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x54c59f),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc55858),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xbdbdbd),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "aura-theme-dark",
        [
            Face {
                rgb: Some(0x61ffca),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xa277ff),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x61ffca),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xff6767),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xedecee),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "autumn",
        [
            Face {
                rgb: Some(0xaa5500),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000080),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xaa5500),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xaa0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "average",
        [
            Face {
                rgb: Some(0x008900),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x757575),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x008900),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xec0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x757575),
                bold: false,
                italic: false,
                underline: true,
            },
        ],
    ),
    (
        "base16-snazzy",
        [
            Face {
                rgb: Some(0x5af78e),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe2e4e5),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x5af78e),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xff5c57),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe2e4e5),
                bold: false,
                italic: false,
                underline: true,
            },
        ],
    ),
    (
        "borland",
        [
            Face {
                rgb: Some(0x0000ff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x999999),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x0000ff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xaa0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "bw",
        [
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
            Face {
                rgb: None,
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "catppuccin-frappe",
        [
            Face {
                rgb: Some(0xa6d189),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xef9f76),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x8caaee),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe78284),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc6d0f5),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "catppuccin-latte",
        [
            Face {
                rgb: Some(0x40a02b),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xfe640b),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x1e66f5),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd20f39),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x4c4f69),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "catppuccin-macchiato",
        [
            Face {
                rgb: Some(0xa6da95),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf5a97f),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x8aadf4),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xed8796),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xcad3f5),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "catppuccin-mocha",
        [
            Face {
                rgb: Some(0xa6e3a1),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xfab387),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x89b4fa),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf38ba8),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xcdd6f4),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "colorful",
        [
            Face {
                rgb: Some(0xdd2200),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000080),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xff0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "darcula",
        [
            Face {
                rgb: Some(0x6a8759),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xa9b7c6),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x6a8759),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xff6b68),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xa9b7c6),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "doom-one",
        [
            Face {
                rgb: Some(0x70b33f),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xa2cbff),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x98c379),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xb0c4de),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xb0c4de),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "doom-one2",
        [
            Face {
                rgb: Some(0x70b33f),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xa2cbff),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x98c379),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xb0c4de),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xb0c4de),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "dracula",
        [
            Face {
                rgb: Some(0xf1fa8c),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf1fa8c),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: false,
                italic: false,
                underline: true,
            },
        ],
    ),
    (
        "emacs",
        [
            Face {
                rgb: Some(0x008000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000080),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xbb4444),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xff0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "evergarden",
        [
            Face {
                rgb: Some(0xb2c98f),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd699b6),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xb2c98f),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd6cbb4),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x6e8585),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "friendly",
        [
            Face {
                rgb: Some(0xc65d09),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000080),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x4070a0),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xff0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "fruity",
        [
            Face {
                rgb: Some(0x0086d2),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffffff),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x0086d2),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffffff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffffff),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "github-dark",
        [
            Face {
                rgb: Some(0xa5d6ff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x79c0ff),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x79c0ff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffa198),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe6edf3),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "github",
        [
            Face {
                rgb: Some(0x0a3069),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x0a3069),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x1f2328),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "gruvbox-light",
        [
            Face {
                rgb: Some(0x79740e),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x79740e),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x79740e),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x3c3836),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x076678),
                bold: false,
                italic: false,
                underline: true,
            },
        ],
    ),
    (
        "gruvbox",
        [
            Face {
                rgb: Some(0xb8bb26),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xb8bb26),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xb8bb26),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xebdbb2),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x83a598),
                bold: false,
                italic: false,
                underline: true,
            },
        ],
    ),
    (
        "hrdark",
        [
            Face {
                rgb: Some(0xa6be9d),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x1d2432),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xa6be9d),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x1d2432),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x1d2432),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "hr_high_contrast",
        [
            Face {
                rgb: Some(0xa87662),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xa87662),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "igor",
        [
            Face {
                rgb: Some(0x009c00),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x009c00),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "kanagawa-dragon",
        [
            Face {
                rgb: Some(0x8a9a7b),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x8ba4b0),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x949fb5),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe82424),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc5c9c5),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "kanagawa-lotus",
        [
            Face {
                rgb: Some(0x6f894e),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x4d699b),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x6693bf),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe82424),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x545464),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "kanagawa-wave",
        [
            Face {
                rgb: Some(0x98bb6c),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x7e9cd8),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x7fb4ca),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe82424),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xdcd7ba),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "lovelace",
        [
            Face {
                rgb: Some(0xa848a8),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x666666),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xb85820),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc02828),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "manni",
        [
            Face {
                rgb: Some(0xcc3300),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x003300),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xcc3300),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xff0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "modus-operandi",
        [
            Face {
                rgb: Some(0x2544bb),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x2544bb),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "modus-vivendi",
        [
            Face {
                rgb: Some(0x79a8ff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffffff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x79a8ff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffffff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffffff),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "monokai",
        [
            Face {
                rgb: Some(0xe6db74),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe6db74),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "monokailight",
        [
            Face {
                rgb: Some(0xd88200),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x272822),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd88200),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x272822),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x272822),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "murphy",
        [
            Face {
                rgb: Some(0xff8888),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000080),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xff0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "native",
        [
            Face {
                rgb: Some(0xffa500),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffffff),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xed9d13),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd22323),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd0d0d0),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "nord",
        [
            Face {
                rgb: Some(0xa3be8c),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x88c0d0),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xa3be8c),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xbf616a),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd8dee9),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "nordic",
        [
            Face {
                rgb: Some(0xa3be8c),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x88c0d0),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xa3be8c),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc5727a),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xbbc3d4),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "onedark",
        [
            Face {
                rgb: Some(0x98c379),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xabb2bf),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x98c379),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xabb2bf),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xabb2bf),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "onesenterprise",
        [
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "paraiso-dark",
        [
            Face {
                rgb: Some(0x48b685),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe7e9db),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x48b685),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe7e9db),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe7e9db),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "paraiso-light",
        [
            Face {
                rgb: Some(0x48b685),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x2f1e2e),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x48b685),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x2f1e2e),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x2f1e2e),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "pastie",
        [
            Face {
                rgb: Some(0x22bb22),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x333333),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xdd2200),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xaa0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "perldoc",
        [
            Face {
                rgb: Some(0xcb6c20),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000080),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xcd5555),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xaa0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "pygments",
        [
            Face {
                rgb: Some(0x008000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000080),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xba2121),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xff0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "rainbow_dash",
        [
            Face {
                rgb: Some(0x318495),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x2c5dcd),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x00cc66),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xff0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x4d4d4d),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "rose-pine-dawn",
        [
            Face {
                rgb: Some(0xea9d34),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x575279),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xea9d34),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x575279),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x575279),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "rose-pine-moon",
        [
            Face {
                rgb: Some(0xf6c177),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe0def4),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf6c177),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe0def4),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe0def4),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "rose-pine",
        [
            Face {
                rgb: Some(0xf6c177),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe0def4),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf6c177),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe0def4),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe0def4),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "rpgle",
        [
            Face {
                rgb: Some(0xd88200),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x272822),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd88200),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x272822),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x272822),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "rrt",
        [
            Face {
                rgb: Some(0x87ceeb),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x87ceeb),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "solarized-dark",
        [
            Face {
                rgb: Some(0x2aa198),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xcb4b16),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x2aa198),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xdc322f),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x93a1a1),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "solarized-dark256",
        [
            Face {
                rgb: Some(0x00afaf),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd75f00),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x00afaf),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xaf0000),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x8a8a8a),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "solarized-light",
        [
            Face {
                rgb: Some(0x2aa198),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd33682),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x2aa198),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd33682),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xd33682),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "swapoff",
        [
            Face {
                rgb: Some(0x00ffff),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe5e5e5),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x00ffff),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe5e5e5),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe5e5e5),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "tango",
        [
            Face {
                rgb: Some(0x4e9a06),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000080),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x4e9a06),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xef2929),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000000),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "tokyonight-day",
        [
            Face {
                rgb: Some(0x587539),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x8c6c3e),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x2e7de9),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc64343),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x3760bf),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "tokyonight-moon",
        [
            Face {
                rgb: Some(0xc3e88d),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffc777),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x82aaff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc53b53),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc8d3f5),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "tokyonight-night",
        [
            Face {
                rgb: Some(0x9ece6a),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe0af68),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x7aa2f7),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xdb4b4b),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc0caf5),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "tokyonight-storm",
        [
            Face {
                rgb: Some(0x9ece6a),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xe0af68),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x7aa2f7),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xdb4b4b),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc0caf5),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "trac",
        [
            Face {
                rgb: Some(0xbb8844),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x999999),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xbb8844),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xaa0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "vim",
        [
            Face {
                rgb: Some(0xcd0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x000080),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xcd0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xff0000),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xcccccc),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "vs",
        [
            Face {
                rgb: Some(0xa31515),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xa31515),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "vulcan",
        [
            Face {
                rgb: Some(0x82cc6a),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xecbe7b),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x82cc6a),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xcf5967),
                bold: true,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc9c9c9),
                bold: false,
                italic: false,
                underline: true,
            },
        ],
    ),
    (
        "witchhazel",
        [
            Face {
                rgb: Some(0x1bc5e0),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0x1bc5e0),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xf8f8f2),
                bold: false,
                italic: true,
                underline: false,
            },
        ],
    ),
    (
        "xcode-dark",
        [
            Face {
                rgb: Some(0xfc6a5d),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffffff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xfc6a5d),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffffff),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xffffff),
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
    (
        "xcode",
        [
            Face {
                rgb: Some(0xc41a16),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: Some(0xc41a16),
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
            Face {
                rgb: None,
                bold: false,
                italic: false,
                underline: false,
            },
        ],
    ),
];
