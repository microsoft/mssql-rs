// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! File and stderr trace sinks for the ODBC driver.
//!
//! Rotation is bounded, but **nothing is ever deleted**: there is no retention
//! policy, no file cap and no age-based cleanup. A cleanup pass scanning the
//! trace directory cannot tell its own files from those of another process
//! writing to the same directory, so it risks destroying diagnostics belonging
//! to a concurrently running or longer-lived instance — exactly when they
//! matter most. Rotation therefore bounds only the size of an individual file,
//! so each one stays openable, searchable and attachable to a bug report.
//! Total output is bounded by the operator, who owns reclaiming the space; see
//! the tracing section of `mssql-odbc/README.md` (AB#48091).

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
const ENV_TRACE_MAX_FILE_SIZE_MB: &str = "MSSQL_TDS_TRACE_MAX_FILE_SIZE_MB";
const DEFAULT_TRACE_LEVEL: &str = "warn";
const LOG_FILE_PREFIX: &str = "mssql_tds_trace";
const MAX_FILENAME_ATTEMPTS: u32 = 100;
const DEFAULT_MAX_FILE_SIZE_MB: u64 = 100;
const MAX_FILE_SIZE_MB: u64 = 1024;

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
        let correlation = mssql_tds::trace_context::snapshot();
        if let Some(dbc) = correlation.dbc {
            write!(writer, "dbc={dbc} ")?;
        }
        if let Some(cid) = correlation.cid {
            let mut buffer = uuid::Uuid::encode_buffer();
            write!(
                writer,
                "cid={} ",
                cid.hyphenated().encode_upper(&mut buffer)
            )?;
        } else {
            write!(writer, "cid=- ")?;
        }
        if let Some(stmt) = correlation.stmt {
            write!(writer, "stmt={stmt} ")?;
        }
        if let Some(exec) = correlation.exec {
            write!(writer, "exec={exec} ")?;
        }
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

struct TraceFileWriter {
    state: Mutex<TraceFileState>,
    max_file_size: u64,
    #[cfg(test)]
    open_count: std::sync::atomic::AtomicU32,
}

struct TraceFileState {
    base_path: PathBuf,
    path: PathBuf,
    file: Option<std::fs::File>,
    bytes_written: u64,
    next_rotation: u32,
}

impl TraceFileState {
    /// Adds `written` to the rollover counter, saturating.
    ///
    /// `usize` is never wider than `u64` on a supported target, so the
    /// `u64::MAX` arm is unreachable today. Taking the checked conversion
    /// anyway keeps the counter from under-counting — and so from skipping a
    /// rollover — rather than silently truncating if that ever stops holding.
    fn record_written(&mut self, written: usize) {
        self.bytes_written = self
            .bytes_written
            .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
    }
}

#[derive(Clone)]
struct SharedTraceFileWriter(Arc<TraceFileWriter>);

impl TraceFileWriter {
    fn new(path: PathBuf, file: std::fs::File, max_file_size: u64) -> Self {
        Self {
            state: Mutex::new(TraceFileState {
                base_path: path.clone(),
                path,
                file: Some(file),
                bytes_written: 0,
                next_rotation: 1,
            }),
            max_file_size,
            #[cfg(test)]
            open_count: std::sync::atomic::AtomicU32::new(1),
        }
    }

    fn state(&self) -> MutexGuard<'_, TraceFileState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn close(&self) {
        self.state().file.take();
    }

    fn rotate_if_needed(&self, state: &mut TraceFileState) {
        if state.bytes_written < self.max_file_size {
            return;
        }

        match reserve_rotated_trace_file(&state.base_path, &mut state.next_rotation) {
            Ok((path, file)) => {
                state.file = Some(file);
                state.path = path;
                state.bytes_written = 0;
            }
            Err(error) => {
                report(format_args!(
                    "[mssql-odbc] ERROR: could not rotate trace file {:?}: {error}. Continuing in the current file.",
                    state.path
                ));
                // Retry at most once per `max_file_size` bytes. Leaving the counter
                // at or above the threshold would make every later event re-attempt
                // rotation, turning a persistent failure (full or read-only volume)
                // into a burst of failed file-creation syscalls per trace write.
                state.bytes_written = 0;
            }
        }
    }
}

enum TraceWriter<'writer> {
    Cached {
        state: MutexGuard<'writer, TraceFileState>,
    },
    Transient {
        file: std::fs::File,
        state: MutexGuard<'writer, TraceFileState>,
    },
    Sink(io::Sink),
}

impl Write for TraceWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Cached { state } => {
                let written = state
                    .file
                    .as_mut()
                    .ok_or_else(|| io::Error::other("trace file is closed"))?
                    .write(buf)?;
                state.record_written(written);
                Ok(written)
            }
            Self::Transient { file, state } => {
                let written = file.write(buf)?;
                state.record_written(written);
                Ok(written)
            }
            Self::Sink(sink) => sink.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Cached { state } => state
                .file
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

        let state = self.0.state();
        let has_live_env = crate::handles::live_env_count() != 0;
        self.make_writer_for_env_state(state, has_live_env)
    }
}

impl SharedTraceFileWriter {
    fn make_writer_for_env_state<'writer>(
        &'writer self,
        mut state: MutexGuard<'writer, TraceFileState>,
        has_live_env: bool,
    ) -> TraceWriter<'writer> {
        self.0.rotate_if_needed(&mut state);

        if !has_live_env {
            state.file.take();
            return match OpenOptions::new().append(true).open(&state.path) {
                Ok(file) => TraceWriter::Transient { file, state },
                Err(error) => {
                    report(format_args!(
                        "[mssql-odbc] ERROR: could not reopen trace file {:?}: {error}",
                        state.path
                    ));
                    TraceWriter::Sink(io::sink())
                }
            };
        }

        if state.file.is_none() {
            match OpenOptions::new().append(true).open(&state.path) {
                Ok(reopened) => {
                    state.file = Some(reopened);
                    #[cfg(test)]
                    self.0
                        .open_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(error) => {
                    report(format_args!(
                        "[mssql-odbc] ERROR: could not reopen trace file {:?}: {error}",
                        state.path
                    ));
                    return TraceWriter::Sink(io::sink());
                }
            }
        }
        TraceWriter::Cached { state }
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
        } else {
            mssql_tds::trace_context::enable();
        }
    });
}

pub(crate) fn starts_execution(api: &str) -> bool {
    matches!(
        api,
        "SQLExecute"
            | "SQLExecDirectW"
            | "SQLDescribeParam"
            | "SQLTablesW"
            | "SQLColumnsW"
            | "SQLPrimaryKeysW"
            | "SQLForeignKeysW"
            | "SQLStatisticsW"
            | "SQLSpecialColumnsW"
            | "SQLProceduresW"
            | "SQLGetTypeInfoW"
    )
}

/// The registry check permits stale/null handles on diagnostics and free paths
/// without dereferencing them. Live handles remain subject to the API's
/// existing lifetime contract. No handle lock is acquired for correlation.
///
/// # Safety
/// A live `handle` and its parent must remain allocated through this call.
pub(crate) unsafe fn context_for(
    handle: crate::api::odbc_types::SqlHandle,
    api: &str,
) -> mssql_tds::trace_context::Context {
    use crate::handles::{DbcHandle, DescHandle, HandleType, StmtHandle, handle_from_raw};
    use mssql_tds::trace_context::Context;

    match crate::handles::live_type(handle) {
        Some(HandleType::Dbc) => {
            let dbc = unsafe { handle_from_raw::<DbcHandle>(handle) };
            let context = Context::connection(Arc::clone(&dbc.trace));
            if matches!(api, "SQLConnectW" | "SQLDriverConnectW") {
                context.pending_connection()
            } else {
                context
            }
        }
        Some(HandleType::Stmt) => {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(handle) };
            let context = Context::connection(Arc::clone(&stmt.parent_dbc().trace));
            match &stmt.trace {
                Some(trace) => {
                    let context = context.statement(Arc::clone(trace), starts_execution(api));
                    if matches!(api, "SQLGetDiagRecW" | "SQLGetDiagFieldW") {
                        // A rejected concurrent execute can replace diagnostics
                        // without replacing the active result's execution ID.
                        context.without_execution()
                    } else {
                        context
                    }
                }
                None => context,
            }
        }
        Some(HandleType::Desc) => {
            let desc = unsafe { handle_from_raw::<DescHandle>(handle) };
            let dbc = unsafe { handle_from_raw::<DbcHandle>(desc.parent_dbc) };
            Context::connection(Arc::clone(&dbc.trace))
        }
        _ => Context::default(),
    }
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
    let max_file_size_mb = bounded_env_u64(
        ENV_TRACE_MAX_FILE_SIZE_MB,
        DEFAULT_MAX_FILE_SIZE_MB,
        1,
        MAX_FILE_SIZE_MB,
    );

    let timestamp = Utc::now().format("%Y%m%d%H%M%S%3f").to_string();
    let (log_path, file) = reserve_trace_file(&dir, &timestamp, std::process::id())
        .map_err(|error| format!("could not create a trace file in {dir:?}: {error}"))?;
    let writer = Arc::new(TraceFileWriter::new(
        log_path.clone(),
        file,
        max_file_size_mb * 1024 * 1024,
    ));

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

/// Validates an operator-supplied bound, falling back to `default` for
/// anything unparseable or outside `min..=max`.
///
/// Split out of [`bounded_env_u64`] so the validation can be tested without
/// mutating the process environment: `set_var` is unsound while any other
/// thread may touch the environment, and the test harness runs tests
/// concurrently with others in this crate that read environment variables.
fn bounded_value(name: &str, value: &str, default: u64, min: u64, max: u64) -> u64 {
    match value.parse::<u64>() {
        Ok(parsed) if (min..=max).contains(&parsed) => parsed,
        _ => {
            report(format_args!(
                "[mssql-odbc] ERROR: Invalid {name} value '{value}'; expected an integer from {min} through {max}. Falling back to {default}."
            ));
            default
        }
    }
}

fn bounded_env_u64(name: &str, default: u64, min: u64, max: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => bounded_value(name, &value, default, min, max),
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => {
            report(format_args!(
                "[mssql-odbc] ERROR: Could not read {name}: {error}. Falling back to {default}."
            ));
            default
        }
    }
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

fn reserve_rotated_trace_file(
    current_path: &Path,
    next_rotation: &mut u32,
) -> io::Result<(PathBuf, std::fs::File)> {
    let directory = current_path
        .parent()
        .ok_or_else(|| io::Error::other("trace path has no parent directory"))?;
    let stem = current_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| io::Error::other("trace filename is not valid UTF-8"))?;

    for _ in 0..MAX_FILENAME_ATTEMPTS {
        let path = directory.join(format!("{stem}.{}.log", *next_rotation));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        match options.open(&path) {
            Ok(file) => {
                // The suffix is now taken by a real file.
                *next_rotation = next_rotation.saturating_add(1);
                return Ok((path, file));
            }
            // Something else already holds this suffix, so skip it for good.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                *next_rotation = next_rotation.saturating_add(1);
                continue;
            }
            // Nothing was created, so the suffix must stay available. Burning
            // it on a transient failure would leave a gap in the sequence once
            // the volume recovers, and a missing number reads as a deleted
            // file — which this driver never does.
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique rotated trace filename",
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
        let writer = Arc::new(TraceFileWriter::new(path.clone(), file, u64::MAX));
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));

        make_writer
            .make_writer_for_env_state(writer.state(), true)
            .write_all(b"first\n")
            .unwrap();
        make_writer
            .make_writer_for_env_state(writer.state(), true)
            .write_all(b"second\n")
            .unwrap();

        assert_eq!(writer.open_count.load(Ordering::Relaxed), 1);
        writer.close();

        let moved_path = path.with_extension("moved");
        std::fs::rename(&path, &moved_path).unwrap();
        std::fs::rename(&moved_path, &path).unwrap();

        make_writer
            .make_writer_for_env_state(writer.state(), true)
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
        let writer = Arc::new(TraceFileWriter::new(path.clone(), file, u64::MAX));
        writer.close();
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));

        make_writer
            .make_writer_for_env_state(writer.state(), false)
            .write_all(b"first\n")
            .unwrap();
        assert!(writer.state().file.is_none());

        remove_dir_all(dir).unwrap();
    }

    /// The no-ENV path writes through a handle reopened per event rather than
    /// the cached one, and it has to keep the same rollover accounting. The
    /// test above pins only that nothing is cached, with a `u64::MAX`
    /// threshold, so dropping `record_written` from `TraceWriter::Transient`
    /// leaves it green — and a process with no live environment would then
    /// write one unbounded file, which is the failure AB#48091 exists to
    /// remove.
    #[test]
    fn rotation_follows_the_writer_without_an_environment_too() {
        let dir = test_directory("transient-rotation");
        let (path, file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let writer = Arc::new(TraceFileWriter::new(path.clone(), file, 5));
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));
        writer.close();

        let write = |payload: &[u8]| {
            make_writer
                .make_writer_for_env_state(writer.state(), false)
                .write_all(payload)
                .unwrap();
        };
        let rotated = path.with_file_name(format!(
            "{}.1.log",
            path.file_stem().unwrap().to_string_lossy()
        ));

        write(b"first");
        assert_eq!(
            writer.state().bytes_written,
            5,
            "the no-ENV path must advance the rollover counter like the cached one"
        );

        write(b"second"); // crosses the 5-byte limit, so this event rolls over
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&rotated).unwrap(), "second");

        // A failed reservation must leave this path writing to the current
        // file and must not consume the number, exactly as the cached path does.
        writer.state().base_path = dir.join("absent").join("mssql_tds_trace.log");
        write(b"third");
        assert_eq!(
            writer.state().next_rotation,
            2,
            "a transient failure must not burn a rollover number"
        );
        assert_eq!(std::fs::read_to_string(&rotated).unwrap(), "secondthird");

        remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trace_file_writer_recovers_a_poisoned_lock() {
        let dir = test_directory("poison");
        let (path, file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let writer = Arc::new(TraceFileWriter::new(path.clone(), file, u64::MAX));
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));
        let _ = std::panic::catch_unwind(|| {
            let _guard = writer.state.lock().unwrap();
            panic!("poison the trace serialization lock");
        });

        make_writer
            .make_writer_for_env_state(writer.state(), true)
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
        let writer = Arc::new(TraceFileWriter::new(path, file, u64::MAX));
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));
        let event_writer = make_writer.make_writer_for_env_state(writer.state(), true);

        assert!(matches!(
            writer.state.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        drop(event_writer);
        writer.close();
        assert!(writer.state().file.is_none());
        remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trace_file_writer_rotates_between_events_and_keeps_every_file() {
        let dir = test_directory("rotation");
        let (path, file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let writer = Arc::new(TraceFileWriter::new(path.clone(), file, 5));
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));
        let rotation = |index: u32| {
            path.with_file_name(format!(
                "{}.{index}.log",
                path.file_stem().unwrap().to_string_lossy()
            ))
        };

        // The 5-byte threshold is reached by each write, so the next event rolls
        // over. An event is never split across two files.
        for payload in [
            b"first".as_slice(),
            b"second".as_slice(),
            b"third".as_slice(),
        ] {
            make_writer
                .make_writer_for_env_state(writer.state(), true)
                .write_all(payload)
                .unwrap();
        }

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(rotation(1)).unwrap(), "second");
        assert_eq!(std::fs::read_to_string(rotation(2)).unwrap(), "third");

        writer.close();
        remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotation_never_removes_an_earlier_trace_file() {
        let dir = test_directory("no-deletion");
        let (path, file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let writer = Arc::new(TraceFileWriter::new(path, file, 1));
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));

        // A one-byte threshold rolls over on every event after the first.
        for _ in 0..12 {
            make_writer
                .make_writer_for_env_state(writer.state(), true)
                .write_all(b"x")
                .unwrap();
        }
        writer.close();

        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            12,
            "the driver must retain every rotated trace file"
        );
        remove_dir_all(dir).unwrap();
    }

    /// The rollover-failure arm: a full or read-only volume must not stop the
    /// driver writing, and must not re-attempt rotation on every later event.
    #[test]
    fn a_failed_rollover_keeps_writing_and_retries_once_per_limit() {
        let dir = test_directory("rotation-failure");
        let (path, file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let writer = Arc::new(TraceFileWriter::new(path.clone(), file, 8));
        let make_writer = SharedTraceFileWriter(Arc::clone(&writer));

        // Point rollover at a directory that does not exist, so reserving the
        // next file fails the way an unwritable volume would.
        writer.state().base_path = dir.join("absent").join("mssql_tds_trace.log");

        let write = |payload: &[u8]| {
            make_writer
                .make_writer_for_env_state(writer.state(), true)
                .write_all(payload)
                .unwrap();
        };

        write(b"aaaaaaaa"); // reaches the 8-byte limit
        assert_eq!(writer.state().bytes_written, 8);

        write(b"bb"); // crosses it: one failed rollover attempt, then 2 bytes
        assert_eq!(
            writer.state().bytes_written,
            2,
            "a failed rollover must still reset the counter"
        );

        write(b"cc"); // still under the limit: must not retry
        assert_eq!(
            writer.state().bytes_written,
            4,
            "rollover must be retried at most once per max_file_size, not per event"
        );

        // Reaching the limit again must produce a *second* attempt: a
        // regression that gave up permanently after the first failure would
        // otherwise pass everything above.
        write(b"dddd"); // 4 + 4 = 8; the check runs before the write, so no attempt yet
        assert_eq!(writer.state().bytes_written, 8);

        write(b"e"); // crosses again: second attempt, fails, counter resets
        assert_eq!(
            writer.state().bytes_written,
            1,
            "rotation must be attempted again once the limit is reached a second time"
        );

        // A failed create must not consume the suffix: nothing was written to
        // `.1.log`, so a later recovery has to reuse it rather than skip to
        // `.2.log` and leave a gap that reads as a deleted file.
        assert_eq!(
            writer.state().next_rotation,
            1,
            "a transient failure must not burn a rollover number"
        );

        // Writing continued in the original file and nothing new was created.
        writer.close();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaaaaaaabbccdddde");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        remove_dir_all(dir).unwrap();
    }

    /// Once the volume recovers, rollover must resume at the number the failed
    /// attempt did not consume, so the sequence stays contiguous.
    #[test]
    fn a_recovered_rollover_reuses_the_number_the_failure_left_free() {
        let dir = test_directory("rotation-recovery");
        let (path, file) = reserve_trace_file(&dir, "20260910123456789", 42).unwrap();
        let mut next_rotation = 1;
        let absent = dir.join("absent").join("mssql_tds_trace.log");

        // Fails: the parent directory does not exist.
        assert!(reserve_rotated_trace_file(&absent, &mut next_rotation).is_err());
        assert_eq!(next_rotation, 1, "a failed create must not take the number");

        // The same counter now yields `.1.log` against a usable directory.
        let (recovered, _handle) = reserve_rotated_trace_file(&path, &mut next_rotation).unwrap();
        assert_eq!(
            recovered.file_name().unwrap(),
            format!("{}.1.log", path.file_stem().unwrap().to_string_lossy()).as_str()
        );
        assert_eq!(next_rotation, 2);

        drop(file);
        remove_dir_all(dir).unwrap();
    }

    /// `MSSQL_TDS_TRACE_MAX_FILE_SIZE_MB` is operator-supplied, so every
    /// rejected shape must fall back to the default rather than propagate.
    ///
    /// Exercises [`bounded_value`] rather than [`bounded_env_u64`]: setting an
    /// environment variable is unsound while sibling tests may be reading one
    /// concurrently, so the validation is tested where it is pure.
    #[test]
    fn bounded_value_rejects_out_of_range_and_malformed_values() {
        const NAME: &str = "MSSQL_TDS_TRACE_MAX_FILE_SIZE_MB";
        const DEFAULT: u64 = 100;

        let bounded = |value: &str| bounded_value(NAME, value, DEFAULT, 1, 1024);

        for rejected in ["0", "1025", "abc", "", "-1", "12.5", "99999999999999999999"] {
            assert_eq!(bounded(rejected), DEFAULT, "{rejected:?} should fall back");
        }

        for (accepted, expected) in [("1", 1), ("512", 512), ("1024", 1024)] {
            assert_eq!(bounded(accepted), expected, "{accepted:?} is in range");
        }
    }

    #[test]
    fn an_unrelated_file_in_the_trace_directory_is_left_alone() {
        // Init-time behaviour is covered end-to-end in
        // `tests/e2e/tests/trace_rotation_test.cpp`, which loads the real driver.
        // Asserting it here would require installing a global tracing subscriber,
        // which would redirect every later test in this binary.
        let dir = test_directory("no-cleanup");
        let bystander = dir.join("mssql_tds_trace_from_another_process.log");
        std::fs::write(&bystander, b"keep").unwrap();

        let resolved = prepare_trace_directory(dir.clone()).unwrap();
        let (_path, file) = reserve_trace_file(&resolved, "20260910123456789", 42).unwrap();
        drop(file);

        assert_eq!(std::fs::read_to_string(&bystander).unwrap(), "keep");
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

    fn capture_correlation(work: impl FnOnce()) -> String {
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || TestWriter(Arc::clone(&sink)))
                .event_format(LogFormatter),
        );
        mssql_tds::trace_context::enable();
        tracing::subscriber::with_default(subscriber, work);
        String::from_utf8(output.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn describe_parameter_rpcs_start_executions_but_cached_reads_do_not_activate_them() {
        use crate::api::SQLDescribeParam;
        use crate::api::odbc_types::{SQL_ERROR, SQL_INTEGER, SQL_NULLABLE, SQL_SUCCESS};
        use crate::handles::stmt::{ParameterDescription, PreparedPlan};
        use crate::handles::{DbcHandle, StmtHandle, handle_from_raw};
        use crate::test_support::TestHandles;
        use mssql_tds::connection::tds_client::PreparedStatement;
        use mssql_tds::test_client_support::{done_no_more, sql_error, tds_client_from_tokens};

        let id = uuid::Uuid::new_v4();
        let output = capture_correlation(|| {
            let h = TestHandles::with_env_dbc_stmt();
            h.mark_dbc_connected();
            let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            dbc.trace.establish(id);
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            stmt.inner.lock().unwrap().prepared = Some(PreparedPlan {
                stmt: PreparedStatement::new("SELECT @P1".to_string()),
                marker_count: 1,
                original_sql: "SELECT ?".to_string(),
            });
            let describe = |parameter| unsafe {
                SQLDescribeParam(
                    h.stmt,
                    parameter,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            for execution in 1..=2 {
                dbc.inner.lock().unwrap().client = Some(tds_client_from_tokens(vec![
                    sql_error(11529, 16, "no metadata could be determined"),
                    done_no_more(),
                ]));
                assert_eq!(describe(1), SQL_ERROR);
                let _scope = unsafe { context_for(h.stmt, "SQLRowCount") }.enter();
                assert_eq!(mssql_tds::trace_context::snapshot().exec, Some(execution));
            }
            stmt.inner
                .lock()
                .unwrap()
                .parameter_metadata
                .push(ParameterDescription {
                    data_type: SQL_INTEGER,
                    parameter_size: 10,
                    decimal_digits: 0,
                    nullable: SQL_NULLABLE,
                });
            assert_eq!(describe(1), SQL_SUCCESS);
            assert_eq!(describe(0), SQL_ERROR);
            let _scope = unsafe { context_for(h.stmt, "SQLRowCount") }.enter();
            assert_eq!(mssql_tds::trace_context::snapshot().exec, Some(2));
        });
        let cid = format!("cid={} ", id.to_string().to_uppercase());
        for execution in 1..=2 {
            let execution = format!("exec={execution} ");
            let lines: Vec<_> = output
                .lines()
                .filter(|line| line.contains(&execution))
                .collect();
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("SQLDescribeParam called"))
            );
            assert!(lines.iter().any(|line| line.contains(", mssql_tds::")));
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("SQLDescribeParam: metadata RPC failed"))
            );
            assert!(
                lines.iter().any(|line| line.contains(", DEBUG, ")
                    && line.contains("SQLDescribeParam returning"))
            );
            assert!(
                lines
                    .iter()
                    .all(|line| line.contains(&cid) && line.contains("stmt="))
            );
        }
    }

    #[test]
    fn exported_execution_and_tds_events_share_correlation() {
        use crate::api::odbc_types::{SQL_NTS, SQL_SUCCESS};
        use crate::api::{SQLExecDirectW, SQLRowCount};
        use crate::handles::{DbcHandle, StmtHandle, handle_from_raw};
        use crate::test_support::TestHandles;
        use mssql_tds::test_client_support::{done_no_more, tds_client_from_tokens};

        let id = uuid::Uuid::new_v4();
        let output = capture_correlation(|| {
            let h = TestHandles::with_env_dbc_stmt();
            h.mark_dbc_connected();
            let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            dbc.trace.establish(id);
            for execution in 1..=2 {
                dbc.inner.lock().unwrap().client =
                    Some(tds_client_from_tokens(vec![done_no_more()]));
                let sql: Vec<u16> = "SELECT 1".encode_utf16().chain([0]).collect();
                assert_eq!(
                    unsafe { SQLExecDirectW(h.stmt, sql.as_ptr(), SQL_NTS) },
                    SQL_SUCCESS
                );
                let mut rows = 0;
                assert_eq!(unsafe { SQLRowCount(h.stmt, &mut rows) }, SQL_SUCCESS);
                let context = unsafe { context_for(h.stmt, "SQLRowCount") };
                let _scope = context.enter();
                assert_eq!(mssql_tds::trace_context::snapshot().exec, Some(execution));
            }
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            assert!(stmt.trace.is_some());
            assert_eq!(mssql_tds::trace_context::snapshot(), Default::default());
            let _diagnostic = unsafe { context_for(h.stmt, "SQLGetDiagRecW") }.enter();
            let snapshot = mssql_tds::trace_context::snapshot();
            assert_eq!(snapshot.cid, Some(id));
            assert!(snapshot.stmt.is_some());
            assert_eq!(snapshot.exec, None);
        });
        let expected_id = format!("cid={} ", id.to_string().to_uppercase());
        for execution in 1..=2 {
            let execution = format!("exec={execution} ");
            let lines: Vec<_> = output
                .lines()
                .filter(|line| line.contains(&execution))
                .collect();
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("SQLExecDirectW called"))
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("SQLExecDirectW returning"))
            );
            assert!(lines.iter().any(|line| line.contains("SQLRowCount called")));
            assert!(lines.iter().any(|line| line.contains(", mssql_tds::")));
            assert!(
                lines
                    .iter()
                    .all(|line| line.contains(&expected_id) && line.contains("stmt="))
            );
        }
    }

    #[test]
    fn panic_free_and_invalid_handle_logs_keep_safe_context() {
        use crate::api::odbc_types::SQL_ERROR;
        use crate::test_support::TestHandles;

        let id = uuid::Uuid::new_v4();
        let output = capture_correlation(|| {
            let mut h = TestHandles::with_env_dbc_stmt();
            let dbc =
                unsafe { crate::handles::handle_from_raw::<crate::handles::DbcHandle>(h.dbc) };
            dbc.trace.establish(id);
            let ret = crate::ffi_entry!(
                "CorrelationPanic",
                h.stmt,
                tracing::debug!("panic entry"),
                panic!("test panic")
            );
            assert_eq!(ret, SQL_ERROR);
            assert_eq!(mssql_tds::trace_context::snapshot(), Default::default());
            let extra = h.alloc_extra_stmt();
            h.free_extra_stmt(extra);
            let _invalid = unsafe { context_for(extra, "SQLFreeHandle") }.enter();
            assert_eq!(mssql_tds::trace_context::snapshot(), Default::default());
        });
        for marker in [
            "CorrelationPanic: panic caught",
            "CorrelationPanic returning",
            "Handle freed",
        ] {
            assert!(output.lines().any(|line| line.contains(marker)
                && line.contains(&format!("cid={} ", id.to_string().to_uppercase()))));
        }
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
