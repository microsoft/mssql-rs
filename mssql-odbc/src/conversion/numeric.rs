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
    Float(f64),
}

impl NumericSource {
    pub(crate) fn as_f64(&self) -> f64 {
        match self {
            NumericSource::Int(v) => *v as f64,
            NumericSource::Scaled { mantissa, scale } => {
                *mantissa as f64 / 10f64.powi(*scale as i32)
            }
            NumericSource::Float(f) => *f,
        }
    }

    /// Sign of the value before any truncation toward zero.
    pub(crate) fn is_negative(&self) -> bool {
        match self {
            NumericSource::Int(v) => *v < 0,
            NumericSource::Scaled { mantissa, .. } => *mantissa < 0,
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
pub(crate) fn parse_decimal_literal(text: &str) -> Option<NumericSource> {
    let t = text.trim();
    let (negative, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
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

#[cfg(test)]
mod tests {
    use super::*;

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
            parse_decimal_literal(" .5 "),
            Some(NumericSource::Scaled {
                mantissa: 5,
                scale: 1
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
