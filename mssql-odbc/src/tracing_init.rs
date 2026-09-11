// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chrono::Utc;
use std::ffi::OsString;
use std::fmt;
use std::fs::{OpenOptions, create_dir_all, remove_file};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Once, Weak};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::registry::LookupSpan;

static INIT_TRACING: Once = Once::new();
static TRACE_FILE_WRITER: Mutex<Option<Weak<TraceFileWriter>>> = Mutex::new(None);

const ENV_TRACE: &str = "MSSQL_TDS_TRACE";
const ENV_TRACE_LEVEL: &str = "MSSQL_TDS_TRACE_LEVEL";
const ENV_TRACE_DIR: &str = "MSSQL_TDS_TRACE_DIR";
const DEFAULT_TRACE_LEVEL: &str = "warn";
const LOG_FILE_PREFIX: &str = "mssql_tds_trace";
const MAX_FILENAME_ATTEMPTS: u32 = 100;

struct LogFormatter;

impl<S, N> FormatEvent<S, N> for LogFormatter
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let thread_id = format!("{:?}", std::thread::current().id());
        let thread_id = thread_id
            .strip_prefix("ThreadId(")
            .and_then(|value| value.strip_suffix(')'))
            .unwrap_or("unknown");
        let metadata = event.metadata();

        write!(
            writer,
            "{timestamp}, {thread_id}, {}, {}, ",
            metadata.level(),
            metadata.target()
        )?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

struct TraceFileWriter {
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
    #[cfg(test)]
    open_count: std::sync::atomic::AtomicU32,
}

#[derive(Clone)]
struct SharedTraceFileWriter(Arc<TraceFileWriter>);

impl TraceFileWriter {
    fn new(path: PathBuf, file: std::fs::File) -> Self {
        Self {
            path,
            file: Mutex::new(Some(file)),
            #[cfg(test)]
            open_count: std::sync::atomic::AtomicU32::new(1),
        }
    }

    fn file(&self) -> MutexGuard<'_, Option<std::fs::File>> {
        self.file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn close(&self) {
        self.file().take();
    }
}

enum TraceWriter<'writer> {
    Cached {
        file: MutexGuard<'writer, Option<std::fs::File>>,
    },
    Transient {
        file: std::fs::File,
        _guard: MutexGuard<'writer, Option<std::fs::File>>,
    },
    Sink(io::Sink),
}

impl Write for TraceWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Cached { file } => file
                .as_mut()
                .ok_or_else(|| io::Error::other("trace file is closed"))?
                .write(buf),
            Self::Transient { file, .. } => file.write(buf),
            Self::Sink(sink) => sink.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Cached { file } => file
                .as_mut()
                .ok_or_else(|| io::Error::other("trace file is closed"))?
                .flush(),
            Self::Transient { file, .. } => file.flush(),
            Self::Sink(sink) => sink.flush(),
        }
    }
}

impl<'writer> MakeWriter<'writer> for SharedTraceFileWriter {
    type Writer = TraceWriter<'writer>;

    fn make_writer(&'writer self) -> Self::Writer {
        if crate::handles::process_is_shutting_down() {
            return TraceWriter::Sink(io::sink());
        }

        let file = self.0.file();
        let has_live_env = crate::handles::live_env_count() != 0;
        self.make_writer_for_env_state(file, has_live_env)
    }
}

impl SharedTraceFileWriter {
    fn make_writer_for_env_state<'writer>(
        &'writer self,
        mut cached_file: MutexGuard<'writer, Option<std::fs::File>>,
        has_live_env: bool,
    ) -> TraceWriter<'writer> {
        if !has_live_env {
            return match OpenOptions::new().append(true).open(&self.0.path) {
                Ok(file) => TraceWriter::Transient {
                    file,
                    _guard: cached_file,
                },
                Err(error) => {
                    report(format_args!(
                        "[mssql-odbc] ERROR: could not reopen trace file {:?}: {error}",
                        self.0.path
                    ));
                    TraceWriter::Sink(io::sink())
                }
            };
        }

        if cached_file.is_none() {
            match OpenOptions::new().append(true).open(&self.0.path) {
                Ok(reopened) => {
                    *cached_file = Some(reopened);
                    #[cfg(test)]
                    self.0
                        .open_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(error) => {
                    drop(cached_file);
                    report(format_args!(
                        "[mssql-odbc] ERROR: could not reopen trace file {:?}: {error}",
                        self.0.path
                    ));
                    return TraceWriter::Sink(io::sink());
                }
            }
        }
        TraceWriter::Cached { file: cached_file }
    }
}

pub(crate) fn close_trace_file() {
    if crate::handles::process_is_shutting_down() {
        return;
    }

    let writer = TRACE_FILE_WRITER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(Weak::upgrade);
    if let Some(writer) = writer {
        writer.close();
    }
}

pub(crate) fn init_tracing() {
    if crate::handles::process_is_shutting_down() {
        return;
    }

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
        .event_format(LogFormatter)
        .try_init()
        .map_err(|error| format!("could not install tracing subscriber: {error}"))
}

fn init_file_tracing(dir: OsString) -> Result<(), String> {
    if dir.is_empty() {
        return Err(format!("{ENV_TRACE_DIR} must not be empty"));
    }

    let dir = prepare_trace_directory(PathBuf::from(dir))?;

    let timestamp = Utc::now().format("%Y%m%d%H%M%S%3f").to_string();
    let (log_path, file) = reserve_trace_file(&dir, &timestamp, std::process::id())
        .map_err(|error| format!("could not create a trace file in {dir:?}: {error}"))?;
    let writer = Arc::new(TraceFileWriter::new(log_path.clone(), file));

    let init_result = tracing_subscriber::fmt()
        .with_env_filter(trace_filter())
        .with_ansi(false)
        .with_writer(SharedTraceFileWriter(Arc::clone(&writer)))
        .event_format(LogFormatter)
        .try_init();
    if let Err(error) = init_result {
        let _ = remove_file(&log_path);
        return Err(format!("could not install tracing subscriber: {error}"));
    }
    *TRACE_FILE_WRITER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::downgrade(&writer));
    writer.close();

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

fn reserve_trace_file(
    dir: &Path,
    timestamp: &str,
    pid: u32,
) -> io::Result<(PathBuf, std::fs::File)> {
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
            Ok(file) => return Ok((log_path, file)),
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
        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 {
            report(format_args!(
                "[mssql-odbc] WARNING: {ENV_TRACE_DIR} is writable by group or other users. Ensure every user with write access is trusted: {dir:?}"
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tracing_subscriber::layer::SubscriberExt;

    static NEXT_TEST_DIR: AtomicU32 = AtomicU32::new(0);

    #[derive(Clone)]
    struct TestWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

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
        let expected = std::env::current_dir().unwrap().canonicalize().unwrap();

        let resolved = prepare_trace_directory(PathBuf::from(".")).unwrap();

        assert_eq!(resolved, expected);
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
        let (first_path, first_file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let (second_path, second_file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();

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

        drop((first_file, second_file));
        remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trace_file_writer_reuses_the_handle_until_closed() {
        let dir = test_directory("writer");
        let (path, file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let writer = Arc::new(TraceFileWriter::new(path.clone(), file));
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));

        make_writer
            .make_writer_for_env_state(writer.file(), true)
            .write_all(b"first\n")
            .unwrap();
        make_writer
            .make_writer_for_env_state(writer.file(), true)
            .write_all(b"second\n")
            .unwrap();

        assert_eq!(writer.open_count.load(Ordering::Relaxed), 1);
        writer.close();

        let moved_path = path.with_extension("moved");
        std::fs::rename(&path, &moved_path).unwrap();
        std::fs::rename(&moved_path, &path).unwrap();

        make_writer
            .make_writer_for_env_state(writer.file(), true)
            .write_all(b"third\n")
            .unwrap();
        assert_eq!(writer.open_count.load(Ordering::Relaxed), 2);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "first\nsecond\nthird\n"
        );

        writer.close();
        remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trace_file_writer_does_not_cache_without_an_environment() {
        let dir = test_directory("transient-writer");
        let (path, file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let writer = Arc::new(TraceFileWriter::new(path.clone(), file));
        writer.close();
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));

        make_writer
            .make_writer_for_env_state(writer.file(), false)
            .write_all(b"first\n")
            .unwrap();
        assert!(writer.file().is_none());

        remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trace_file_writer_recovers_a_poisoned_lock() {
        let dir = test_directory("poison");
        let (path, file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let writer = Arc::new(TraceFileWriter::new(path.clone(), file));
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));
        let _ = std::panic::catch_unwind(|| {
            let _guard = writer.file.lock().unwrap();
            panic!("poison the trace serialization lock");
        });

        make_writer
            .make_writer_for_env_state(writer.file(), true)
            .write_all(b"after poison\n")
            .unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "after poison\n");
        writer.close();
        remove_dir_all(dir).unwrap();
    }

    #[test]
    fn event_writer_serializes_close_until_it_is_dropped() {
        let dir = test_directory("close-serialization");
        let (path, file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let writer = Arc::new(TraceFileWriter::new(path, file));
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));
        let event_writer = make_writer.make_writer_for_env_state(writer.file(), true);

        assert!(matches!(
            writer.file.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        drop(event_writer);
        writer.close();
        assert!(writer.file().is_none());
        remove_dir_all(dir).unwrap();
    }

    #[test]
    fn formatter_emits_stable_fields_without_span_context() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let test_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || TestWriter(Arc::clone(&test_output)))
                .event_format(LogFormatter),
        );

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                target: "mssql_tds::connection::tds_client",
                "execute",
                sql_command = "SELECT 'SECRET_SQL_LITERAL'"
            );
            let _entered = span.enter();
            tracing::info!(target: "mssqlodbc::test", operation_id = 42_u64, "completed");
        });

        let output = output.lock().unwrap();
        let output = std::str::from_utf8(&output).unwrap();
        let fields: Vec<_> = output.splitn(5, ',').collect();
        assert_eq!(fields.len(), 5);
        assert!(fields[0].contains('T'));
        assert!(fields[0].ends_with('Z'));
        assert!(fields[1].trim().parse::<u64>().is_ok());
        assert_eq!(fields[2].trim(), "INFO");
        assert_eq!(fields[3].trim(), "mssqlodbc::test");
        assert!(fields[4].contains("completed"));
        assert!(fields[4].contains("operation_id=42"));
        assert!(!output.contains("sql_command"));
        assert!(!output.contains("SECRET_SQL_LITERAL"));
    }

    #[cfg(unix)]
    #[test]
    fn group_and_world_writable_trace_directories_are_allowed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_directory("writable");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(validate_trace_directory(&dir).is_ok());

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(validate_trace_directory(&dir).is_ok());

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(validate_trace_directory(&dir).is_ok());
        remove_dir_all(dir).unwrap();
    }
}
