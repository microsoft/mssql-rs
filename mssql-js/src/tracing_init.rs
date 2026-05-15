// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use once_cell::sync::OnceCell;
use std::fs::{OpenOptions, create_dir_all};
use std::path::{Path, PathBuf};
use std::sync::Once;
use tracing_appender::non_blocking;
use tracing_subscriber::{EnvFilter, Registry, layer::SubscriberExt};

static INIT: Once = Once::new();
static GUARD: OnceCell<tracing_appender::non_blocking::WorkerGuard> = OnceCell::new();
// === Environment Variable and Constant Strings ===
const ENV_TRACE: &str = "MSSQLJS_TRACE";
const ENV_TRACE_OUTPUTS: &str = "MSSQLJS_TRACE_OUTPUTS";
const ENV_TRACE_DIR: &str = "MSSQLJS_TRACE_DIR";
const ENV_TRACE_LEVEL: &str = "MSSQLJS_TRACE_LEVEL";
const DEFAULT_TRACE_OUTPUTS: &str = "console";
const DEFAULT_TRACE_LEVEL: &str = "info";
const TRACE_LOG_FILENAME: &str = "mssqljs_trace.log";

fn is_insecure_path(path: &Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();

    // Unix temporary directories
    if path_str.starts_with("/tmp") || path_str.starts_with("/var/tmp") {
        return true;
    }

    // Windows temporary directories
    if path_str.contains("\\temp\\") || path_str.contains("\\tmp\\") {
        return true;
    }

    // Check if it's exactly the system temp directory
    let system_temp = std::env::temp_dir().to_string_lossy().to_lowercase();
    if path_str.starts_with(&system_temp) {
        return true;
    }

    false
}

fn get_trace_log_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(ENV_TRACE_DIR) {
        let path = PathBuf::from(dir);

        // Security check: warn about insecure paths but allow them
        if is_insecure_path(&path) {
            eprintln!("[mssql-js] WARNING: Insecure log directory detected: {path:?}",);
            eprintln!("[mssql-js] WARNING: Logs may contain sensitive data (queries, data etc).");
            eprintln!(
                "[mssql-js] WARNING: Logging to /tmp, /var/tmp, or system temp directories is not recommended."
            );
            eprintln!(
                "[mssql-js] WARNING: These directories may be world-readable and inappropriate for sensitive data."
            );
            eprintln!(
                "[mssql-js] WARNING: Consider using a secure, application-controlled directory with proper permissions."
            );
            eprintln!("[mssql-js] WARNING: Example: /var/log/myapp or /app/logs");
            eprintln!("[mssql-js] WARNING: Proceeding with logging to the specified directory...");
        }

        if !path.exists()
            && let Err(e) = create_dir_all(&path)
        {
            eprintln!("[mssql-js] ERROR: Could not create log directory '{path:?}': {e}",);
            return None;
        }
        return Some(path.join(TRACE_LOG_FILENAME));
    }

    // No MSSQLJS_TRACE_DIR set - this is now required for file logging
    None
}

fn get_trace_filter() -> EnvFilter {
    let filter = std::env::var(ENV_TRACE_LEVEL).unwrap_or_else(|_| DEFAULT_TRACE_LEVEL.to_string());
    match EnvFilter::try_new(&filter) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "[mssql-js] WARNING: Invalid MSSQLJS_TRACE_LEVEL value '{filter}': {e}. Falling back to 'info'."
            );
            EnvFilter::new("info")
        }
    }
}

fn get_trace_outputs() -> (bool, bool) {
    // Returns (file_enabled, console_enabled)
    let outputs =
        std::env::var(ENV_TRACE_OUTPUTS).unwrap_or_else(|_| DEFAULT_TRACE_OUTPUTS.to_string());
    let mut file = false;
    let mut console = false;
    for output in outputs.split(',').map(|s| s.trim().to_lowercase()) {
        match output.as_str() {
            "file" => file = true,
            "console" => console = true,
            "" => {} // ignore empty
            other => {
                eprintln!(
                    "[mssql-js] WARNING: Unknown trace output '{other}'. Supported: file, console."
                );
            }
        }
    }
    (file, console)
}

pub fn init_tracing() {
    let enable_trace = std::env::var(ENV_TRACE)
        .unwrap_or_else(|_| "false".to_string())
        .parse::<bool>()
        .unwrap_or(false);

    if enable_trace {
        INIT.call_once(|| {
            let (file_enabled, console_enabled) = get_trace_outputs();
            if !file_enabled && !console_enabled {
                eprintln!("[mssql-js] WARNING: No valid trace outputs enabled. Set {ENV_TRACE_OUTPUTS} to 'file', 'console', or both.");
                return;
            }

            let mut guard = None;
            let mut file_layer = None;
            if file_enabled {
                match get_trace_log_path() {
                    Some(log_path) => {
                        match OpenOptions::new().create(true).append(true).open(&log_path) {
                            Ok(f) => {
                                let (file_writer, g) = non_blocking(f);
                                file_layer = Some(tracing_subscriber::fmt::layer()
                                    .with_writer(file_writer)
                                    .with_ansi(false));
                                guard = Some(g);
                            },
                            Err(e) => {
                                eprintln!(
                                    "[mssql-js] ERROR: Could not create trace log file {log_path:?}: {e}. File logging will be disabled."
                                );
                            }
                        }
                    }
                    None => {
                        eprintln!("[mssql-js] ERROR: File logging is enabled but {ENV_TRACE_DIR} is not set.");
                        eprintln!("[mssql-js] ERROR: For security reasons, you must explicitly specify a secure log directory.");
                        eprintln!("[mssql-js] ERROR: Set {ENV_TRACE_DIR} to a secure, application-controlled directory.");
                        eprintln!("[mssql-js] ERROR: Example: export {ENV_TRACE_DIR}=/var/log/myapp");
                        eprintln!("[mssql-js] ERROR: File logging will be disabled. Console logging may still be active.");
                    }
                }
            }
            if let Some(g) = guard {
                let _ = GUARD.set(g);
            }

            let filter_layer = get_trace_filter();
            let registry = Registry::default().with(filter_layer);
            if let Some(file_layer) = file_layer {
                if console_enabled {
                    let registry_with_file_and_console = registry
                        .with(file_layer)
                        .with(tracing_subscriber::fmt::layer()
                            .with_writer(std::io::stdout)
                            .with_ansi(true));
                    tracing::subscriber::set_global_default(registry_with_file_and_console)
                    .expect("Setting default subscriber failed");
                } else {
                    let registry_with_file = registry.with(file_layer);
                    tracing::subscriber::set_global_default(registry_with_file)
                    .expect("Setting default subscriber failed");
                }
            } else if console_enabled {
                let registry_with_console = registry.with(tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stdout)
                        .with_ansi(true));
                tracing::subscriber::set_global_default(registry_with_console)
                .expect("Setting default subscriber failed");
            } else {
                let empty_registry = registry.with(tracing_subscriber::fmt::layer());
                tracing::subscriber::set_global_default(empty_registry)
                .expect("Setting default subscriber failed");
            };
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_get_trace_filter_default() {
        unsafe {
            env::remove_var(ENV_TRACE_LEVEL);
        }
        let filter = get_trace_filter();
        assert_eq!(filter.to_string(), DEFAULT_TRACE_LEVEL);
    }

    #[test]
    fn test_get_trace_filter_valid() {
        unsafe {
            env::remove_var(ENV_TRACE_LEVEL);
            env::set_var(ENV_TRACE_LEVEL, "debug");
        }
        let filter = get_trace_filter();
        assert_eq!(filter.to_string(), "debug");
        unsafe { env::remove_var(ENV_TRACE_LEVEL) };
    }

    #[test]
    fn test_get_trace_filter_invalid() {
        unsafe { env::set_var(ENV_TRACE_LEVEL, "banana") };
        let filter = get_trace_filter();
        assert_eq!(filter.to_string(), "banana=trace");
        unsafe { env::remove_var(ENV_TRACE_LEVEL) };
    }

    #[test]
    fn test_insecure_path_unix_tmp() {
        let path = PathBuf::from("/tmp/logs");
        assert!(is_insecure_path(&path));
    }

    #[test]
    fn test_insecure_path_var_tmp() {
        let path = PathBuf::from("/var/tmp/logs");
        assert!(is_insecure_path(&path));
    }

    #[test]
    fn test_insecure_path_windows_temp() {
        let path = PathBuf::from("C:\\Windows\\Temp\\logs");
        assert!(is_insecure_path(&path));
    }

    #[test]
    fn test_secure_path_var_log() {
        let path = PathBuf::from("/var/log/myapp");
        assert!(!is_insecure_path(&path));
    }

    #[test]
    fn test_secure_path_app_logs() {
        let path = PathBuf::from("/app/logs");
        assert!(!is_insecure_path(&path));
    }

    #[test]
    fn test_get_trace_log_path_without_env() {
        unsafe { env::remove_var(ENV_TRACE_DIR) };
        let result = get_trace_log_path();
        assert!(result.is_none());
    }

    #[test]
    fn test_get_trace_log_path_with_insecure_path() {
        unsafe { env::set_var(ENV_TRACE_DIR, "/tmp") };
        let result = get_trace_log_path();
        // Should still return a path but with warnings printed
        assert!(result.is_some());
        assert!(result.unwrap().to_string_lossy().contains("/tmp"));
        unsafe { env::remove_var(ENV_TRACE_DIR) };
    }

    #[test]
    fn test_default_trace_outputs_is_console() {
        assert_eq!(DEFAULT_TRACE_OUTPUTS, "console");
    }

    #[test]
    fn test_get_trace_outputs_default() {
        unsafe { env::remove_var(ENV_TRACE_OUTPUTS) };
        let (file, console) = get_trace_outputs();
        assert!(!file);
        assert!(console);
    }

    #[test]
    fn test_get_trace_outputs_file_requires_explicit() {
        unsafe { env::set_var(ENV_TRACE_OUTPUTS, "file") };
        let (file, console) = get_trace_outputs();
        assert!(file);
        assert!(!console);
        unsafe { env::remove_var(ENV_TRACE_OUTPUTS) };
    }
}
