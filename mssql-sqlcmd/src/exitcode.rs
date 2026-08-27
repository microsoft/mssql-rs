// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Process exit codes.
//!
//! sqlcmd returns 0 on success and 1 for its own failures. `-b` makes a batch
//! error propagate, and `:exit(query)` turns the first cell of the first row
//! into the exit code, with reserved negatives for the cases where it cannot.

pub const SUCCESS: i32 = 0;
pub const FAILURE: i32 = 1;

/// `:exit(query)` ran but produced no result set.
pub const NO_RESULT: i32 = -100;
/// `:exit(query)` produced a result set with no rows.
pub const NO_ROWS: i32 = -101;
/// The first cell could not be read as a number.
pub const NOT_NUMERIC: i32 = -102;
