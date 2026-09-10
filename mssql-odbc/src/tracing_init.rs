// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chrono::Local;
use std::ffi::OsString;
use std::fmt;
use std::fs::{OpenOptions, create_dir_all, remove_file};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, Once};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

static INIT_TRACING: Once = Once::new();

const ENV_TRACE: &str = "MSSQL_TDS_TRACE";
const ENV_TRACE_LEVEL: &str = "MSSQL_TDS_TRACE_LEVEL";
const ENV_TRACE_DIR: &str = "MSSQL_TDS_TRACE_DIR";
const DEFAULT_TRACE_LEVEL: &str = "warn";
const LOG_FILE_PREFIX: &str = "mssql_tds_trace";
const MAX_FILENAME_ATTEMPTS: u32 = 100;

struct TraceFileWriter {
    path: PathBuf,
    write_lock: Mutex<()>,
}

enum TraceWriter<'writer> {
    File {
        file: std::fs::File,
        _guard: MutexGuard<'writer, ()>,
    },
    Sink(io::Sink),
}

impl Write for TraceWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::File { file, .. } => file.write(buf),
            Self::Sink(sink) => sink.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File { file, .. } => file.flush(),
            Self::Sink(sink) => sink.flush(),
        }
    }
}

impl<'writer> MakeWriter<'writer> for TraceFileWriter {
    type Writer = TraceWriter<'writer>;

    fn make_writer(&'writer self) -> Self::Writer {
        let guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match OpenOptions::new().append(true).open(&self.path) {
            Ok(file) => TraceWriter::File {
                file,
                _guard: guard,
            },
            Err(error) => {
                drop(guard);
                report(format_args!(
                    "[mssql-odbc] ERROR: could not write trace file {:?}: {error}",
                    self.path
                ));
                TraceWriter::Sink(io::sink())
            }
        }
    }
}

pub(crate) fn init_tracing() {
    if std::panic::catch_unwind(init_tracing_once).is_err() {
        report(format_args!(
            "[mssql-odbc] ERROR: panic while initializing tracing"
        ));
    }
}

fn init_tracing_once() {
    INIT_TRACING.call_once(|| {
        let enabled = std::env::var(ENV_TRACE)
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        if !enabled {
            return;
        }

        let trace_dir = std::env::var_os(ENV_TRACE_DIR);
        let result = match trace_dir {
            Some(dir) => init_file_tracing(dir),
            None => init_stderr_tracing(),
        };

        if let Err(error) = result {
            report(format_args!("[mssql-odbc] ERROR: {error}"));
        }
    });
}

fn trace_filter() -> EnvFilter {
    let level = std::env::var(ENV_TRACE_LEVEL).unwrap_or_else(|_| DEFAULT_TRACE_LEVEL.into());
    EnvFilter::try_new(level.as_str()).unwrap_or_else(|error| {
        report(format_args!(
            "[mssql-odbc] ERROR: Invalid {ENV_TRACE_LEVEL} value '{level}': {error}. Falling back to '{DEFAULT_TRACE_LEVEL}'."
        ));
        EnvFilter::new(DEFAULT_TRACE_LEVEL)
    })
}

fn init_stderr_tracing() -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_env_filter(trace_filter())
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| format!("could not install tracing subscriber: {error}"))
}

fn init_file_tracing(dir: OsString) -> Result<(), String> {
    if dir.is_empty() {
        return Err(format!("{ENV_TRACE_DIR} must not be empty"));
    }

    let dir = prepare_trace_directory(PathBuf::from(dir))?;

    let timestamp = Local::now().format("%Y%m%d%H%M%S%3f").to_string();
    let log_path = reserve_trace_file(&dir, &timestamp, std::process::id())
        .map_err(|error| format!("could not create a trace file in {dir:?}: {error}"))?;

    let init_result = tracing_subscriber::fmt()
        .with_env_filter(trace_filter())
        .with_ansi(false)
        .with_writer(TraceFileWriter {
            path: log_path.clone(),
            write_lock: Mutex::new(()),
        })
        .try_init();
    if let Err(error) = init_result {
        let _ = remove_file(&log_path);
        return Err(format!("could not install tracing subscriber: {error}"));
    }

    report(format_args!("[mssql-odbc] Tracing to {log_path:?}"));
    Ok(())
}

fn prepare_trace_directory(dir: PathBuf) -> Result<PathBuf, String> {
    let absolute_dir = if dir.is_absolute() {
        dir
    } else {
        std::env::current_dir()
            .map_err(|error| format!("could not resolve {ENV_TRACE_DIR}: {error}"))?
            .join(dir)
    };
    create_dir_all(&absolute_dir)
        .map_err(|error| format!("could not create trace directory {absolute_dir:?}: {error}"))?;
    let resolved_dir = absolute_dir
        .canonicalize()
        .map_err(|error| format!("could not resolve trace directory {absolute_dir:?}: {error}"))?;

    if !resolved_dir.is_dir() {
        return Err(format!(
            "{ENV_TRACE_DIR} is not a directory: {resolved_dir:?}"
        ));
    }

    validate_trace_directory(&resolved_dir)?;
    Ok(resolved_dir)
}

fn reserve_trace_file(dir: &Path, timestamp: &str, pid: u32) -> io::Result<PathBuf> {
    for attempt in 0..MAX_FILENAME_ATTEMPTS {
        let log_path = trace_log_path(dir, timestamp, pid, attempt);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        match options.open(&log_path) {
            Ok(_) => return Ok(log_path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique trace filename",
    ))
}

fn trace_log_path(dir: &Path, timestamp: &str, pid: u32, attempt: u32) -> PathBuf {
    let collision_suffix = if attempt == 0 {
        String::new()
    } else {
        format!("_{attempt}")
    };
    dir.join(format!(
        "{LOG_FILE_PREFIX}_{timestamp}_{pid}{collision_suffix}.log"
    ))
}

fn validate_trace_directory(dir: &Path) -> Result<(), String> {
    let temp_dir = std::env::temp_dir();
    let resolved_temp_dir = temp_dir.canonicalize().unwrap_or(temp_dir);
    if dir.starts_with(&resolved_temp_dir) {
        report(format_args!(
            "[mssql-odbc] WARNING: {ENV_TRACE_DIR} points inside the system temporary directory. Trace files may contain SQL text and parameter values."
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = dir
            .metadata()
            .map_err(|error| format!("could not inspect trace directory {dir:?}: {error}"))?;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(format!(
                "{ENV_TRACE_DIR} must not be writable by group or other users: {dir:?}"
            ));
        }
    }

    Ok(())
}

fn report(args: fmt::Arguments<'_>) {
    let _ = writeln!(io::stderr().lock(), "{args}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::remove_dir_all;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT_TEST_DIR: AtomicU32 = AtomicU32::new(0);

    fn test_directory(test_name: &str) -> PathBuf {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mssqlodbc-{test_name}-{}-{sequence}",
            std::process::id()
        ));
        let _ = remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn trace_log_path_contains_timestamp_and_pid() {
        let path = trace_log_path(Path::new("trace-dir"), "20260910123456789", 42, 0);

        assert_eq!(
            path,
            Path::new("trace-dir").join("mssql_tds_trace_20260910123456789_42.log")
        );
    }

    #[test]
    fn current_directory_is_an_explicit_trace_directory() {
        let path = trace_log_path(Path::new("."), "20260910123456789", 42, 0);

        assert_eq!(
            path,
            Path::new(".").join("mssql_tds_trace_20260910123456789_42.log")
        );
    }

    #[test]
    fn relative_trace_directory_is_resolved_to_an_absolute_path() {
        let dir_name = format!(
            "mssqlodbc-relative-{}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        );
        let expected = std::env::current_dir().unwrap().join(&dir_name);

        let resolved = prepare_trace_directory(PathBuf::from(&dir_name)).unwrap();

        assert!(resolved.is_absolute());
        assert_eq!(resolved, expected.canonicalize().unwrap());
        remove_dir_all(expected).unwrap();
    }

    #[test]
    fn filename_collision_adds_attempt_suffix() {
        let path = trace_log_path(Path::new("trace-dir"), "20260910123456789", 42, 3);

        assert_eq!(
            path,
            Path::new("trace-dir").join("mssql_tds_trace_20260910123456789_42_3.log")
        );
    }

    #[test]
    fn reserve_trace_file_retries_a_filename_collision() {
        let dir = test_directory("collision");
        let first_path = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let second_path = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();

        assert_eq!(
            first_path.file_name().unwrap(),
            "mssql_tds_trace_20260910123456789_42.log"
        );
        assert_eq!(
            second_path.file_name().unwrap(),
            "mssql_tds_trace_20260910123456789_42_1.log"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                first_path.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trace_file_writer_appends_and_releases_each_handle() {
        let dir = test_directory("writer");
        let path = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let make_writer = TraceFileWriter {
            path: path.clone(),
            write_lock: Mutex::new(()),
        };

        make_writer.make_writer().write_all(b"first\n").unwrap();
        make_writer.make_writer().write_all(b"second\n").unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "first\nsecond\n");
        remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trace_file_writer_recovers_a_poisoned_lock() {
        let dir = test_directory("poison");
        let path = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let make_writer = TraceFileWriter {
            path: path.clone(),
            write_lock: Mutex::new(()),
        };
        let _ = std::panic::catch_unwind(|| {
            let _guard = make_writer.write_lock.lock().unwrap();
            panic!("poison the trace serialization lock");
        });

        make_writer
            .make_writer()
            .write_all(b"after poison\n")
            .unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "after poison\n");
        remove_dir_all(dir).unwrap();
    }
}
