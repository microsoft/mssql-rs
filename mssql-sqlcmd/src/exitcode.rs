// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Process exit codes.
//!
//! sqlcmd returns 0 on success and 1 for its own failures. `-b` makes a batch
//! error propagate, and `:exit(query)` turns the first cell of the first row
//! into the exit code, with reserved negatives for the cases where it cannot.

pub const SUCCESS: i32 = 0;
pub const FAILURE: i32 = 1;

/// The message state that means "stop now".
///
/// A server message with state 127 ends the session whatever its severity, and
/// whatever `-b` or `-V` say.
pub const TERMINATING_STATE: u8 = 127;

/// The exit code to return after a state-127 message of number `msg_number`.
///
/// Both references return the **message number**, which is why `RAISERROR(14599,
/// 16, 127)` exits 14599 and an ad-hoc `RAISERROR('boom', 16, 127)` exits 50000.
///
/// Unix exit statuses are 8 bits, and the two tools disagree about what to do
/// with that. go-sqlcmd hands the full number to `exit()` and lets the OS
/// truncate — 50000 becomes 80, 14599 becomes 7. msodbcsql instead clamps to a
/// plain failure. All four combinations were measured against the real
/// binaries.
pub fn terminating(msg_number: u32, go_compat: bool) -> i32 {
    if go_compat || cfg!(windows) {
        // `as i32` is the truncation the reference itself relies on; a message
        // number never exceeds `i32::MAX` in practice.
        msg_number as i32
    } else {
        FAILURE
    }
}

/// `:exit(query)` ran but produced no result set.
pub const NO_RESULT: i32 = -100;
/// `:exit(query)` produced a result set with no rows.
pub const NO_ROWS: i32 = -101;
/// The first cell could not be read as a number.
pub const NOT_NUMERIC: i32 = -102;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminating_message_exits_with_its_number() {
        // Measured on Windows against both references.
        assert_eq!(terminating(50000, true), 50000);
        assert_eq!(terminating(14599, true), 14599);
    }

    #[test]
    fn odbc_clamps_on_unix_but_not_on_windows() {
        // msodbcsql returns 1 on Linux whatever the message number, while on
        // Windows it returns the number itself.
        let expected = if cfg!(windows) { 14599 } else { FAILURE };
        assert_eq!(terminating(14599, false), expected);
    }

    #[test]
    fn go_compat_keeps_the_number_on_every_platform() {
        // The OS truncates on Unix -- 50000 & 0xFF == 80 -- which is what
        // go-sqlcmd relies on rather than clamping itself.
        assert_eq!(terminating(50000, true), 50000);
    }
}
