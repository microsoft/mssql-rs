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

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use msodbcsql18::api::{
    SQLAllocHandle, SQLCloseCursor, SQLDriverConnectW, SQLExecDirectW, SQLFetch, SQLFreeHandle,
    SQLGetData, SQLSetEnvAttr,
};

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
fn drain(stmt: *mut c_void, query: &[u16], buf: &mut [u8]) -> u64 {
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
            rows += 1;
        }

        SQLCloseCursor(stmt);
    }
    rows
}

fn odbc_glue(c: &mut Criterion) {
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
    let rows = drain(handles.stmt, &query, &mut buf);
    assert_eq!(rows, ROWS, "expected {ROWS} rows, drained {rows}");

    let mut group = c.benchmark_group("odbc_glue");
    group.throughput(Throughput::Elements(ROWS));
    group.sample_size(
        env::var("BENCH_SAMPLES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30),
    );
    group.bench_function("fetch_getdata_int", |b| {
        b.iter(|| drain(handles.stmt, &query, &mut buf));
    });
    group.finish();
}

criterion_group!(benches, odbc_glue);
criterion_main!(benches);
