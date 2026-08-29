// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `-R` — formatting money, numbers and timestamps with the client's regional
//! settings.
//!
//! Only ODBC `sqlcmd` implements this; go-sqlcmd accepts the flag and ignores
//! it. The reference goes through the platform's own locale services, so this
//! does too rather than carrying a locale database: `GetCurrencyFormatEx` and
//! friends on Windows, `localeconv` and `strftime` elsewhere. That keeps the
//! output tied to whatever the machine is actually configured for, which is the
//! whole point of the flag.
//!
//! Measured against the reference, `-R` changes:
//!
//! | type | plain | `-R` (en-US) |
//! |---|---|---|
//! | `money`, `smallmoney` | `1234.5600` | `$1,234.56` |
//! | `decimal`, `numeric` | `1234.5600` | `1,234.56` |
//! | `date` | `2026-03-04` | `3/4/2026` |
//! | `datetime`, `smalldatetime` | `2026-03-04 13:45:06.000` | `3/4/2026 1:45:06 PM` |
//!
//! and leaves `int`, `bigint`, `float` and `real` alone.
//!
//! Two defects in the reference are deliberately **not** reproduced, because
//! both put something in front of a user that cannot be meant:
//!
//! - `datetime2` with a fractional-seconds scale renders as
//!   `1:45:06.%07lu PM` — an unsubstituted `printf` specifier.
//! - `time` fails outright on Windows with *"Internal error at
//!   LocalizeTimestampData"*, though it works on Linux.
//!
//! Here both format the way the neighbouring types do.

/// A civil date and time, already split into fields by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Formats a decimal string as currency in the current locale.
///
/// `digits` is the plain rendering, e.g. `-1234.5600`. Returns `None` when the
/// platform cannot answer, so the caller can fall back to the plain text rather
/// than print something misleading.
pub fn currency(digits: &str) -> Option<String> {
    platform::currency(digits)
}

/// Formats a decimal string as a number in the current locale, applying the
/// locale's grouping and decimal separator.
pub fn number(digits: &str) -> Option<String> {
    platform::number(digits)
}

/// The locale's short date, e.g. `3/4/2026`.
pub fn short_date(ts: Timestamp) -> Option<String> {
    platform::short_date(ts)
}

/// The locale's long time, e.g. `1:45:06 PM`.
pub fn long_time(ts: Timestamp) -> Option<String> {
    platform::long_time(ts)
}

/// The locale's short date and long time, separated by a space — the shape the
/// reference prints for `datetime`.
pub fn date_and_time(ts: Timestamp) -> Option<String> {
    Some(format!("{} {}", short_date(ts)?, long_time(ts)?))
}

#[cfg(windows)]
mod platform {
    use super::Timestamp;

    // NUL-terminated UTF-16, which every `*W` entry point wants.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn from_wide(buffer: &[u16]) -> String {
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    }

    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    unsafe extern "system" {
        fn GetCurrencyFormatEx(
            locale: *const u16,
            flags: u32,
            value: *const u16,
            format: *const core::ffi::c_void,
            out: *mut u16,
            out_len: i32,
        ) -> i32;
        fn GetNumberFormatEx(
            locale: *const u16,
            flags: u32,
            value: *const u16,
            format: *const core::ffi::c_void,
            out: *mut u16,
            out_len: i32,
        ) -> i32;
        fn GetDateFormatEx(
            locale: *const u16,
            flags: u32,
            date: *const SystemTime,
            format: *const u16,
            out: *mut u16,
            out_len: i32,
            calendar: *const u16,
        ) -> i32;
        fn GetTimeFormatEx(
            locale: *const u16,
            flags: u32,
            time: *const SystemTime,
            format: *const u16,
            out: *mut u16,
            out_len: i32,
        ) -> i32;
    }

    /// `LOCALE_NAME_USER_DEFAULT`: whatever the signed-in user has configured.
    const USER_DEFAULT: *const u16 = std::ptr::null();
    const DATE_SHORTDATE: u32 = 0x0000_0001;
    /// Windows wants a plain `-1234.56`: no grouping, `.` as the decimal point,
    /// which is exactly how the values arrive here.
    fn call_format(
        digits: &str,
        f: unsafe extern "system" fn(
            *const u16,
            u32,
            *const u16,
            *const core::ffi::c_void,
            *mut u16,
            i32,
        ) -> i32,
    ) -> Option<String> {
        let value = wide(digits);
        let mut out = [0u16; 128];
        // SAFETY: `value` is NUL-terminated and `out` is sized by `len()`.
        let written = unsafe {
            f(
                USER_DEFAULT,
                0,
                value.as_ptr(),
                std::ptr::null(),
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        (written > 0).then(|| from_wide(&out))
    }

    pub(super) fn currency(digits: &str) -> Option<String> {
        call_format(digits, GetCurrencyFormatEx)
    }

    pub(super) fn number(digits: &str) -> Option<String> {
        call_format(digits, GetNumberFormatEx)
    }

    fn system_time(ts: Timestamp) -> Option<SystemTime> {
        Some(SystemTime {
            year: u16::try_from(ts.year).ok()?,
            month: ts.month as u16,
            day_of_week: 0,
            day: ts.day as u16,
            hour: ts.hour as u16,
            minute: ts.minute as u16,
            second: ts.second as u16,
            milliseconds: 0,
        })
    }

    pub(super) fn short_date(ts: Timestamp) -> Option<String> {
        let st = system_time(ts)?;
        let mut out = [0u16; 128];
        // SAFETY: `st` outlives the call and `out` is sized by `len()`.
        let written = unsafe {
            GetDateFormatEx(
                USER_DEFAULT,
                DATE_SHORTDATE,
                &st,
                std::ptr::null(),
                out.as_mut_ptr(),
                out.len() as i32,
                std::ptr::null(),
            )
        };
        (written > 0).then(|| from_wide(&out))
    }

    pub(super) fn long_time(ts: Timestamp) -> Option<String> {
        let st = system_time(ts)?;
        let mut out = [0u16; 128];
        // SAFETY: as above.
        let written = unsafe {
            GetTimeFormatEx(
                USER_DEFAULT,
                0,
                &st,
                std::ptr::null(),
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        (written > 0).then(|| from_wide(&out))
    }
}

#[cfg(not(windows))]
mod platform {
    use super::Timestamp;
    use std::ffi::{CStr, CString};
    use std::sync::Once;

    #[repr(C)]
    struct Lconv {
        decimal_point: *const libc_char,
        thousands_sep: *const libc_char,
        grouping: *const libc_char,
        int_curr_symbol: *const libc_char,
        currency_symbol: *const libc_char,
        mon_decimal_point: *const libc_char,
        mon_thousands_sep: *const libc_char,
        mon_grouping: *const libc_char,
        positive_sign: *const libc_char,
        negative_sign: *const libc_char,
        int_frac_digits: u8,
        frac_digits: u8,
        p_cs_precedes: u8,
        p_sep_by_space: u8,
        n_cs_precedes: u8,
        n_sep_by_space: u8,
        p_sign_posn: u8,
        n_sign_posn: u8,
    }

    #[allow(non_camel_case_types)]
    type libc_char = i8;

    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const libc_char,
    }

    const LC_ALL: i32 = 6;
    /// `CHAR_MAX` in a `struct lconv` field means "this locale does not say".
    const CHAR_MAX: u8 = 127;

    unsafe extern "C" {
        fn setlocale(category: i32, locale: *const libc_char) -> *mut libc_char;
        fn localeconv() -> *mut Lconv;
        fn strftime(
            out: *mut libc_char,
            max: usize,
            format: *const libc_char,
            tm: *const Tm,
        ) -> usize;
    }

    /// The C runtime starts every process in the `C` locale whatever the
    /// environment says, so `-R` would otherwise never see the user's settings.
    /// Done once, and only when `-R` is actually used.
    fn ensure_locale() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let empty = CString::new("").expect("no interior NUL");
            // SAFETY: an empty locale name means "read the environment".
            unsafe { setlocale(LC_ALL, empty.as_ptr()) };
        });
    }

    fn conv_str(p: *const libc_char) -> String {
        if p.is_null() {
            return String::new();
        }
        // SAFETY: `localeconv` returns NUL-terminated strings owned by the CRT.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    /// Splits `-1234.5600` into its sign, integer digits and fraction.
    fn parts(digits: &str) -> (bool, String, String) {
        let negative = digits.starts_with('-');
        let body = digits.trim_start_matches(['-', '+']);
        match body.split_once('.') {
            Some((whole, frac)) => (negative, whole.to_string(), frac.to_string()),
            None => (negative, body.to_string(), String::new()),
        }
    }

    /// Applies the locale's grouping rule, which is a list of group sizes read
    /// right to left; the last entry repeats.
    fn group(whole: &str, sep: &str, grouping: &str) -> String {
        if sep.is_empty() || grouping.is_empty() {
            return whole.to_string();
        }
        let sizes: Vec<usize> = grouping
            .bytes()
            .take_while(|b| *b != 0 && *b != i8::MAX as u8)
            .map(|b| b as usize)
            .filter(|n| *n > 0)
            .collect();
        if sizes.is_empty() {
            return whole.to_string();
        }

        let mut out: Vec<String> = Vec::new();
        let mut rest: &str = whole;
        let mut index = 0;
        loop {
            let size = sizes[index.min(sizes.len() - 1)];
            if rest.len() <= size {
                out.push(rest.to_string());
                break;
            }
            let split = rest.len() - size;
            out.push(rest[split..].to_string());
            rest = &rest[..split];
            index += 1;
        }
        out.reverse();
        out.join(sep)
    }

    /// Rounds `frac` to `places`, carrying into `whole` when it rolls over.
    fn round_to(whole: &str, frac: &str, places: usize) -> (String, String) {
        if frac.len() <= places {
            let mut padded = frac.to_string();
            while padded.len() < places {
                padded.push('0');
            }
            return (whole.to_string(), padded);
        }
        let keep = &frac[..places];
        let next = frac.as_bytes()[places];
        if next < b'5' {
            return (whole.to_string(), keep.to_string());
        }
        // Round half up, which is what both platforms' formatters do.
        let combined = format!("{whole}{keep}");
        let bumped: String = match combined.parse::<u128>() {
            Ok(n) => (n + 1).to_string(),
            Err(_) => return (whole.to_string(), keep.to_string()),
        };
        let bumped = if bumped.len() <= places {
            format!("{:0>width$}", bumped, width = places + 1)
        } else {
            bumped
        };
        let split = bumped.len() - places;
        (bumped[..split].to_string(), bumped[split..].to_string())
    }

    fn format_with(digits: &str, monetary: bool) -> Option<String> {
        ensure_locale();
        // SAFETY: `localeconv` returns a pointer to CRT-owned static storage.
        let lc = unsafe { localeconv().as_ref()? };

        let (decimal, sep, grouping) = if monetary {
            (
                conv_str(lc.mon_decimal_point),
                conv_str(lc.mon_thousands_sep),
                conv_str(lc.mon_grouping),
            )
        } else {
            (
                conv_str(lc.decimal_point),
                conv_str(lc.thousands_sep),
                conv_str(lc.grouping),
            )
        };
        let decimal = if decimal.is_empty() {
            ".".to_string()
        } else {
            decimal
        };

        let (negative, whole, frac) = parts(digits);
        // POSIX gives `frac_digits` as CHAR_MAX in the `C` locale, meaning
        // "unspecified"; the reference renders that as no decimal places at
        // all, which is why `1234.56` comes back as `1235` there. The same
        // count drives plain numbers, matching the reference on both.
        let places = if lc.frac_digits >= CHAR_MAX {
            0
        } else {
            lc.frac_digits as usize
        };
        let (whole, frac) = round_to(&whole, &frac, places);

        let mut out = String::new();
        if monetary {
            let symbol = conv_str(lc.currency_symbol);
            if negative {
                let sign = conv_str(lc.negative_sign);
                out.push_str(if sign.is_empty() { "-" } else { &sign });
            }
            // `CHAR_MAX` means the locale does not say; put the symbol first,
            // which is the commoner convention and matches the `C` locale's
            // empty symbol either way.
            let symbol_first = lc.p_cs_precedes != 0;
            if symbol_first {
                out.push_str(&symbol);
            }
            out.push_str(&group(&whole, &sep, &grouping));
            if !frac.is_empty() {
                out.push_str(&decimal);
                out.push_str(&frac);
            }
            if !symbol_first {
                out.push_str(&symbol);
            }
        } else {
            if negative {
                out.push('-');
            }
            out.push_str(&group(&whole, &sep, &grouping));
            if !frac.is_empty() {
                out.push_str(&decimal);
                out.push_str(&frac);
            }
        }
        Some(out)
    }

    pub(super) fn currency(digits: &str) -> Option<String> {
        format_with(digits, true)
    }

    pub(super) fn number(digits: &str) -> Option<String> {
        format_with(digits, false)
    }

    fn tm(ts: Timestamp) -> Tm {
        Tm {
            tm_sec: ts.second as i32,
            tm_min: ts.minute as i32,
            tm_hour: ts.hour as i32,
            tm_mday: ts.day as i32,
            tm_mon: ts.month as i32 - 1,
            tm_year: ts.year - 1900,
            tm_wday: 0,
            tm_yday: 0,
            tm_isdst: -1,
            tm_gmtoff: 0,
            tm_zone: std::ptr::null(),
        }
    }

    fn strftime_with(ts: Timestamp, format: &str) -> Option<String> {
        ensure_locale();
        let tm = tm(ts);
        let format = CString::new(format).ok()?;
        let mut buffer = vec![0i8; 128];
        // SAFETY: `buffer` is sized by `len()`, and `tm` outlives the call.
        let written = unsafe {
            strftime(
                buffer.as_mut_ptr(),
                buffer.len(),
                format.as_ptr(),
                &tm as *const Tm,
            )
        };
        if written == 0 {
            return None;
        }
        // SAFETY: `strftime` NUL-terminates on success.
        Some(
            unsafe { CStr::from_ptr(buffer.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    pub(super) fn short_date(ts: Timestamp) -> Option<String> {
        strftime_with(ts, "%x")
    }

    pub(super) fn long_time(ts: Timestamp) -> Option<String> {
        strftime_with(ts, "%X")
    }

    #[cfg(test)]
    mod tests {
        use super::{group, parts, round_to};

        #[test]
        fn a_value_splits_into_sign_whole_and_fraction() {
            assert_eq!(
                parts("-1234.5600"),
                (true, "1234".to_string(), "5600".to_string())
            );
            assert_eq!(parts("42"), (false, "42".to_string(), String::new()));
        }

        #[test]
        fn grouping_reads_right_to_left_and_repeats_the_last_size() {
            // "\u{3}" is the usual three-digit grouping.
            assert_eq!(group("1234567", ",", "\u{3}"), "1,234,567");
            assert_eq!(group("123", ",", "\u{3}"), "123");
            // The Indian convention: three, then twos.
            assert_eq!(group("1234567", ",", "\u{3}\u{2}"), "12,34,567");
        }

        #[test]
        fn grouping_is_skipped_when_the_locale_has_none() {
            assert_eq!(group("1234567", "", "\u{3}"), "1234567");
            assert_eq!(group("1234567", ",", ""), "1234567");
        }

        #[test]
        fn rounding_pads_when_there_are_too_few_digits() {
            assert_eq!(
                round_to("12", "5", 3),
                ("12".to_string(), "500".to_string())
            );
        }

        #[test]
        fn rounding_truncates_below_a_half() {
            assert_eq!(
                round_to("12", "3400", 2),
                ("12".to_string(), "34".to_string())
            );
        }

        #[test]
        fn rounding_goes_up_at_a_half() {
            assert_eq!(
                round_to("12", "3450", 2),
                ("12".to_string(), "35".to_string())
            );
        }

        #[test]
        fn rounding_carries_into_the_whole_part() {
            // This is the `C`-locale money case: 1234.56 with no decimal places.
            assert_eq!(
                round_to("1234", "56", 0),
                ("1235".to_string(), String::new())
            );
            assert_eq!(round_to("9", "99", 1), ("10".to_string(), "0".to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These see whatever locale the machine is configured for, so they assert
    /// only what holds in every locale. The exact rendering is compared against
    /// the reference by the differential suite, which is where a locale-specific
    /// regression would show up.
    ///
    /// In particular "the digits survive" is *not* invariant: the `C` locale
    /// reports no fraction digits, so `1234.56` legitimately becomes `1235`.
    fn is_plausible(text: &str) -> bool {
        !text.is_empty() && text.chars().any(|c| c.is_ascii_digit())
    }

    #[test]
    fn currency_produces_something_numeric() {
        if let Some(text) = currency("1234.5600") {
            assert!(is_plausible(&text), "currency gave {text:?}");
        }
    }

    #[test]
    fn a_number_produces_something_numeric() {
        if let Some(text) = number("1234.56") {
            assert!(is_plausible(&text), "number gave {text:?}");
        }
    }

    #[test]
    fn a_negative_stays_negative() {
        if let Some(text) = number("-1234.56") {
            // Either a leading sign or the accounting parenthesis form.
            assert!(
                text.starts_with('-') || text.contains('('),
                "sign went missing: {text}"
            );
        }
    }

    #[test]
    fn zero_is_rendered() {
        if let Some(text) = number("0") {
            assert!(text.contains('0'), "zero gave {text:?}");
        }
    }

    #[test]
    fn a_date_carries_its_parts() {
        let ts = Timestamp {
            year: 2026,
            month: 3,
            day: 4,
            hour: 13,
            minute: 45,
            second: 6,
        };
        if let Some(text) = short_date(ts) {
            // Every locale writes the year, in two digits or four.
            assert!(text.contains("26"), "year went missing: {text}");
            assert!(text.contains('4'), "day went missing: {text}");
        }
    }

    #[test]
    fn a_time_carries_its_minutes() {
        let ts = Timestamp {
            year: 2026,
            month: 3,
            day: 4,
            hour: 13,
            minute: 45,
            second: 6,
        };
        if let Some(text) = long_time(ts) {
            // The hour may be 13 or 1 depending on the locale's clock, but the
            // minutes are the same either way.
            assert!(text.contains("45"), "minutes went missing: {text}");
        }
    }

    #[test]
    fn a_date_and_time_joins_both() {
        let ts = Timestamp {
            year: 2026,
            month: 3,
            day: 4,
            hour: 13,
            minute: 45,
            second: 6,
        };
        if let (Some(date), Some(time), Some(both)) =
            (short_date(ts), long_time(ts), date_and_time(ts))
        {
            assert_eq!(both, format!("{date} {time}"));
        }
    }
}
