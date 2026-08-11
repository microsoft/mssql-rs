// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Isolates the cost of the ODBC layer itself.
//!
//! `mssql-tds/benches/odbc_split.rs` measures the same query straight against
//! `TdsClient`. This bench runs the identical query through `SQLFetch` +
//! `SQLGetData`, so subtracting one from the other leaves the ODBC glue:
//! handle mutexes, diagnostics bookkeeping, and text rendering.
//!
//! The driver's exported entry points are called **directly**, with no Driver
//! Manager in the call path, for two reasons: `odbc32.dll` adds its own
//! per-call cost that would otherwise be attributed to us, and skipping it
//! makes an edit-measure cycle seconds rather than minutes.
//!
//! Configured with the same variables as the C++ harness:
//! `ODBC_TEST_SERVER`, `ODBC_TEST_DATABASE`, `ODBC_TEST_UID`,
//! `ODBC_TEST_PWD`, `ODBC_TEST_ENCRYPT`, `ODBC_TEST_TRUST_CERT`. The bench
//! reports "not configured" and exits cleanly when they are absent, so it is
//! safe to run in CI.

use std::env;
use std::ffi::c_void;

use criterion::measurement::{Measurement, ValueFormatter};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use msodbcsql18::api::{
    SQLAllocHandle, SQLCloseCursor, SQLDriverConnectW, SQLExecDirectW, SQLFetch, SQLFreeHandle,
    SQLGetData, SQLSetEnvAttr,
};
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection::tds_client::{CursorColumn, TdsClient};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting};

/// Wall time is useless here. Each iteration executes a real query, so roughly
/// 4 ms of the ~11 ms is spent waiting on the server, and that wait carries
/// enough variance to swamp what we are trying to see: back-to-back runs of an
/// *unmodified* binary differ by 4-5% with p < 0.05, while the changes worth
/// measuring in the ODBC layer are ~3%.
///
/// Process CPU time removes the server wait by construction and leaves only
/// work we can actually delete. It also matches what the C++ harness reports
/// as `cpu_time`, so numbers from the two are comparable.
struct CpuTime;

#[cfg(windows)]
mod cpu_clock {
    use std::ffi::c_void;
    use std::sync::OnceLock;
    use std::time::Instant;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn QueryProcessCycleTime(process: *mut c_void, cycle_time: *mut u64) -> i32;
    }

    /// CPU cycles consumed by every thread in the process. Counting all threads
    /// is deliberate: Tokio runtime threads are part of the cost we measure.
    ///
    /// `GetProcessTimes` was tried first and rejected — it is quantized to the
    /// ~15.6 ms scheduler tick, and one iteration here is only ~7 ms of CPU, so
    /// nearly all of its resolution went to quantization noise.
    fn cycles() -> u64 {
        let mut c = 0u64;
        let ok = unsafe { QueryProcessCycleTime(GetCurrentProcess(), &mut c) };
        assert!(ok != 0, "QueryProcessCycleTime failed");
        c
    }

    /// Cycles are the precise unit, but nanoseconds are the readable one and
    /// make these numbers directly comparable to the C++ harness's `cpu_time`.
    /// Calibrate once by busy-spinning a known wall interval on this thread.
    fn cycles_per_nano() -> f64 {
        static CAL: OnceLock<f64> = OnceLock::new();
        *CAL.get_or_init(|| {
            let (c0, t0) = (cycles(), Instant::now());
            while t0.elapsed().as_millis() < 100 {
                std::hint::spin_loop();
            }
            let (c1, t1) = (cycles(), t0.elapsed());
            (c1 - c0) as f64 / t1.as_nanos() as f64
        })
    }

    pub fn now_nanos() -> u64 {
        (cycles() as f64 / cycles_per_nano()) as u64
    }
}

#[cfg(not(windows))]
mod cpu_clock {
    /// Falls back to wall time off Windows so the bench still builds and runs;
    /// only the Windows numbers are noise-free enough to compare.
    pub fn now_nanos() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }
}

impl Measurement for CpuTime {
    type Intermediate = u64;
    type Value = u64;

    fn start(&self) -> Self::Intermediate {
        cpu_clock::now_nanos()
    }

    fn end(&self, start: Self::Intermediate) -> Self::Value {
        cpu_clock::now_nanos().saturating_sub(start)
    }

    fn add(&self, a: &Self::Value, b: &Self::Value) -> Self::Value {
        a + b
    }

    fn zero(&self) -> Self::Value {
        0
    }

    fn to_f64(&self, value: &Self::Value) -> f64 {
        *value as f64
    }

    fn formatter(&self) -> &dyn ValueFormatter {
        &CpuTimeFormatter
    }
}

struct CpuTimeFormatter;

impl ValueFormatter for CpuTimeFormatter {
    fn scale_values(&self, typical_ns: f64, values: &mut [f64]) -> &'static str {
        let (factor, unit) = if typical_ns < 1_000.0 {
            (1.0, "ns")
        } else if typical_ns < 1_000_000.0 {
            (1e-3, "us")
        } else {
            (1e-6, "ms")
        };
        for v in values.iter_mut() {
            *v *= factor;
        }
        unit
    }

    fn scale_throughputs(
        &self,
        _typical_ns: f64,
        throughput: &Throughput,
        values: &mut [f64],
    ) -> &'static str {
        let count = match throughput {
            Throughput::Elements(n) => *n as f64,
            Throughput::Bytes(n) => *n as f64,
            _ => 1.0,
        };
        for v in values.iter_mut() {
            // ns per iteration -> items per second
            *v = count * 1e9 / *v / 1000.0;
        }
        "Kelem/s"
    }

    fn scale_for_machines(&self, _values: &mut [f64]) -> &'static str {
        "ns"
    }
}

const SQL_HANDLE_ENV: i16 = 1;
const SQL_HANDLE_DBC: i16 = 2;
const SQL_HANDLE_STMT: i16 = 3;
const SQL_ATTR_ODBC_VERSION: i32 = 200;
const SQL_OV_ODBC3: usize = 3;
/// `sqlext.h` defines `SQL_NTS` as `-3` (`-1` is `SQL_NULL_DATA`).
const SQL_NTS: i16 = -3;
const SQL_DRIVER_NOPROMPT: u16 = 0;
const SQL_C_CHAR: i16 = 1;
const SQL_SUCCESS: i16 = 0;
const SQL_SUCCESS_WITH_INFO: i16 = 1;
const SQL_NO_DATA: i16 = 100;

/// Matches `kRows` in `mssql-odbc/tests/perf/benches/datatype_bench.cpp` and
/// `ROWS` in `mssql-tds/benches/odbc_split.rs`.
const ROWS: u64 = 5000;

fn ok(rc: i16) -> bool {
    rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Byte-for-byte the query behind `BM_Type_Int`.
fn int_query() -> String {
    format!(
        "SELECT TOP ({ROWS}) CAST(a.object_id AS INT) AS v \
         FROM sys.all_objects a CROSS JOIN sys.all_objects b"
    )
}

fn connection_string() -> Option<String> {
    dotenv::dotenv().ok();
    let server = env::var("ODBC_TEST_SERVER").ok()?;
    let database = env::var("ODBC_TEST_DATABASE").ok()?;
    let uid = env::var("ODBC_TEST_UID").ok()?;
    let pwd = env::var("ODBC_TEST_PWD").ok()?;
    let encrypt = env::var("ODBC_TEST_ENCRYPT").unwrap_or_else(|_| "Optional".into());
    let trust = env::var("ODBC_TEST_TRUST_CERT").unwrap_or_else(|_| "Yes".into());
    Some(format!(
        "SERVER={server};DATABASE={database};UID={uid};PWD={pwd};\
         Encrypt={encrypt};TrustServerCertificate={trust};"
    ))
}

/// Owns the env/dbc/stmt triple so an early return still frees them.
struct Handles {
    env: *mut c_void,
    dbc: *mut c_void,
    stmt: *mut c_void,
}

impl Drop for Handles {
    fn drop(&mut self) {
        unsafe {
            SQLFreeHandle(SQL_HANDLE_STMT, self.stmt);
            SQLFreeHandle(SQL_HANDLE_DBC, self.dbc);
            SQLFreeHandle(SQL_HANDLE_ENV, self.env);
        }
    }
}

fn connect() -> Option<Handles> {
    let conn_str = connection_string()?;
    unsafe {
        let mut env_handle: *mut c_void = std::ptr::null_mut();
        assert!(
            ok(SQLAllocHandle(
                SQL_HANDLE_ENV,
                std::ptr::null_mut(),
                &mut env_handle
            )),
            "SQLAllocHandle(ENV) failed"
        );
        assert!(
            ok(SQLSetEnvAttr(
                env_handle,
                SQL_ATTR_ODBC_VERSION,
                SQL_OV_ODBC3 as *mut c_void,
                0
            )),
            "SQLSetEnvAttr failed"
        );

        let mut dbc: *mut c_void = std::ptr::null_mut();
        assert!(
            ok(SQLAllocHandle(SQL_HANDLE_DBC, env_handle, &mut dbc)),
            "SQLAllocHandle(DBC) failed"
        );

        let wide_conn = wide(&conn_str);
        let rc = SQLDriverConnectW(
            dbc,
            std::ptr::null_mut(),
            wide_conn.as_ptr(),
            SQL_NTS,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            SQL_DRIVER_NOPROMPT,
        );
        assert!(ok(rc), "SQLDriverConnectW failed: {rc}");

        let mut stmt: *mut c_void = std::ptr::null_mut();
        assert!(
            ok(SQLAllocHandle(SQL_HANDLE_STMT, dbc, &mut stmt)),
            "SQLAllocHandle(STMT) failed"
        );

        Some(Handles {
            env: env_handle,
            dbc,
            stmt,
        })
    }
}

/// Drains every row the way `perf_fixture::DrainRows` does: unbound `SQLFetch`,
/// then one `SQLGetData` as `SQL_C_CHAR` per column. Returns the row count so a
/// miscounted loop cannot silently pass.
///
/// `get_data == false` walks the same rows without materializing any column,
/// which is what `SQLFetch` alone costs. Both variants drain the full rowset,
/// so they differ only in column materialization — no confound from the cursor
/// being closed with rows still on the wire.
fn drain(stmt: *mut c_void, query: &[u16], buf: &mut [u8], get_data: bool) -> u64 {
    let mut rows = 0u64;
    unsafe {
        let rc = SQLExecDirectW(stmt, query.as_ptr(), SQL_NTS);
        assert!(ok(rc), "SQLExecDirectW failed: {rc}");

        loop {
            let rc = SQLFetch(stmt);
            if rc == SQL_NO_DATA {
                break;
            }
            assert!(ok(rc), "SQLFetch failed: {rc}");

            if get_data {
                let mut indicator: isize = 0;
                let rc = SQLGetData(
                    stmt,
                    1,
                    SQL_C_CHAR,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as isize,
                    &mut indicator,
                );
                assert!(ok(rc), "SQLGetData failed: {rc}");
            }
            rows += 1;
        }

        SQLCloseCursor(stmt);
    }
    rows
}

/// Connects a bare `TdsClient` using the same credentials the ODBC cases use,
/// so the TDS and ODBC numbers in a run are directly subtractable.
fn connect_tds(rt: &tokio::runtime::Runtime) -> Option<TdsClient> {
    dotenv::dotenv().ok();
    let server = env::var("ODBC_TEST_SERVER").ok()?;
    let (host, port) = match server.split_once(',') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()?),
        None => (server, 1433),
    };

    let mut context = ClientContext::default();
    context.user_name = env::var("ODBC_TEST_UID").ok()?;
    context.password = env::var("ODBC_TEST_PWD").ok()?;
    context.database = env::var("ODBC_TEST_DATABASE").unwrap_or_default();
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::PreferOff,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };

    rt.block_on(async {
        TdsConnectionProvider {}
            .create_client(context, &format!("tcp:{host},{port}"), None)
            .await
            .ok()
    })
}

/// TDS-side twin of [`drain`]: walks the same rows through `TdsClient` with no
/// ODBC layer in between, so `drain` minus this is the ODBC glue cost.
async fn drain_tds(client: &mut TdsClient, query: &str, get_data: bool) -> u64 {
    client.execute(query.to_string(), ()).await.unwrap();
    let mut rows = 0u64;
    loop {
        while client.next_row_cursor().await.unwrap() {
            if get_data {
                match client.read_row_column(0).await.unwrap() {
                    CursorColumn::Value(_) => {}
                    other => panic!("unexpected cursor column: {other:?}"),
                }
            }
            rows += 1;
        }
        if !client.advance_to_rows().await.unwrap() {
            break;
        }
    }
    rows
}

fn odbc_glue(c: &mut Criterion<CpuTime>) {
    let Some(handles) = connect() else {
        eprintln!(
            "odbc_glue: not configured (need ODBC_TEST_SERVER, ODBC_TEST_DATABASE, \
             ODBC_TEST_UID, ODBC_TEST_PWD); skipping"
        );
        return;
    };

    let query = wide(&int_query());
    let mut buf = vec![0u8; 8192];

    // Fail loudly here rather than benchmarking an empty loop.
    let rows = drain(handles.stmt, &query, &mut buf, true);
    assert_eq!(rows, ROWS, "expected {ROWS} rows, drained {rows}");

    let mut group = c.benchmark_group("odbc_glue");
    group.throughput(Throughput::Elements(ROWS));
    group.sample_size(
        env::var("BENCH_SAMPLES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30),
    );
    group.bench_function("fetch_only", |b| {
        b.iter(|| drain(handles.stmt, &query, &mut buf, false));
    });
    group.bench_function("fetch_getdata_int", |b| {
        b.iter(|| drain(handles.stmt, &query, &mut buf, true));
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    if let Some(mut client) = connect_tds(&rt) {
        let tds_query = int_query();
        let rows = rt.block_on(drain_tds(&mut client, &tds_query, true));
        assert_eq!(rows, ROWS, "expected {ROWS} TDS rows, drained {rows}");

        group.bench_function("tds_fetch_only", |b| {
            b.iter(|| rt.block_on(drain_tds(&mut client, &tds_query, false)));
        });
        group.bench_function("tds_fetch_column", |b| {
            b.iter(|| rt.block_on(drain_tds(&mut client, &tds_query, true)));
        });
    } else {
        eprintln!("odbc_glue: TDS-direct cases skipped — could not connect TdsClient");
    }

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().with_measurement(CpuTime);
    targets = odbc_glue
}
criterion_main!(benches);
