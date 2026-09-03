// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exact numeric value model shared by both conversion directions.
//!
//! Keeping exact sources exact (rather than routing everything through `f64`)
//! is what lets an integer target report truncation instead of silently
//! dropping a fraction, and lets a value too wide for the target be reported as
//! `22003` rather than saturating.

use super::error::ConvError;

/// A numeric value in a form that keeps exact sources exact, so an integer
/// target can report truncation instead of silently dropping a fraction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NumericSource {
    Int(i128),
    /// `mantissa / 10^scale` — the exact decimal types (`decimal`, `numeric`,
    /// `money`, `smallmoney`) and decimal literals in character columns.
    Scaled {
        mantissa: i128,
        scale: u32,
    },
    /// A decimal literal with more digits than an exact `i128` mantissa holds.
    /// `approx` serves float targets; integer targets need only the integer part
    /// and whether anything non-zero was dropped, which survives any length.
    /// `negative` is kept separately because `int_part` cannot hold the sign of
    /// `-0.something` and `approx` can underflow to `-0.0`.
    ///
    /// [`NumericSource::Float`] has no such rescue, and deliberately so: it
    /// holds exponent forms, which msodbcsql also routes through a double
    /// (`sqlccnvt.cpp:5118`). `"-1e-400"` is `-0.0` there too, so
    /// [`NumericSource::is_negative`] answering `false` matches rather than
    /// diverges. See `parse_numeric_text` for the routing.
    WideDecimal {
        approx: f64,
        negative: bool,
        int_part: i128,
        fraction_dropped: bool,
    },
    Float(f64),
}

impl NumericSource {
    pub(crate) fn as_f64(&self) -> f64 {
        match self {
            NumericSource::Int(v) => *v as f64,
            NumericSource::Scaled { mantissa, scale } => {
                *mantissa as f64 / 10f64.powi(*scale as i32)
            }
            NumericSource::WideDecimal { approx, .. } => *approx,
            NumericSource::Float(f) => *f,
        }
    }

    /// Sign of the value before any truncation toward zero.
    pub(crate) fn is_negative(&self) -> bool {
        match self {
            NumericSource::Int(v) => *v < 0,
            NumericSource::Scaled { mantissa, .. } => *mantissa < 0,
            NumericSource::WideDecimal {
                negative,
                int_part,
                fraction_dropped,
                ..
            } => *negative && (*int_part != 0 || *fraction_dropped),
            NumericSource::Float(f) => *f < 0.0,
        }
    }

    /// Value truncated toward zero plus whether a fractional part was dropped.
    /// `None` when the value cannot be represented as an integer at all.
    pub(crate) fn to_i128_truncating(self) -> Option<(i128, bool)> {
        match self {
            NumericSource::Int(v) => Some((v, false)),
            NumericSource::Scaled { mantissa, scale } => {
                // Past 10^38 the divisor exceeds every representable mantissa, so
                // the quotient is zero and the whole value is the dropped fraction.
                let Some(divisor) = 10i128.checked_pow(scale) else {
                    return Some((0, mantissa != 0));
                };
                Some((mantissa / divisor, mantissa % divisor != 0))
            }
            NumericSource::WideDecimal {
                int_part,
                fraction_dropped,
                ..
            } => Some((int_part, fraction_dropped)),
            NumericSource::Float(f) => {
                if !f.is_finite() || !(-1.7e38..=1.7e38).contains(&f) {
                    return None;
                }
                Some((f.trunc() as i128, f.fract() != 0.0))
            }
        }
    }
}

/// Parses a plain decimal literal (`-12.34`, `+7`, `.5`) into an exact
/// [`NumericSource::Scaled`]. Exponent forms are left to the `f64` fallback.
///
/// Private, and deliberately does not trim: `parse_numeric_text` has already
/// applied the blanks-only rule, and `str::trim` here would silently re-admit
/// the whole Unicode whitespace set that rule exists to reject.
fn parse_decimal_literal(text: &str) -> Option<NumericSource> {
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (int_digits, frac_digits) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_digits.is_empty() && frac_digits.is_empty() {
        return None;
    }
    if !int_digits
        .bytes()
        .chain(frac_digits.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mantissa: i128 = format!("{int_digits}{frac_digits}").parse().ok()?;
    Some(NumericSource::Scaled {
        mantissa: if negative { -mantissa } else { mantissa },
        scale: frac_digits.len() as u32,
    })
}

/// Narrows an `i128` to a target integer type, reporting an out-of-range value
/// as [`ConvError::OutOfRange`] rather than wrapping.
pub(crate) fn narrow_i128<T: TryFrom<i128>>(v: i128) -> Result<T, ConvError> {
    T::try_from(v).map_err(|_| ConvError::OutOfRange)
}

/// Narrows an `f64` to `real`, for either direction.
///
/// One function because msodbcsql has one arm: `SQL_C_FLOAT` and `SQL_REAL` are
/// both `7`, so `case SQL_C_FLOAT` (`sqlccnvt.cpp:5519`) serves a `real`
/// parameter and a `SQL_C_FLOAT` fetch buffer alike - the same identifier
/// collision that makes [`parse_numeric_text`] shared. Its rule is symmetric:
///
/// ```cpp
/// if (Temp > FLT_MAX || Temp < -FLT_MAX ||
///     (Temp > 0.0 && Temp < FLT_MIN) ||
///     (Temp < 0.0 && Temp > -FLT_MIN))
///     Error = CVT_PREC;            // IDS_22_003
/// ```
///
/// So a non-zero magnitude *below* `f32::MIN_POSITIVE` is `22003` rather than a
/// silent flush to zero - the half that is easy to miss.
///
/// The comparisons are left to reproduce the C semantics on their own rather
/// than being guarded by a finiteness check. `Temp` is a `DOUBLE`
/// (`sqlccnvt.cpp:5327`) and `FLT_MAX` promotes to one, so `+INF > FLT_MAX`
/// holds and an infinity is `22003`; a NaN compares false four times and
/// passes. Zero passes on the `Temp > 0.0` / `Temp < 0.0` guards.
pub(crate) fn narrow_f64_to_f32(v: f64) -> Result<f32, ConvError> {
    let magnitude = v.abs();
    if magnitude > f64::from(f32::MAX) || (v != 0.0 && magnitude < f64::from(f32::MIN_POSITIVE)) {
        return Err(ConvError::OutOfRange);
    }
    Ok(v as f32)
}

/// Interprets text as a number, for either direction (fetch & params).
///
/// Both directions must agree on what counts as a number, because msodbcsql
/// answers the question once: `Convert` dispatches a character source to
/// `ConvertToFixed`, whose `case SQL_C_CHAR` arm runs `CharToBigint`
/// (`sqlccnvt.cpp:5088`). `SQL_C_CHAR` and `SQL_CHAR` are both `1`, so a
/// character *column* and a character *application buffer* enter that arm
/// indistinguishably - the same parser serves `SQLGetData` and
/// `SQLBindParameter`.
///
/// Severity is the caller's business, not this function's: a dropped fraction is
/// a `01S07` warning outbound and a `22001` error inbound, so callers take
/// [`NumericSource::to_i128_truncating`]'s flag and decide.
///
/// Only exponent forms and integers too wide for an exact mantissa go through
/// `f64`, so a *decimal* carrying more significant digits than a double holds
/// still reports its fraction - `CharToBigint` walks the digits one at a time
/// and flags any non-zero one past the scale (`sqlccnvt.cpp:7823`) however long
/// the literal is.
pub(crate) fn parse_numeric_text(text: &str) -> Result<NumericSource, ConvError> {
    // An embedded NUL ends the number. `CharToBigint` loops
    // `while (len < srclen && charstr[len] != '\0')` (`sqlccnvt.cpp:7800`), so an
    // application that passes `strlen + 1` as the length still parses.
    //
    // A *leading* NUL diverges: that loop exits at once leaving `*pValue = 0`
    // and `CVT_NO_ERROR`, where the empty remainder is `22018` here. Left as an
    // error - a length-prefixed buffer starting with NUL carries no number, and
    // msodbcsql itself reports `22018` for the same buffer under `SQL_NTS`.
    let text = text.split('\0').next().unwrap_or("");

    // Only blanks are padding: `CharToBigint` trims `' '` alone
    // (`sqlccnvt.cpp:7777`). Everything else fails downstream on its own - the
    // digit-only checks in `parse_decimal_literal` and `parse_wide_decimal`
    // reject it, and `f64::from_str` admits no whitespace at all - so a tab, an
    // interior blank or a non-breaking space lands on `22018` without a guard
    // here. `a_non_numeric_literal_is_22018` is what holds that.
    let trimmed = text.trim_matches(' ');

    if let Some(source) = parse_decimal_literal(trimmed) {
        return Ok(source);
    }

    // `parse_decimal_literal` gives up once the digits overflow an exact
    // mantissa, but the fraction still has to be reported rather than rounded
    // away by `f64`.
    if let Some(source) = parse_wide_decimal(trimmed) {
        return Ok(source);
    }

    // Exponent forms, and integers too wide for an exact mantissa, fall back to
    // `f64`. msodbcsql routes the same way and for the same reason: `Convert`
    // scans for an `e`/`E` (`sqlccnvt.cpp:5092`) and sends a plain literal to
    // `CharToBigint` (`:5109`, which walks digits and flags a dropped fraction)
    // but an exponent literal to `CharToDouble` (`:5118`, which keeps only what
    // the double holds).
    //
    // That makes the answer depend on the spelling, in both drivers: `"1e-400"`
    // underflows to `0.0` and reports no dropped fraction, where the same value
    // written out as `"0." + 400 zeros + "1"` reports one. Deliberately left
    // alone - recovering the fraction from the text would be more self-
    // consistent but would diverge from msodbcsql on both directions at once.
    // The same routing is why `"-1e-400"` is `-0.0` and so not negative.
    match trimmed.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(NumericSource::Float(f)),
        // Rust folds overflow into `Ok(inf)`, but msodbcsql's `VarR8FromStr`
        // reports `DISP_E_OVERFLOW` -> 22003 and keeps the cast error for text
        // that is not a number at all. Digits present means it was numeric.
        Ok(_) if trimmed.bytes().any(|b| b.is_ascii_digit()) => Err(ConvError::OutOfRange),
        // "inf" / "infinity" / "nan" parse in Rust but are not SQL literals.
        _ => Err(ConvError::InvalidCharacterValue),
    }
}

/// A plain decimal whose digits exceed an exact mantissa. Keeps the `f64`
/// approximation for float targets and the integer part plus a dropped-fraction
/// flag for integer targets, because `f64` alone would round the fraction away
/// and report nothing. `None` when the integer part itself overflows, which
/// leaves the `f64` fallback to report `22003`.
fn parse_wide_decimal(text: &str) -> Option<NumericSource> {
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (int_digits, frac_digits) = body.split_once('.')?;
    if int_digits.is_empty() && frac_digits.is_empty() {
        return None;
    }
    if !int_digits
        .bytes()
        .chain(frac_digits.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let int_part: i128 = if int_digits.is_empty() {
        0
    } else {
        int_digits.parse().ok()?
    };
    let approx: f64 = text.parse().ok()?;
    Some(NumericSource::WideDecimal {
        approx,
        negative,
        int_part: if negative { -int_part } else { int_part },
        fraction_dropped: frac_digits.bytes().any(|b| b != b'0'),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `real` range check is symmetric, and both directions get the same
    /// answer because they call this one function. Fetch used to check only the
    /// overflow half and silently flushed a denormal to zero.
    #[test]
    fn the_real_range_check_is_symmetric() {
        for v in [1e39f64, -1e39, 1e-40, -1e-40] {
            assert_eq!(
                narrow_f64_to_f32(v),
                Err(ConvError::OutOfRange),
                "value {v}"
            );
        }

        // The boundaries themselves are representable, and zero is not
        // underflow: msodbcsql guards the underflow arms with `Temp > 0.0` /
        // `Temp < 0.0` (`sqlccnvt.cpp:5520`).
        for v in [
            0.0f64,
            -0.0,
            f64::from(f32::MAX),
            f64::from(f32::MIN_POSITIVE),
            -f64::from(f32::MIN_POSITIVE),
        ] {
            assert!(narrow_f64_to_f32(v).is_ok(), "value {v}");
        }
    }

    /// An infinity exceeds `FLT_MAX` and is rejected; a NaN compares false
    /// against every bound and passes. Both fall out of the comparisons rather
    /// than being special-cased, which is what msodbcsql does.
    #[test]
    fn an_infinity_is_out_of_range_but_a_nan_is_not() {
        assert_eq!(narrow_f64_to_f32(f64::INFINITY), Err(ConvError::OutOfRange));
        assert_eq!(
            narrow_f64_to_f32(f64::NEG_INFINITY),
            Err(ConvError::OutOfRange)
        );
        assert!(narrow_f64_to_f32(f64::NAN).unwrap().is_nan());
    }

    #[test]
    fn plain_decimal_literals_parse_exactly() {
        assert_eq!(
            parse_decimal_literal("-12.34"),
            Some(NumericSource::Scaled {
                mantissa: -1234,
                scale: 2
            })
        );
        assert_eq!(
            parse_decimal_literal("+7"),
            Some(NumericSource::Scaled {
                mantissa: 7,
                scale: 0
            })
        );
        assert_eq!(
            parse_numeric_text(" .5 ").unwrap(),
            NumericSource::Scaled {
                mantissa: 5,
                scale: 1
            }
        );
        // A leading zero in the fraction is absorbed into the mantissa while
        // scale still counts both digits.
        assert_eq!(
            parse_decimal_literal("-0.01"),
            Some(NumericSource::Scaled {
                mantissa: -1,
                scale: 2
            })
        );
    }

    #[test]
    fn non_decimal_text_is_rejected() {
        // Exponent forms are deliberately left to the f64 fallback.
        assert_eq!(parse_decimal_literal("1e3"), None);
        assert_eq!(parse_decimal_literal("abc"), None);
        assert_eq!(parse_decimal_literal(""), None);
        assert_eq!(parse_decimal_literal("-"), None);
    }

    /// A NUL ends the number in both directions, because `CharToBigint`'s loop
    /// stops there whichever way the data moves (`sqlccnvt.cpp:7800`). On a
    /// bound buffer that forgives an application passing `strlen + 1`; on a
    /// fetched column it means an embedded NUL truncates rather than rejecting.
    #[test]
    fn a_nul_ends_the_number_in_either_direction() {
        assert_eq!(
            parse_numeric_text("-42\u{0}").unwrap().to_i128_truncating(),
            Some((-42, false))
        );
        assert_eq!(
            parse_numeric_text("1\u{0}2").unwrap().to_i128_truncating(),
            Some((1, false))
        );
        // Nothing before the NUL is no number at all.
        assert_eq!(
            parse_numeric_text("\u{0}12"),
            Err(ConvError::InvalidCharacterValue)
        );
    }

    /// The sign of a wide literal cannot come from `approx`: a magnitude below
    /// `f64::MIN_POSITIVE` underflows to `-0.0`, and `-0.0 < 0.0` is false, so
    /// `SQL_C_BIT` would write 0 instead of reporting an out-of-range negative.
    #[test]
    fn a_wide_decimal_keeps_its_sign_through_underflow() {
        let tiny = format!("-0.{}{}", "0".repeat(400), "9".repeat(39));
        let source = parse_numeric_text(&tiny).unwrap();
        assert_eq!(source.as_f64(), -0.0, "precondition: underflows");
        assert!(source.is_negative());

        // A negative zero is not negative: nothing non-zero survives it.
        let zero = format!("-0.{}", "0".repeat(40));
        assert!(!parse_numeric_text(&zero).unwrap().is_negative());
    }

    /// The answer depends on the spelling, and that is msodbcsql's behaviour,
    /// not an oversight: `Convert` sends a plain literal to `CharToBigint`,
    /// which walks digits and flags a dropped fraction, and an exponent literal
    /// to `CharToDouble`, which keeps only what the double holds
    /// (`sqlccnvt.cpp:5092`, `:5109`, `:5118`). An underflowing exponent is
    /// therefore exactly zero, with no fraction to report and no sign.
    #[test]
    fn an_underflowing_exponent_loses_its_fraction_as_msodbcsql_does() {
        let plain = format!("0.{}1", "0".repeat(400));
        assert_eq!(
            parse_numeric_text(&plain).unwrap().to_i128_truncating(),
            Some((0, true)),
            "a plain literal keeps the digit walk"
        );
        assert_eq!(
            parse_numeric_text("1e-400").unwrap().to_i128_truncating(),
            Some((0, false)),
            "an exponent literal is whatever the double holds"
        );

        // Subnormal but non-zero: the double still carries a fraction.
        assert_eq!(
            parse_numeric_text("1e-320").unwrap().to_i128_truncating(),
            Some((0, true))
        );

        // -0.0 is not negative, for the same reason.
        assert!(!parse_numeric_text("-1e-400").unwrap().is_negative());
    }

    /// A literal past an exact mantissa still has to reach a float target at
    /// full precision. Reducing it to an integer-plus-sentinel serves the
    /// integer path but would hand `SQL_C_DOUBLE` about 1.1 for this value.
    #[test]
    fn a_wide_decimal_keeps_its_value_for_float_targets() {
        let wide = "1.234567890123456789012345678901234567890";
        let source = parse_numeric_text(wide).unwrap();
        assert!(
            (source.as_f64() - 1.234_567_890_123_456_7).abs() < 1e-15,
            "got {}",
            source.as_f64()
        );
        assert_eq!(source.to_i128_truncating(), Some((1, true)));
        assert!(!source.is_negative());

        let negative = parse_numeric_text("-0.500000000000000000000000000000000000000001").unwrap();
        assert!((negative.as_f64() + 0.5).abs() < 1e-15);
        assert_eq!(negative.to_i128_truncating(), Some((0, true)));
        assert!(negative.is_negative());
    }

    #[test]
    fn truncation_toward_zero_reports_dropped_fraction() {
        let n = NumericSource::Scaled {
            mantissa: -1234,
            scale: 2,
        };
        assert_eq!(n.to_i128_truncating(), Some((-12, true)));
        assert!(n.is_negative());

        let exact = NumericSource::Scaled {
            mantissa: 1200,
            scale: 2,
        };
        assert_eq!(exact.to_i128_truncating(), Some((12, false)));
    }

    /// A scale past 10^38 overflows `checked_pow`; the whole value is then the
    /// dropped fraction rather than a panic.
    #[test]
    fn scale_beyond_i128_pow_yields_zero_with_truncation() {
        let n = NumericSource::Scaled {
            mantissa: 5,
            scale: 40,
        };
        assert_eq!(n.to_i128_truncating(), Some((0, true)));
    }

    #[test]
    fn non_finite_and_oversized_floats_are_unrepresentable() {
        assert_eq!(NumericSource::Float(f64::NAN).to_i128_truncating(), None);
        assert_eq!(
            NumericSource::Float(f64::INFINITY).to_i128_truncating(),
            None
        );
        assert_eq!(NumericSource::Float(1e39).to_i128_truncating(), None);
    }

    #[test]
    fn narrowing_out_of_range_is_reported_not_wrapped() {
        assert_eq!(narrow_i128::<i8>(127), Ok(127i8));
        assert_eq!(narrow_i128::<i8>(128), Err(ConvError::OutOfRange));
        assert_eq!(narrow_i128::<u8>(-1), Err(ConvError::OutOfRange));
    }

    #[test]
    fn as_f64_covers_every_variant() {
        assert_eq!(NumericSource::Int(-3).as_f64(), -3.0);
        assert_eq!(
            NumericSource::Scaled {
                mantissa: 1234,
                scale: 2
            }
            .as_f64(),
            12.34
        );
        assert_eq!(NumericSource::Float(1.5).as_f64(), 1.5);
    }
}
