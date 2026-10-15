# Multithreaded Testing Plan

Concurrent ODBC use must not crash, deadlock, corrupt data, leak diagnostics
across handles, or leave a connection busy. PRs run every test single-threaded
plus small concurrent smoke runs; nightly and weekly runs hunt races.

## Approach

- **Shared-body tests (default).** A behavioral test is written once with
  `ODBC_TEST_F` in its API-group file, such as `get_data_test.cpp`, and also
  runs across threads. No second copy to drift.
- **Multithreaded-only scenarios** in `tests/e2e/tests/multithreaded_tests/`,
  for what has no single-threaded form: handle churn, races on one STMT, a
  shared APD, connect races, and cancel (`cancel_test.cpp`, from #703).

Separate copies drifted from the single-threaded suite (a fixed bug was
re-filed from stale expectations); running three GetData test bodies under
`SharedDbc` found AB#49225 without new test logic.

## Example

Converting a test changes only the macro:

```cpp
#include "odbc_concurrent_test.h"

// Before: runs once, on the main thread.
TEST_F(GetDataLiveTest, ReadsAnInt) { /* same body */ }

// After: same body, also run across threads.
ODBC_TEST_F(GetDataLiveTest, ReadsAnInt) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(42 AS INT)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(42, value);
    SQLCloseCursor(stmt_);
}
```

The macro registers these gtest tests itself at program start; the file only
has to be built by an `add_odbc_test` target, as existing test files are:

```text
GetDataLiveTest.ReadsAnInt              exactly as TEST_F
GetDataLiveTest.ReadsAnInt/CrossThread  SetUp, body, TearDown on three threads
GetDataLiveTest.ReadsAnInt/SharedEnv    THREADS x ITERS runs sharing one ENV
GetDataLiveTest.ReadsAnInt/SharedStmt   observers read the STMT's diagnostics
```

Every run of that body expects 42, so a result delivered to the wrong
connection would still pass. Derive the expected value from the run instead:

```cpp
ODBC_TEST_F(GetDataLiveTest, EachRunReadsItsOwnValue) {
    const std::string token = WorkerToken("getdata");  // e.g. "getdata-w2-i7"
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('" + token + "' AS VARCHAR(64))"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(token, GetChar(1));  // another worker's row cannot match
    SQLCloseCursor(stmt_);
}
```

`/SharedDbc` is opt-in:

```cpp
ODBC_TEST_F_MODES(GetDataLiveTest, EachRunReadsItsOwnValue,
                  odbc_concurrency::kAllModes) { /* same body */ }
```

## Modes

| Mode | What runs | Pass condition | Not for |
|---|---|---|---|
| `Single` | The body once, on the main thread | As `TEST_F` | - |
| `CrossThread` | SetUp, body, and TearDown on three different threads | As `TEST_F`; catches thread affinity (handles move between threads in pools, async runtimes, and finalizers) | - |
| `SharedEnv` | `THREADS` workers x `ITERS` runs; shared ENV, own DBC and STMT | Every run passes | Tests that change ENV, pooling, or process state; connect, disconnect, or driver-load tests; fixed-name permanent objects; reliance on `HasFatalFailure()` |
| `SharedDbc` (opt-in via `ODBC_TEST_F_MODES` with `kAllModes`) | `THREADS` workers x `ITERS` runs; shared DBC, own STMT | Only `HY000` connection busy may fail, and is retried; the connection is idle afterward | As `SharedEnv`, plus connection-attribute or transaction changes, shared temp-object names, bodies unsafe to rerun |
| `SharedStmt` | Owner runs the body `ITERS` times; `THREADS - 1` observers read its STMT and DBC diagnostics | Owner as `Single`; observers read in every iteration; every record well formed. Only the DBC reads overlap the owner's calls in the driver; STMT reads are serialized by the Driver Manager (see Driver Manager facts) | Bodies that free or replace `stmt_` or `dbc_` |

- gtest keeps one result per test, not per thread. `SharedEnv` and `SharedDbc`
  therefore capture each run's failures on its own thread (and `SharedDbc`
  classifies busy errors), so one worker's failure or skip cannot stop another,
  and `HasFatalFailure()` in the body does not see them. They are re-reported
  with their original file and line. `SharedStmt` observers hand their first
  error to the main thread, which reports it after joining.
- `SharedStmt` observers call only `SQLGetDiagRec` and `SQLGetDiagField`. Every
  other call clears the owner's diagnostics, and an idle `SQLCancel` also
  closes the open cursor on both drivers. Cross-thread cancel has its own test,
  `multithreaded_tests/cancel_test.cpp`.
- Idle-cancel bodies use `CancelIdleStatement()` to exclude diagnostic observers
  during `SQLCancel`, whose DM diagnostic cleanup is not protected by unixODBC's
  `Threading=1`. The post-close invalid-state probes also hold
  `diagnostics_mutex_` until a harmless attribute read clears their DM-generated
  errors. All other calls still overlap diagnostic reads, and no mode or
  assertion is skipped. Rust cancellation tests cover driver-level statement
  lock contention directly, without the unsafe DM overlap.
- There is no mode that runs the whole body on one shared STMT or descriptor.
  Bound buffers live on each body's stack, and even without bindings a STMT
  holds one cursor:

  ```text
  thread 1: SQLExecDirect("SELECT 'mine-1'")
  thread 2: SQLExecDirect("SELECT 'mine-2'")   replaces thread 1's results
  thread 1: SQLFetch + SQLGetData              reads "mine-2" (test fails, driver is right)
  thread 2: SQLFetch                           SQL_NO_DATA (thread 1 consumed the row)
  ```

  Shared-STMT and descriptor races are scenarios 8 and 9, which check
  invariants instead of values. `SharedDbc` is unaffected: each worker owns its
  STMT.

## Architecture

At program start the macro registers one gtest test per mode; at run time each
test hands its body to the runner for its mode.

```mermaid
flowchart TB
    write["1 · You write one test<br/>ODBC_TEST_F(GetDataLiveTest, Name) { body }"]

    subgraph register["2 · Registration, at program start (odbc_concurrent_test.h)"]
        cls["Generated test class<br/>your fixture + WorkerScope<br/>body becomes ConcurrentBody()"]
        reg["Register()<br/>adds one gtest test per mode:<br/>Name · Name/CrossThread · Name/SharedEnv<br/>Name/SharedStmt · Name/SharedDbc (opt-in)"]
        cls --> reg
    end

    subgraph run["3 · Run, one gtest test at a time (odbc_concurrent_test.h)"]
        dispatch["RunTest(mode)<br/>picks the runner"]
        runners["Mode runners<br/>Single: run body · RunCrossThread<br/>RunConcurrent: SharedEnv, SharedDbc<br/>RunSharedStmt"]
        dispatch --> runners
    end

    subgraph support["Used by every runner"]
        fixture["ODBCTest fixture<br/>(odbc_test_fixture.h / .cpp)<br/>SetUp: ENV · Connect: DBC + STMT<br/>TearDown: frees what it owns<br/>LendHandles: borrow ENV / DBC"]
        scope["WorkerScope<br/>Worker() · Iteration()<br/>WorkerToken(): values<br/>unique to each run"]
        helpers["Helpers<br/>LoadConfig: threads, iterations, timeout<br/>StartGate: start workers together<br/>Watchdog: dump a hung run<br/>Classify: retry 'connection busy'<br/>CheckDiagnostics · VerifyConnectionIdle"]
    end

    write --> cls
    reg --> dispatch
    runners --> support
```

How each runner uses the handles the gtest test instance allocated in its own
SetUp:

```mermaid
flowchart LR
    coord["gtest test instance<br/>SetUp: ENV + DBC + STMT"]

    coord -->|Single| single["Run the body once<br/>on the main thread"]

    coord -->|CrossThread| cross["New fixture<br/>thread A: SetUp<br/>thread B: body<br/>thread C: TearDown"]

    coord -->|SharedEnv| env["THREADS workers × ITERS iterations<br/>each iteration: new fixture<br/>borrows ENV, opens own DBC + STMT"]

    coord -->|SharedDbc| dbc["THREADS workers × ITERS iterations<br/>each iteration: new fixture<br/>borrows ENV + DBC, own STMT<br/>'connection busy' → retry"]

    coord -->|SharedStmt| stmt["Owner: ITERS iterations<br/>each: new fixture, borrows ENV,<br/>opens own DBC + STMT<br/>THREADS − 1 observers read<br/>the owner's diagnostics"]

    env --> checks["After the run<br/>every worker finished every iteration<br/>SharedDbc: connection still usable"]
    dbc --> checks
```

Each run in a concurrent mode is one fresh fixture (`CrossThread` borrows
nothing and allocates all its own handles).

**Threads and iterations.** `MSSQL_ODBC_MT_THREADS` is how many threads run at
once; `MSSQL_ODBC_MT_ITERS` is how many times each runs the body, one after
another. With `THREADS=3`, `ITERS=2`:

| Mode | `THREADS=3` means | Body runs | `Worker()`, `Iteration()` |
|---|---|---|---|
| `Single` | ignored | 1 | 0, 0 |
| `CrossThread` | ignored (always 3 threads, all alive, taking turns) | 1 | 0, 0 |
| `SharedEnv`, `SharedDbc` | 3 workers in parallel | 3 × 2 = 6, at most 3 at once | (0..2, 0..1) |
| `SharedStmt` | 1 owner + 2 observers | 2, one after another | (0, 0..1) |

Measured on `RunUniqueValuesSurviveEveryRetrievalPath`:

```text
[SharedEnv]: threads=3 iterations=2
[SharedEnv] completed: iterations=6 conflicts=0 elapsed_ms=77
[SharedStmt]: observers=2 iterations=2
[SharedStmt] completed: iterations=2 diagnostic_reads=1473 elapsed_ms=44
```

Threads create contention; iterations repeat it, so a narrow race window gets
more chances to be hit. AB#49225 reproduced reliably at 8x20 but not at 4x2.

## Running and debugging

| Setting | Default | Meaning |
|---|---|---|
| `MSSQL_ODBC_MT_THREADS` | 2 | Threads running at once |
| `MSSQL_ODBC_MT_ITERS` | 1 | Runs of the body per thread |
| `MSSQL_ODBC_MT_TIMEOUT_SECONDS` | 300 | Hang watchdog for each phase of a concurrent variant: fixture `SetUp`, the run, fixture `TearDown` |
| `MSSQL_ODBC_MT_DUMP_DIR` | working directory | Where the watchdog writes a Windows minidump |

Select variants with a gtest filter, on the command line or through
`GTEST_FILTER` (which also reaches every executable under `ctest` and
`run_e2e`). Match the mode suffixes, not every `/`: `TEST_P` names contain `/`
too.

```bash
# Single-threaded only
./get_data_test --gtest_filter='-*/CrossThread:*/SharedEnv:*/SharedStmt:*/SharedDbc'

# Multithreaded only, at stress size
MSSQL_ODBC_MT_THREADS=8 MSSQL_ODBC_MT_ITERS=20 \
  ./get_data_test --gtest_filter='*/CrossThread:*/SharedEnv:*/SharedStmt:*/SharedDbc'
```

- Failures name their run, e.g. `SharedEnv worker=2 iteration=7` or
  `CrossThread body thread`. The first failure stops all workers.
- On a hang the watchdog prints each worker's completed iterations, then
  writes a minidump of every thread and exits with 124 on Windows, or calls
  `abort()` for a core dump elsewhere. It leaves a process alone when a
  debugger is attached (detected on Windows, Linux, and macOS) or when it
  cannot tell.
- To tell a race from a plain bug, filter one variant and set
  `MSSQL_ODBC_MT_THREADS=1 MSSQL_ODBC_MT_ITERS=1` (one worker, or the
  `SharedStmt` owner with no observers).
- Use a conditional breakpoint such as `Worker()==3` when several workers run
  the body. Avoid `--gtest_break_on_failure` with `SharedDbc`: it also breaks
  on the busy failures the harness retries.

## Driver and Driver Manager facts

Measured 2026-10-08 on both drivers unless noted.

- A non-MARS connection serves one STMT at a time; another STMT gets `HY000`
  "Connection is busy".
- `SQLDriverConnect` holds the DBC mutex across connection setup (driver
  source; scenario 10 relies on it).
- `SQLDisconnect` frees child STMTs and can race an in-flight child call: a
  known lifetime bug, not a test scenario.
- `SQLCancel` is complete (AB#47083, closed; #703). It interrupts a running
  execute, fetch,
  `SQLGetData`, `SQLMoreResults`, `SQLPutData`, or final `SQLParamData` from
  another thread; the call fails `HY008` (msodbcsql's `SQLMoreResults`
  returns `SQL_NO_DATA`, [parity registry](parity-deviations.md) entry 27).
  `multithreaded_tests/cancel_test.cpp` covers all of those on both Driver
  Managers except a blocked `SQLGetData`, which entry 27 pins from source and
  Rust unit tests; its e2e case is listed under AB#49258 below.
- **The Windows Driver Manager serializes calls per handle.** A second call on
  a busy STMT, or on a descriptor associated with it, waits; only `SQLCancel`
  bypasses the lock. Calls on another STMT, the DBC, or an unassociated
  descriptor proceed.
- **Effect on the modes, on every platform** (Windows, and unixODBC at the
  `Threading=1` that `run_e2e.sh` sets, below): calls on one handle never
  reach the driver concurrently, except `SQLCancel`. `SharedDbc` reaches it
  concurrently through STMTs on one DBC. `SharedStmt` does through its DBC
  diagnostic reads only; its STMT reads run between the owner's calls, so
  that half checks crashes, malformed records, and wrong-thread access, not
  same-STMT races. It does not detect diagnostics kept per thread: zero
  records passes, since the owner may have cleared them. Same-STMT races
  are scenario 8, which loads the driver without a Driver Manager.
- **unixODBC `Threading`:** 0 none, 1 per handle (a STMT call locks that
  STMT, a DBC call that DBC), 2 per DBC, 3 global (which
  also locks `SQLCancel`). unixODBC master defaults to 0 when odbcinst.ini does
  not set it; `run_e2e.sh` sets 1 for both legs. At 0, unixODBC's own
  per-handle diagnostic list is unlocked: `/SharedStmt` crashed inside
  `libodbc` (under `SQLGetDiagField`) with either driver loaded, in 7 of 10
  runs at 2 x 1 (WSL, unixODBC 2.3.9), and in none of 60 at 1. macOS CI hit
  it first; Linux CI passed by luck.
- Every non-diagnostic call clears the handle's diagnostics, including an idle
  `SQLCancel`, `SQLGetStmtAttr`, and `SQLNumResultCols`. Cross-thread
  `SQLCancel` preserves the interrupted call's diagnostics.
- An idle `SQLCancel` performs `SQLFreeStmt(SQL_CLOSE)`, releases the connection,
  and reports `SQL_SUCCESS_WITH_INFO` / `01S05` when close succeeds (AB#49386).
  Driver Managers (Windows and unixODBC) suppress the warning, so the direct-entry
  Rust tests assert it while `ConnectionBusyLiveTest.IdleCancel*` checks the
  cursor cleanup and connection reuse through the Driver Manager. Close errors
  remain errors; running-command and parked-DAE cancellation are unchanged.
  unixODBC's `DriverManager/SQLCancel.c` scans the driver's diagnostic records
  with `IsDiagRec01S05` and converts `SQL_SUCCESS_WITH_INFO` to `SQL_SUCCESS`
  when it finds `01S05`. The Windows behavior is measured below; the unixODBC
  behavior here is source-confirmed and asserted by the Linux/macOS e2e lanes.
  Reference: `ExportImp::SQLCancel`, `odbc/sqlcmisc.cpp:779-791`, idle branch;
  `ExportImp::SQLFreeStmt`, `odbc/sqlccmd.cpp:404-432`, `SQL_CLOSE` branch.
  Measured on Windows on 2026-10-15 with msodbcsql `SQL_DRIVER_VER=18.06.0001`
  and the AB#49386 Rust debug build (`18.6.2.1`): direct-DLL calls closed
  unfetched, partially fetched, and exhausted cursors, returned `01S05`, and
  allowed a second statement to execute. The native Driver Manager leg also
  passed the eight `ConnectionBusyLiveTest.IdleCancel*` variants present on
  that date (two bodies, each in four concurrency modes).
  A Linux unixODBC 2.3.9 standalone mock-driver probe reproduced malformed
  diagnostic records through the DM at `Threading=1`, without mssql-odbc or
  SQL Server. Direct-driver observation avoided that DM corruption, but Windows
  msodbcsql can treat cancellation overlapping its diagnostic lock as a no-op.
  The e2e observer gate therefore excludes only those unsafe overlaps while
  keeping the idle-close contract deterministic on both drivers.
  Idle cancellation now abandons an owned pending response via ATTENTION,
  using `SQL_ATTR_CONNECTION_TIMEOUT` (zero selects 120 seconds) for settlement.
  It does not change the normal `SQLFreeStmt(SQL_CLOSE)` drain. The mock tests
  hold back a response tail and DONE_ATTN independently, cover partially read
  PLP data, and verify failed settlement retires the connection. The live
  `IdleCancelAbandonsDelayedBatchTail` case checks that a five-second WAITFOR
  and trailing INSERT are abandoned rather than completed by the close.
  Measured on Windows on 2026-10-17 with native `SQL_DRIVER_VER=18.06.0001`
  and the updated Rust debug DLL (`18.6.2.1`): both returned `01S05` in less
  than one millisecond and left the temporary table empty. This follows native
  `Cancel` (`sqlcmisc.cpp:847-851`) into the ATTENTION branch of
  `ProcessTDSStream` (`sqlccmd.cpp:11718-11731`).
  The live suite now also covers `IdleCancelAbandonsPartiallyReadPlp`: a
  `varchar(max)` value is read only through its first truncated `SQLGetData`
  chunk before cancellation, and a second statement verifies the delayed
  INSERT never ran. Together with the delayed non-PLP tail case, the suite
  contains four bodies in four modes (16 variants); the historical eight-case
  measurement above predates both additions.
  On 2026-10-17 all 16 variants passed through the Windows DM with native
  `18.06.0001` (five iterations per concurrent mode). Direct-DLL probes against
  that native version and Rust `18.6.2.1` also passed the partial-PLP case:
  truncated `SQLGetData`, `01S05` after cancellation, and no trailing INSERT.
  Remeasured on 2026-10-19 through temporary Windows user DSNs, with native
  `SQL_DRIVER_VER=18.06.0001` and Rust `18.6.2.1`: never-executed, unfetched,
  partially fetched, and exhausted statements all returned `SQL_SUCCESS` and
  `SQLGetDiagRec` returned `SQL_NO_DATA`. The live fixture now asserts this
  DM result rather than accepting either success code; direct-entry Rust
  tests continue to require `SQL_SUCCESS_WITH_INFO` / `01S05`.
  `ClosingFetchedParameterArrayPublishesCompletion` checks live close-time
  status/count outputs for two parameter sets. The Rust close/cancel test
  specifically models an exhausted wire with still-unpublished array results
  using scripted tokens: removing the parameter-array exception makes its
  output assertions fail. This state is modeled explicitly, since the current
  public `SQLMoreResults` path publishes completion as it exhausts the batch.

## Scenarios

### Shared-body (API-group files)

1. **Typed payload integrity** (`SharedEnv`): fetch every SQL Server type,
   including `sql_variant`, legacy LOBs, XML, optional JSON, and max-length
   values, with worker-, iteration-, and row-specific values. Stream
   max-length values in small `SQLGetData` chunks; every truncated chunk fills
   the buffer, even when that splits a UTF-8 character (checked only under a
   UTF-8 client encoding), and the indicator counts down or is `SQL_NO_TOTAL`.
   Covered for character targets from `varchar(max)`, `varchar(n)`, `text`,
   `nvarchar(max)`, `nvarchar(n)`, `ntext`, and `xml`, and for binary targets
   from `varbinary(max)` and `binary(n)`, which have no terminator and so fill
   all of `BufferLength`; one case of each ends on an exactly full chunk.
   Edge cases: NULL and empty values, length probes,
   abandoned partial reads, mid-row cursor close, surrogate pairs across
   chunks, typed-to-character conversions, multiple results, and long row
   streams. `vector` waits for a fetch contract.
2. **Parameters** (`SharedEnv`): prepared reuse and rebinding across C and SQL
   types, data-at-execution (including cancel of a partial stream), column- and
   row-wise arrays with statuses and ignored sets, output and return-value
   parameters, and parameterized errors that stay on their STMT. TVPs, `vector`
   and UDT parameters, and output or data-at-execution parameters in arrays wait
   for driver support.
3. **Shared DBC** (`SharedDbc`): opt in the eligible tests from 1 and 2 once
   AB#49263 runs the mode nightly only. Array DML stays out until a busy retry
   cannot duplicate rows.
4. **Shared STMT diagnostics** (`SharedStmt`): every converted test, especially
   those that post warnings and errors. Concurrent in the driver on the DBC
   only (see Driver Manager facts); same-STMT races are scenario 8.
5. **Thread handoff** (`CrossThread`): every converted test.
6. **Diagnostics isolation** (`SharedEnv`, `SharedDbc`): distinct concurrent
   errors appear only on their own handle and clear on successful reuse.

### Multithreaded-only (`tests/multithreaded_tests/`)

Purpose-built workloads using the same settings, start gate, and watchdog.

7. **Handle churn:** threads allocate ENV/DBC/STMT trees, connect, query,
   disconnect, and free; every iteration completes.
8. **One STMT shared across threads** (nightly only). Values are checkable only
   when threads do not execute or fetch.
   - **8a. Unbound races (build first):** threads race `SQLExecDirect`,
     `SQLExecute`, `SQLFetch`, `SQLGetData`, `SQLMoreResults`, `SQLCloseCursor`,
     `SQLNumResultCols`, and `SQLRowCount` with call-local buffers only, so the
     test process stays memory-safe.
   - **8b. Binding and descriptor races:** 8a plus `SQLBindCol`,
     `SQLBindParameter`, `SQLFreeStmt(SQL_UNBIND / SQL_RESET_PARAMS)`,
     `SQLSetStmtAttr`, `SQLCancel`, and updates and `SQLCopyDesc` on the STMT's
     own APD and ARD. Buffers outlive every thread. A cancelled call may also
     fail `HY008`.
   - **8a/8b checks:** one column shape for every query; self-checking values
     (a worker/row token with a checksum) so torn or mixed reads fail; only
     success, `SQL_NO_DATA`, `HY010`, `24000`, or `HY000` busy; afterwards
     `SQLFreeStmt` with `SQL_CLOSE`, `SQL_UNBIND`, and `SQL_RESET_PARAMS`, then a
     known query must return exact values and the connection must be idle. A
     missing record after `SQL_ERROR` is tolerated here only, since another
     thread can clear it.
   - **8c. Read-only metadata:** one thread executes (or prepares) a fixed
     query; then all threads repeatedly call `SQLNumResultCols`,
     `SQLDescribeCol`, `SQLColAttribute`, `SQLDescribeParam`, `SQLNumParams`,
     and `SQLGetStmtAttr`, and every answer must match exactly. Afterwards one
     `SQLFetch` returns the first row. No diagnostic checks.
9. **Shared explicit APD:** STMTs on one DBC share an APD with read-only input
   buffers; every execute delivers the bound values, busy retried. Not a shared
   ARD, where concurrent fetches write the same buffers by design.
10. **Same-DBC connect race:** two `SQLDriverConnect` calls on one disconnected
    DBC give one success and one state error (`HY010` or Driver Manager); the
    DBC stays usable. Debug and release builds; Rust-driver robustness, not
    parity.

Scenarios 8 and 9 need same-handle calls to reach the driver concurrently. No
Driver Manager provides that safely: Windows serializes them, and unixODBC at
`Threading=0` corrupts its own diagnostic list. So they call the driver
library directly, without a Driver Manager, which is also the
ThreadSanitizer target (AB#49259).

## Stress Profiles

| Profile | Driver | Threads x iterations | Runs | Platforms |
|---|---|---|---|---|
| PR | both | 2 x 1 | `Single`, `CrossThread`, `SharedEnv`, `SharedStmt` | Windows, Linux, macOS |
| Nightly | mssql-odbc | 8 x 20 | All concurrent modes, scenarios 7-10 | Windows, Linux, macOS; scenarios 8-9 without a Driver Manager |
| Nightly | msodbcsql | 2 x 1 | All concurrent modes | Windows, Linux |
| Weekly soak | mssql-odbc | 16 x 50 or time-bound | Same as nightly | Same as nightly |

unixODBC runs use `Threading=1` (see Driver Manager facts).

Measured on `get_data_test` with 22 converted tests (66 concurrent variants),
local SQL Server:

| Run | msodbcsql | mssql-odbc |
|---|---|---|
| Single-threaded, all tests | 6.5 s | 5.5 s |
| Concurrent variants at 2 x 1 | 3-4 s | 3-4 s |
| Concurrent variants at 16 x 50 | 96 s | 115 s |

- At 2 x 1 a single slow connect adds seconds to one variant (runs of up to
  9 s seen on both drivers), so PR timing is noisy at this size.
- 16 x 50 costs about 4.5-5 s per converted test, mostly `SharedEnv` connecting
  on every run. For about 700 converted tests that extrapolates to roughly an
  hour per driver per platform, so it is weekly. 8 x 20 is a fifth of that and
  reproduced AB#49225.
- msodbcsql is the reference: its runs check that tests suit concurrent modes,
  not its thread safety.
- PRs do not hunt races; their smoke run catches tests unfit for concurrency
  (fixed object names, process-wide settings) in the PR that adds them.
- Register each executable twice in CTest, filtered to single-threaded and
  concurrent variants, so concurrency failures do not hide parity results.
  Shard executables across parallel jobs.
- Until AB#49263 adds those filters, every registered variant runs in PR CI, so
  no test opts into `SharedDbc`.
- Nightly failures need a named triage owner.

## Validation

- Compare independent-connection outcomes with msodbcsql; compare shared-handle
  scenarios by invariants, not interleavings.
- Sanitizer builds must instrument both the Rust driver and the C++ harness.
- Hangs produce dumps automatically. Crashes do not on Windows by default: gtest
  turns off Windows Error Reporting when it catches exceptions
  (`SetErrorMode(SEM_NOGPFAULTERRORBOX)`), so nightly runs with
  `--gtest_catch_exceptions=0` and WER LocalDumps; Linux enables core dumps.
  That flag also lets a C++ exception from a test terminate the process
  instead of failing the test. A Debug build then stops at the CRT's modal
  `abort()` dialog until the watchdog dumps it, so nightly Windows runs use a
  Release build (observed locally).
- msodbcsql has failed or exited without a result a few times at 8 x 20 or more
  (one `0xC0000409` in 41 runs; a chunk-fill failure and a silent exit in about
  16 runs at 16 x 50), none reproducible on demand. Dumps will tell a harness
  bug from a reference-driver one.

## Known failures

- Fixed by AB#49386: idle `SQLCancel` now closes an open cursor and releases the
  connection rather than leaving it busy. This is not a deliberate deviation.
- Fixed: AB#49220 (empty `IMAGE` read as NULL), by PR #700; AB#49219 (`run_e2e`
  auto-detect used `sa` with no password), in this PR; AB#49225 (a busy shared
  DBC reported `HY000` "No active TDS client"), by PR #710.

## Migration

Tracked under AB#49256:

1. **AB#49257 (this PR):** the harness with all five modes; the
   `get_data_test.cpp` pilot (five converted tests, one worker-token test,
   fourteen chunk-fill tests); empty-value, NULL, and parameterized-`THROW` tests.
2. **AB#49263:** CTest split, PR filter, nightly and weekly profiles, dumps.
3. **AB#49260:** fetch and GetData suites, plus worker-token tests.
4. **AB#49258:** parameter and execution suites, plus worker-token tests; and
   remaining cancel cases `cancel_test.cpp` does not cover, added to it
   (idle cursor cleanup is covered by `connection_busy_test.cpp`, AB#49386):
   - A cancel just before execute does not leak into that or the next
     execution, through a Driver Manager (the driver's unit tests cover it
     directly).
   - A fetch blocked by a row lock held by a second connection, rather than a
     `WAITFOR` inside the batch.
   - A `SQLGetData` blocked mid-stream on a PLP column, the one operation in
     the cancel list above with no e2e case.
5. **AB#49262:** diagnostics, descriptor, metadata, and attribute suites;
   scenario 6.
6. **AB#49261:** scenarios 7 and 10.
7. **AB#49259:** scenarios 8a, 8b, 8c, and 9 under ThreadSanitizer.

## Deferred

- **Disconnect or free during use:** add to 8b once handle lifetimes are
  reference-counted or generation-checked.
- **Async execution and MARS:** once the driver supports them.
