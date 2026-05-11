// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chrono::Local;
use once_cell::sync::OnceCell;
use std::fmt;
use std::fs::{OpenOptions, create_dir_all};
use std::path::PathBuf;
use std::sync::Once;
use tracing::Subscriber;
use tracing_appender::non_blocking;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Registry, layer::SubscriberExt};

static INIT: Once = Once::new();
static GUARD: OnceCell<tracing_appender::non_blocking::WorkerGuard> = OnceCell::new();

// Environment variable names
const ENV_TRACE: &str = "MSSQL_TDS_TRACE";
const ENV_TRACE_LEVEL: &str = "MSSQL_TDS_TRACE_LEVEL";

// Defaults
const DEFAULT_TRACE_LEVEL: &str = "info";
const LOG_FILE_PREFIX: &str = "mssql_tds_trace";

/// Format: <ISO-8601 timestamp>, <thread_id>, <LEVEL>, <target>, <message>
struct LogFormatter;

impl<S, N> FormatEvent<S, N> for LogFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        // Timestamp in ISO-8601 format
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        // Thread ID - extract numeric ID using stack buffer (zero heap allocations)
        // 32 bytes is sufficient for "ThreadId(18446744073709551615)" (max u64 = 20 digits + 10 chars overhead)
        let mut buf = [0u8; 32];
        let thread_id = std::thread::current().id();
        let thread_id_str = {
            use std::io::Write;
            let mut cursor = std::io::Cursor::new(&mut buf[..]);
            write!(&mut cursor, "{:?}", thread_id).ok();
            let len = cursor.position() as usize;
            std::str::from_utf8(&buf[..len])
                .ok()
                .and_then(|s| s.strip_prefix("ThreadId("))
                .and_then(|s| s.strip_suffix(")"))
                .unwrap_or("unknown")
        };

        // Level
        let level = event.metadata().level();

        // Target
        let target = event.metadata().target();

        // Format: timestamp, thread_id, LEVEL, target, message
        write!(
            writer,
            "{}, {}, {}, {}, ",
            timestamp, thread_id_str, level, target
        )?;

        // Message (fields)
        ctx.field_format().format_fields(writer.by_ref(), event)?;

        // Add newline
        writeln!(writer)?;

        Ok(())
    }
}

/// Generate the log file path with timestamp and PID.
/// Always uses <cwd>/mssql_python_logs/ as the log directory.
fn get_trace_log_path() -> Option<PathBuf> {
    let dir = std::env::current_dir().ok()?.join("mssql_python_logs");

    // Create directory if it doesn't exist
    if !dir.exists()
        && let Err(e) = create_dir_all(&dir)
    {
        eprintln!("[mssql-py-core] ERROR: Could not create log directory {dir:?}: {e}");
        return None;
    }

    let timestamp = Local::now().format("%Y%m%d%H%M%S");
    let pid = std::process::id();
    let filename = format!("{LOG_FILE_PREFIX}_{timestamp}_{pid}.log");

    Some(dir.join(filename))
}

/// Get the EnvFilter from the environment variable or default to info.
fn get_trace_filter() -> EnvFilter {
    let filter = std::env::var(ENV_TRACE_LEVEL).unwrap_or_else(|_| DEFAULT_TRACE_LEVEL.to_string());

    // Only accept the five valid simple log levels
    let valid_levels = ["error", "warn", "info", "debug", "trace"];
    if !valid_levels.contains(&filter.to_lowercase().as_str()) {
        eprintln!(
            "[mssql-py-core] ERROR: Invalid {ENV_TRACE_LEVEL} value '{filter}'. Valid levels: error, warn, info, debug, trace. Falling back to 'info'."
        );
        return EnvFilter::new("info");
    }

    EnvFilter::new(&filter)
}

/// Initialize tracing if MSSQL_TDS_TRACE is set to true.
/// This is called automatically when the Python module is loaded.
pub(crate) fn init_tracing() {
    let enable_trace = std::env::var(ENV_TRACE)
        .unwrap_or_else(|_| "false".to_string())
        .to_lowercase()
        .parse::<bool>()
        .unwrap_or(false);

    if enable_trace {
        let mut success = false;
        INIT.call_once(|| {
            match get_trace_log_path() {
                Some(log_path) => {
                    match OpenOptions::new().create(true).append(true).open(&log_path) {
                        Ok(f) => {
                            let (file_writer, guard) = non_blocking(f);
                            let file_layer = tracing_subscriber::fmt::layer()
                                .with_writer(file_writer)
                                .with_ansi(false)
                                .event_format(LogFormatter);

                            eprintln!(
                                "[mssql-py-core] Created log file → {log_path:?}"
                            );

                            // Store guard to keep async writer alive
                            let _ = GUARD.set(guard);

                            let filter_layer = get_trace_filter();
                            let registry = Registry::default()
                                .with(filter_layer)
                                .with(file_layer);

                            tracing::subscriber::set_global_default(registry)
                                .expect("Setting default subscriber failed");

                            success = true;
                        }
                        Err(e) => {
                            eprintln!(
                                "[mssql-py-core] ERROR: Could not create trace log file {log_path:?}: {e}. Tracing will be disabled."
                            );
                        }
                    }
                }
                None => {
                    eprintln!(
                        "[mssql-py-core] ERROR: Could not determine log directory. Tracing will be disabled."
                    );
                }
            }
        });

        if success {
            eprintln!("[mssql-py-core] Tracing enabled");
        } else {
            eprintln!("[mssql-py-core] ERROR: Tracing is disabled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    // Helper to clean up environment variables after each test
    fn cleanup_env() {
        unsafe {
            env::remove_var(ENV_TRACE);
            env::remove_var(ENV_TRACE_LEVEL);
        }
    }

    #[test]
    fn test_default_trace_level() {
        cleanup_env();
        let filter = get_trace_filter();
        assert_eq!(filter.to_string(), DEFAULT_TRACE_LEVEL);
    }

    #[test]
    fn test_custom_trace_level_debug() {
        cleanup_env();
        unsafe { env::set_var(ENV_TRACE_LEVEL, "debug") };
        let filter = get_trace_filter();
        let filter_str = filter.to_string();
        cleanup_env();
        assert_eq!(filter_str, "debug");
    }

    #[test]
    fn test_custom_trace_level_trace() {
        cleanup_env();
        unsafe { env::set_var(ENV_TRACE_LEVEL, "trace") };
        let filter = get_trace_filter();
        let filter_str = filter.to_string();
        cleanup_env();
        assert_eq!(filter_str, "trace");
    }

    #[test]
    fn test_invalid_trace_level_fallback() {
        cleanup_env();
        unsafe { env::set_var(ENV_TRACE_LEVEL, "invalid_level") };
        let filter = get_trace_filter();
        // Invalid simple level names should fallback to info
        assert_eq!(filter.to_string(), "info");
        cleanup_env();
    }

    #[test]
    fn test_advanced_filter_syntax() {
        cleanup_env();
        unsafe { env::set_var(ENV_TRACE_LEVEL, "mssql_tds=debug,tokio=info") };
        let filter = get_trace_filter();
        // Advanced filter syntax should be rejected and fallback to info
        assert_eq!(filter.to_string(), "info");
        cleanup_env();
    }

    #[test]
    fn test_log_file_path_default_directory() {
        cleanup_env();
        let path = get_trace_log_path();
        assert!(path.is_some());
        let path = path.unwrap();
        assert!(path.to_string_lossy().contains("mssql_python_logs"));
        assert!(path.to_string_lossy().contains(LOG_FILE_PREFIX));
        cleanup_env();
    }

    #[test]
    fn test_log_filename_format() {
        cleanup_env();
        let path = get_trace_log_path();
        assert!(path.is_some());
        let filename = path
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        // Check format: mssql_tds_trace_<timestamp>_<pid>.log
        assert!(filename.starts_with(LOG_FILE_PREFIX));
        assert!(filename.ends_with(".log"));

        // Verify timestamp format (14 digits) and PID
        let parts: Vec<&str> = filename
            .strip_prefix(&format!("{}_", LOG_FILE_PREFIX))
            .unwrap()
            .strip_suffix(".log")
            .unwrap()
            .split('_')
            .collect();

        assert_eq!(parts.len(), 2, "Should have timestamp and PID");
        assert_eq!(
            parts[0].len(),
            14,
            "Timestamp should be YYYYMMDDHHMMSS (14 digits)"
        );
        assert!(parts[1].parse::<u32>().is_ok(), "PID should be a number");

        cleanup_env();
    }

    /// Integration tests that verify end-to-end tracing functionality.
    /// These tests actually initialize tracing and verify log output.
    ///
    /// NOTE: These tests use `call_once()` for initialization, so they must be run
    /// in separate processes. Run them individually with:
    /// ```
    /// cargo test --lib test_tracing_end_to_end -- --ignored --test-threads=1
    /// cargo test --lib test_tracing_disabled -- --ignored --test-threads=1
    /// ```
    #[cfg(test)]
    mod integration_tests {
        use super::*;
        use std::fs;
        use std::path::PathBuf;
        use std::thread;
        use std::time::Duration;

        /// Clean up test directory after test
        fn cleanup_test_dir(dir: &PathBuf) {
            if dir.exists() {
                let _ = fs::remove_dir_all(dir);
            }
        }

        #[test]
        fn test_tracing_end_to_end() {
            cleanup_env();
            // Use default directory: <cwd>/mssql_python_logs/
            let test_dir = std::env::current_dir().unwrap().join("mssql_python_logs");
            cleanup_test_dir(&test_dir);

            unsafe {
                env::set_var(ENV_TRACE, "true");
            }

            // Initialize tracing
            init_tracing();

            // Emit logs at different levels to test filtering
            tracing::error!("Error message");
            tracing::warn!("Warn message");
            tracing::info!("Info message");
            tracing::debug!("Debug message - should not appear");
            tracing::trace!("Trace message - should not appear");

            // Give the async writer time to flush
            thread::sleep(Duration::from_millis(100));

            // Verify log directory and file were created
            assert!(test_dir.exists(), "Log directory should be created");
            let log_files: Vec<_> = fs::read_dir(&test_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();

            assert_eq!(log_files.len(), 1, "Should create exactly one log file");
            let log_file = log_files[0].path();

            // Verify filename format: mssql_tds_trace_<YYYYMMDDHHMMSS>_<PID>.log
            let filename = log_file.file_name().unwrap().to_string_lossy();
            assert!(filename.starts_with("mssql_tds_trace_"));
            assert!(filename.ends_with(".log"));

            let without_prefix = filename.strip_prefix("mssql_tds_trace_").unwrap();
            let without_suffix = without_prefix.strip_suffix(".log").unwrap();
            let parts: Vec<&str> = without_suffix.split('_').collect();
            assert_eq!(parts.len(), 2, "Should have timestamp and PID");
            assert_eq!(parts[0].len(), 14, "Timestamp should be 14 digits");
            assert!(
                parts[1].parse::<u32>().is_ok(),
                "PID should be a valid number"
            );

            // Verify log file content and filtering
            let log_content = fs::read_to_string(&log_file).unwrap();

            // Should contain ERROR, WARN, INFO (at or above info level)
            assert!(log_content.contains("Error message"));
            assert!(log_content.contains("Warn message"));
            assert!(log_content.contains("Info message"));

            // Should NOT contain DEBUG or TRACE (filtered out by the default 'info' level)
            assert!(!log_content.contains("Debug message"));
            assert!(!log_content.contains("Trace message"));

            // Verify CSV format
            let lines: Vec<&str> = log_content.lines().collect();
            assert!(!lines.is_empty(), "Log file should not be empty");

            for line in lines {
                if line.trim().is_empty() {
                    continue;
                }
                // CSV format: timestamp, thread_id, level, target, message
                let parts: Vec<&str> = line.split(',').collect();
                assert!(
                    parts.len() >= 5,
                    "Each log line should have at least 5 CSV fields"
                );
                assert!(
                    parts[0].contains('T'),
                    "First field should be ISO-8601 timestamp"
                );
                assert!(
                    parts[1].trim().parse::<u64>().is_ok(),
                    "Thread ID should be a numeric value"
                );
                assert!(
                    ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"].contains(&parts[2].trim()),
                    "Log level should be valid"
                );
            }

            cleanup_test_dir(&test_dir);
            cleanup_env();
        }
    }
}
