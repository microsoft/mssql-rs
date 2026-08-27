// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `--driver-logging-level` and `--trace-file`.
//!
//! The driver emits `tracing` events; these two options decide how much of that
//! is kept and where it goes. Diagnostics are deliberately never mixed into the
//! results stream, which scripts parse.

use std::fs::File;
use std::sync::Mutex;

use crate::cli::validate::Options;
use crate::messages;

/// go-sqlcmd takes a number rather than a level name. Anything above the
/// defined range is treated as the most verbose setting.
fn level_for(value: i64) -> Option<tracing_level::Level> {
    use tracing_level::Level;
    Some(match value {
        n if n <= 0 => return None,
        1 => Level::Error,
        2 => Level::Warn,
        3 => Level::Info,
        4 => Level::Debug,
        _ => Level::Trace,
    })
}

/// A minimal level enum, kept local so the crate does not take a tracing
/// dependency purely to name five constants.
pub mod tracing_level {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Level {
        Error,
        Warn,
        Info,
        Debug,
        Trace,
    }

    impl Level {
        pub fn as_filter(self) -> &'static str {
            match self {
                Level::Error => "error",
                Level::Warn => "warn",
                Level::Info => "info",
                Level::Debug => "debug",
                Level::Trace => "trace",
            }
        }
    }
}

/// Where diagnostics are written once started.
static TRACE_FILE: Mutex<Option<File>> = Mutex::new(None);

/// Applies the diagnostic options. Returns the message to print if the trace
/// file cannot be opened, since carrying on would silently discard the output
/// the caller asked for.
pub fn start(options: &Options) -> Result<(), String> {
    if let Some(path) = &options.trace_file {
        match File::create(path) {
            Ok(file) => *TRACE_FILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(file),
            Err(_) => return Err(messages::invalid_filename(path)),
        }
    }

    if let Some(level) = level_for(options.driver_logging_level) {
        // The driver reads its filter from the environment, so setting it here
        // reaches the driver without this crate depending on a subscriber.
        // SAFETY: called once, before any thread is spawned.
        unsafe {
            std::env::set_var("RUST_LOG", level.as_filter());
        }
    }

    Ok(())
}

/// Writes one diagnostic line, if tracing was asked for.
pub fn write(line: &str) {
    use std::io::Write;
    if let Ok(mut guard) = TRACE_FILE.lock()
        && let Some(file) = guard.as_mut()
    {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_below_mean_no_tracing() {
        assert_eq!(level_for(0), None);
        assert_eq!(level_for(-1), None);
    }

    #[test]
    fn levels_climb_with_the_number_and_saturate() {
        use tracing_level::Level;
        assert_eq!(level_for(1), Some(Level::Error));
        assert_eq!(level_for(3), Some(Level::Info));
        assert_eq!(level_for(5), Some(Level::Trace));
        assert_eq!(level_for(99), Some(Level::Trace));
    }

    #[test]
    fn filter_names_are_the_ones_the_driver_expects() {
        use tracing_level::Level;
        assert_eq!(Level::Error.as_filter(), "error");
        assert_eq!(Level::Trace.as_filter(), "trace");
    }
}
