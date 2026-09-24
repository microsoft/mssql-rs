---
applyTo: "mssql-odbc/**"
---

# mssql-odbc Engineering Instructions

Crate-specific requirements for changes under `mssql-odbc/`.

## Index

- [1. Before making changes](#1-before-making-changes)
- [2. Parity reference: the classic C++ msodbcsql driver](#2-parity-reference-the-classic-c-msodbcsql-driver)
- [2.1. Verifying parity claims and recording deviations](#21-verifying-parity-claims-and-recording-deviations)
- [2.2. ODBC version handling](#22-odbc-version-handling)
- [3. No panics](#3-no-panics)
- [4. Diagnostics and error handling](#4-diagnostics-and-error-handling)
- [5. Unsafe code](#5-unsafe-code)
- [6. Ownership and memory](#6-ownership-and-memory)
- [6.1. Application lifetime contracts and bug scope](#61-application-lifetime-contracts-and-bug-scope)
- [7. Concurrency](#7-concurrency)
- [7.1. Handle hierarchy and locking](#71-handle-hierarchy-and-locking)
- [7.1.1. DM guarantees we rely on](#711-dm-guarantees-we-rely-on)
- [7.1.2. Locking rules](#712-locking-rules-mirroring-msodbcsql)
- [7.2. Known descriptor concurrency gap](#72-known-descriptor-concurrency-gap)
- [7.3. Prepared parameter definitions](#73-prepared-parameter-definitions)
- [8. FFI boundary conventions](#8-ffi-boundary-conventions)
- [9. Types and casts](#9-types-and-casts)
- [10. Testing](#10-testing)

## 1. Before making changes

- Read [mssql-odbc/README.md](../../mssql-odbc/README.md) for project status,
  architecture, and build/test instructions.
- Follow the repository-wide conventions in
  [copilot-instructions.md](../copilot-instructions.md).

## 2. Parity reference: the classic C++ msodbcsql driver

The classic C++ Microsoft ODBC Driver for SQL Server is the authoritative
implementation reference for compatibility work. Its source is in the
`SqlClientDrivers` Azure DevOps organization, `msodbcsql` project and repository,
on the `master` branch.

### 2.1. Verifying parity claims and recording deviations

- Read the owning msodbcsql caller and implementation before matching,
  rejecting, or documenting behavior. Do not infer behavior from MS Learn or
  another SQL Server driver.
- A source reading does not establish retail behavior. Support every behavioral
  parity claim with both a source citation (file, function, and relevant branch)
  and a measurement that records `SQL_DRIVER_VER` and the tested build. Where
  the Driver Manager prevents the claim from being measured through a normal
  application path, a source citation alone is admissible if the entry states
  its evidence level and names the measurement that would close it.
- CI compares against the version pinned by `msodbcsqlVersion` in
  `.pipeline/validation-pipeline.yml`. Use the e2e runner's
  `--compare-with-msodbcsql` mode for observable parity checks.
- `SKIP_IF_COMPARING_MSODBCSQL()` removes the assertion from the
  reference-driver leg, so add or retain it only for one of three reasons:
  a case asserting mssql-odbc-specific behavior the reference does not share at
  all (a "not implemented" response, for example); a measured divergence; or a
  documented gap tracked by a work item where asserting the reference leg would
  need a server capability the suite cannot assume. Anything else should assert
  on both legs. When the test exists solely to pin one divergence that already
  has a registry entry, prefer asserting each leg's expected result over
  skipping, so the reference stays measured on every run; see the carve-out in
  [the e2e README](../../mssql-odbc/tests/e2e/README.md).
- A consumer-based divergence requires evidence from both the consumer's
  routing path and its delivery path.
- Record every deliberate deviation that meets the registry's
  [entry criteria](../../mssql-odbc/docs/parity-deviations.md#updating-this-document)
  in that file as part of the same change. The registry entry must state what
  msodbcsql does, what this driver does instead, why the difference is
  intentional, and any required sign-off. Link a work item when one tracks
  follow-up work. Update the entry whenever the decision changes.
- Do not register an unimplemented behavior as a deliberate deviation. Track
  it as an implementation gap instead. The registry explains the exact
  boundary between these categories.

### 2.2. ODBC version handling

- **Supported contract: ODBC 3.x only.** The exported
  `SQLSetEnvAttr(SQL_ATTR_ODBC_VERSION)` implementation accepts
  `SQL_OV_ODBC3` and `SQL_OV_ODBC3_80`. It rejects `SQL_OV_ODBC2` and every
  other value with `SQL_ERROR` / `HY024` without changing the environment's
  previously selected version. **Preserve this behavior** and its
  `api::set_env_attr` unit tests.
- **Driver Manager behavior is not driver behavior.** A Driver Manager owns its
  own environment state and answers the application from it, so a test that
  invokes `SQLSetEnvAttr` through a Driver Manager measures the Driver Manager,
  not this driver's setter. Do **not** assume the Driver Manager maps a 2.x
  application onto the 3.x interface on the driver's behalf: unixODBC replays
  the application's declared version verbatim
  (`DriverManager/SQLConnect.c:1532-1538`), which is why the driver enforces
  the contract itself at `SQLAllocHandle(SQL_HANDLE_DBC)`. See
  [registry entry 14](../../mssql-odbc/docs/parity-deviations.md). **Do not add
  ODBC 2.x application behavior to the driver.**
- **Advertised driver version: ODBC 3.80.** The implemented
  `SQLGetInfo(SQL_DRIVER_ODBC_VER)` response is `"03.80"`.
  `SQL_ODBC_VER` describes the Driver Manager when one is present; the direct
  driver entry point also currently returns `"03.80"`. This represents the highest
  supported ODBC interface version for this driver.
- **Deprecated C identifiers remain valid in ODBC 3.x.** Normalize
  `SQL_C_DATE`, `SQL_C_TIME`, and `SQL_C_TIMESTAMP` to `SQL_C_TYPE_DATE`,
  `SQL_C_TYPE_TIME`, and `SQL_C_TYPE_TIMESTAMP` with
  `api::type_rules::canonical_c_type` before validation or conversion. **Do
  not reject an identifier merely because it originated in ODBC 2.x.**
  **Why:** ODBC 3.x headers still define these deprecated aliases, so a 3.x
  application may legally pass them without any Driver Manager translation.
- **C-type normalization does not apply to SQL type identifiers.** Values `9`
  and `10` are ambiguous in ODBC 3.x (`SQL_DATE`/`SQL_DATETIME` and
  `SQL_TIME`/`SQL_INTERVAL`). Parameter SQL types use the ODBC 3.x concise
  identifiers `SQL_TYPE_DATE`, `SQL_TYPE_TIME`, and `SQL_TYPE_TIMESTAMP` (`91`
  through `93`); ambiguous values are rejected with `HY004`. **Why:** folding
  an ambiguous SQL identifier would guess the caller's intent and could bind a
  different SQL type; the corresponding C identifiers have no such ambiguity.
- **Preserve the ODBC 3.0-to-3.8 behavior boundary.** Both versions are
  supported, but `SQL_C_DEFAULT` resolution uses `SQL_C_SS_TIME2` and
  `SQL_C_SS_TIMESTAMPOFFSET` only when the environment selected
  `SQL_OV_ODBC3_80`; `SQL_OV_ODBC3` uses the pre-3.8 defaults. **Keep this check
  centralized in `OdbcVersion::uses_3_80_types`.** **Why:** those extended C
  types entered the contract at ODBC 3.8; returning them to a 3.0 application
  would expose types and buffer layouts it did not declare support for.
- **Read the msodbcsql API entry point before interpreting downstream
  validators.** Its `SQLBindParameter` path maps `SQL_TYPE_DATE`,
  `SQL_TYPE_TIME`, and `SQL_TYPE_TIMESTAMP` to their older values, and maps
  `SQL_DOUBLE` to `SQL_FLOAT`, before validation. A downstream 2.x identifier
  therefore does not establish that an ODBC 2.x branch belongs in this driver.
  **Why:** reading only the validator loses the caller's normalization context
  and can make a transformed ODBC 3.x input look like native ODBC 2.x support.

## 3. No panics

- **Never** use `.unwrap()` or `.expect()` on `Result` or `Option` in
  non-test code. Tests under `#[cfg(test)]` may use them since panics
  there are caught by the test harness.
- Use `.unwrap_or()`, `.unwrap_or_else()`, `.unwrap_or_default()`, or
  pattern matching instead.
- For `Mutex::lock()`, return `SQL_ERROR` on poison — use `let Ok(state) = handle.inner.lock() else { return SQL_ERROR; }`. Do **not** recover via `e.into_inner()`.
- Never use `unreachable!()`, `todo!()`, or `unimplemented!()` in non-test code.
  Use explicit error returns instead.
- Array/slice access: prefer `.get()` over indexing (`[]`), which panics on
  out-of-bounds.

## 4. Diagnostics and error handling

- All fallible internal functions should return `Result<T, E>` — never panic on
  failure.
- Map errors early: convert `Result` from external crates into the crate's own
  error types at the call site.
- At FFI boundaries, convert every `Result::Err` into the appropriate
  `SqlReturn` code (`SQL_ERROR`, `SQL_INVALID_HANDLE`, etc.).
- Store diagnostic info on the handle so `SQLGetDiagRec` / `SQLGetDiagField`
  can report it — don't discard error details. Three posters, choose by source:
  - `post_diag(state, DiagMsg)` — **preferred for driver-raised diagnostics
    that have a canonical SQLSTATE + message.** A `DiagMsg` bundles a fixed
    SQLSTATE with its message text into a single `ERR_*` constant in
    `sqlstate.rs` (e.g. `ERR_INVALID_CURSOR_STATE`, `ERR_FUNCTION_SEQUENCE`,
    `ERR_CONNECTION_DOES_NOT_EXIST`). This keeps a call site from pairing a
    message with the wrong SQLSTATE and defines a reused message exactly once,
    mirroring msodbcsql's `IDS_*` resource entries. In new code, prefer
    adding/using an `ERR_*` `DiagMsg` constant over inlining `post_sql_error`
    with a literal — especially when the same `(SQLSTATE, message)` pair
    appears, or could appear, in more than one place.
  - `post_sql_error(state, sqlstate, native, message)` — the lower-level
    primitive behind `post_diag`. Use it directly only for genuinely one-off
    or **dynamic** messages (text computed at runtime) that don't warrant a
    constant. Posts exactly one record.
  - `post_tds_error(state, &tds_err, default_sqlstate)` — for any
    `mssql_tds::TdsError` bubbling up from the protocol layer. For
    `TdsError::SqlServerError` it fans out to one record per server-reported
    error, mapping each error number to a SQLSTATE via the static
    `SERVER_ERROR_TO_SQL_STATE_MAP` and falling back to the message's TDS
    severity class when the number is unmapped (`> 18` → `HY000`, `> 10` →
    `42000`, else `01000`, matching msodbcsql's `sqlcerr.cpp:1385-1401`); for
    other variants it posts a single record using `default_sqlstate`. Pick
    `08001` for connect-time failures and `HY000` for execution/fetch
    failures. Do not add rows to `SERVER_ERROR_TO_SQL_STATE_MAP` to correct a
    single error's SQLSTATE — entries there are a permanent compatibility
    commitment, and the severity fallback already covers unmapped errors.
  Never hand-roll `post_sql_error` over a `TdsError` — you lose the
  per-server-error fan-out and the SQLSTATE mapping.
- Every ODBC entry point must clear the handle's diagnostic records at API
  entry by calling `free_errors(...)` after acquiring the handle lock, so a
  fresh call starts without stale diagnostics.
- **A success code is a promise about the caller's buffer, so never report
  `SQL_SUCCESS` for a call that wrote less than the indicator claims.** Report
  `SQL_SUCCESS_WITH_INFO` with `01004` when a read truncates, so the caller knows
  to grow its buffer.

## 5. Unsafe code

- Minimize `unsafe` blocks — keep them as small as possible and comment
  the safety invariant they rely on.
- All raw-pointer writes must be guarded by a null check first.
- For the ubiquitous "write to caller out-param if non-null" pattern, use
  `crate::api::util::write_if_some(ptr, value)` instead of hand-rolling
  `if !ptr.is_null() { unsafe { ptr.write(v) } }`. The helper is the single
  audited chokepoint for that pattern. Skip the helper only when an outer
  null check guards expensive work that should be elided on null
  (e.g., looking up a value before writing it).
- Never dereference a pointer received from C without validating it.
- **Never assume an application buffer is aligned.** ODBC does not require the
  application to align `ParameterValuePtr`, `TargetValuePtr`, or
  `StrLen_or_IndPtr` for the type being transferred — an app may point at an
  offset inside a packed struct or a byte array. Read and write them with
  `read_unaligned` / `write_unaligned` (or `copy_*` helpers) rather than `*ptr`,
  `ptr::read`, or a reference. This is not defensive: in Rust a misaligned plain
  read is undefined behavior on *every* target, not just the ones that fault, and
  the optimizer is entitled to exploit it. msodbcsql reaches the same conclusion
  in C++ by qualifying every one of these accesses `UNALIGNED` (MSVC's
  `__unaligned`) — see `Sql/Ntdbms/sqlncli/odbc/sqlccnvt.cpp:1677-1714`, where
  each integer source read from an application buffer is
  `*(UNALIGNED SCHAR *)` / `SHORT` / `LONG` and so on.
- Use `unsafe fn` only when correctness relies on an unverifiable caller
  promise. Otherwise use a safe function with small, justified `unsafe` blocks.
- Use `#[unsafe(no_mangle)]` only in `exports.rs`; keep implementation
  functions in separate modules with `pub(crate)` visibility.

## 6. Ownership and memory

- **Same side allocates and frees.** Whoever produced an allocation owns
  freeing it; the FFI boundary never transfers deallocation responsibility:
  - Rust-allocated memory (`Box`, `Vec`, `String`, anything from
    `Box::into_raw` / `handle_to_raw`) must be freed by Rust via the
    matching `SQLFreeHandle` / `Box::from_raw` path. Never expect the
    caller (DM or app) to `free()` it, and never `mem::forget` it without
    a paired free path.
  - Caller-provided out-buffers (`*mut SQLCHAR` for `SQLGetData`, output
    pointers for `SQLDescribeCol`, etc.) are owned by the caller. Write
    into them, but never `free`, `realloc`, or wrap them in a `Box` —
    doing so hands them to Rust's allocator and corrupts the caller's
    memory.
- Prefer `Box` for single-owner heap objects; use `Arc` only when shared
  ownership is genuinely required.

### 6.1. Application lifetime contracts and bug scope

- Applications must not use a handle after it is freed. `SQLDisconnect` also
  releases associated statements and explicitly allocated descriptors. A
  surviving Driver Manager wrapper does not establish that its old driver
  handle is still usable. See [SQLFreeHandle](https://learn.microsoft.com/sql/odbc/reference/syntax/sqlfreehandle-function).
- Applications own buffer allocation and must preserve buffers while the
  driver still requires them. Do not infer that a concurrent descriptor setter
  returning success makes an earlier outstanding fetch's buffers safe to free.
  See [Allocating and Freeing Buffers](https://learn.microsoft.com/sql/odbc/reference/develop-app/allocating-and-freeing-buffers).
- Crash prevention for freed application handles or prematurely freed buffers
  is not a driver requirement. Do not add global identity, ownership, or
  per-call admission machinery solely to harden those invalid uses.
- Concurrent calls are not automatically misuse: ODBC requires thread safety.
  Before redesigning synchronization for a reported race, establish a supported
  call sequence, what the Driver Manager already enforces, and a reproducer
  with valid submitted handles and buffers retained through call completion.
  A source-level race or the absence of a classic-driver lock alone does not
  establish the application contract. Keep proven internal fixes narrowly scoped.

## 7. Concurrency

- The ODBC spec allows Driver Manager to call functions on the same handle
  from different threads. Protect mutable state with `Mutex` or `RwLock`.
- Keep lock scopes narrow — lock, copy/update, unlock. Never hold a lock
  across an FFI call or I/O operation.
- Handle poison explicitly with `std::sync::Mutex` — see the no-panics
  rule above for the canonical `let Ok(state) = ... else { return SQL_ERROR; }`
  pattern.

### 7.1. Handle hierarchy and locking

ODBC handles form an ownership hierarchy: ENV owns DBCs, and a DBC owns both
STMTs and DESCs. A STMT may associate with a DESC but does not own it. The
Driver Manager (DM) provides serialization guarantees that the driver relies
on; these guarantees were verified against msodbcsql's behavior.

#### 7.1.1. DM guarantees we rely on

- The DM ensures all child handles are freed before freeing a parent:
  all DBCs freed before `SQLFreeEnv`, all STMTs freed before `SQLFreeConnect`.
- `SQLAllocHandle(STMT)` and `SQLFreeHandle(DBC)` cannot race on the same DBC.
  The DM enforces this via the ODBC connection state machine: `SQLAllocStmt`
  requires state C4+ (connected), while `SQLFreeHandle(DBC)` requires state C2
  (disconnected). These are mutually exclusive states, so the DM rejects one
  before it ever reaches the driver. The same logic applies to ENV: `SQLFreeEnv`
  requires no outstanding DBCs, which the DM verifies first. This means the
  parent handle and its mutex are guaranteed alive during child allocation.
- The DM ensures the DBC is disconnected before calling `SQLFreeConnect` via
  call to `SQLDisconnect`, and `SQLDisconnect` automatically drops all
  statements and descriptors.

#### 7.1.2. Locking rules (mirroring msodbcsql)

- **Alloc path**: Lock the parent's mutex to register the new child in its list.
- **Free path**: Lock the parent's mutex to unregister from its child list.
- **Lock ordering**: Always lock parent before child (ENV before DBC, DBC before
  STMT) to prevent deadlocks. Always acquire the parent lock before the child lock.
- **DESC is a sibling of STMT, not a child**: a descriptor's parent is the DBC
  (`DescHandle::parent_dbc`), not the statement it happens to be associated
  with — an explicit descriptor can be reassociated across statements, or
  shared by several at once. The free path (`free_desc`, `free_handle.rs`)
  walks DBC → STMT to clear a freed descriptor's association from every
  statement that had it active, so the STMT lock and a DESC lock must never
  nest the other way: **never hold a STMT lock while acquiring a DESC lock**.
  Every entry point that both validates STMT state and writes to a
  descriptor (`SQLBindCol`, `SQLBindParameter`, `SQLFetchScroll`,
  `SQLFreeStmt(SQL_UNBIND | SQL_RESET_PARAMS)`, execute's parameter
  snapshot) follows the same two-phase shape: lock STMT, validate and
  resolve the target descriptor handle (`effective_ard`/`effective_apd`),
  drop the STMT lock, *then* lock the descriptor. A descriptor pointer
  resolved this way can be freed by a concurrent `SQLFreeHandle` before it
  is dereferenced. Existing `handles::live_type` rechecks narrow that window,
  but are not a complete lifetime guarantee. Establish the supported concurrent
  call sequence before treating [#441](https://github.com/microsoft/mssql-rs/issues/441)
  as a requirement for a broader ownership redesign.
- **APD before IPD**: `SQLBindParameter`'s `bind_param_records` is the only
  place in this crate that holds two DESC locks at once (writing a
  parameter's APD and IPD records together). It locks APD before IPD, and
  that must stay the only order used anywhere both are locked together —
  `BoundParam::all_from_descriptor_states` (used by
  `snapshot_bound_params`) only ever reads them, never locks both
  simultaneously, so it does not need to follow this rule itself.
- **`debug_assert!` for DM invariants**: The free path uses `debug_assert!` to
  verify the DM upheld its guarantees (e.g., no outstanding children). These
  fire in debug builds only — in release builds the driver trusts the DM and
  frees unconditionally, matching msodbcsql.

### 7.2. Known descriptor concurrency gap

The different admission checks in `SQLBindCol`/`SQLFreeStmt`/`SQLSetStmtAttr`
and the direct descriptor setters are tracked in
[#472](https://github.com/microsoft/mssql-rs/issues/472). Do not justify a new
buffer-use protocol by an application freeing storage that an outstanding
fetch still needs. Establish the supported concurrency and completion
guarantees first; preserve existing guards meanwhile.

### 7.3. Prepared parameter definitions

- Compare SQL definitions during IPD mutation, not APD addresses or conversion
  metadata on every execute. Preserve plans for equivalent `SQLBindParameter`
  calls and APD-only changes. `DESC_CONSISTENT` in msodbcsql controls validation,
  not plan invalidation; its `ParamInfoSnapshot`/`RE_PREPARE` path is the reference.
- Use the shared definition projection for binding, direct IPD fields/records,
  and refinement. Account for partial failed writes and parameter-count changes.
  Release descriptor locks before invalidating the owning statement.
- Keep numeric SQL declarations IPD-based without overwriting the value's wire
  precision/scale. A `SQL_NUMERIC_STRUCT` header is not the prepared declaration.
- No descriptor identity, lifetime counter, or persistent metadata snapshot is
  needed for this sequential cache-invalidation policy.
- Do not extend that sequential claim to IPD mutation overlapping synchronous
  execute. The existing snapshot/stage/restore sequence can lose invalidation;
  a pending flag only while the plan is absent does not close every window.
  Treat this as a separate concurrency gap, not application misuse or an
  assumed Driver Manager serialization guarantee. ODBC's
  [multithreading contract](https://learn.microsoft.com/sql/odbc/reference/develop-app/multithreading)
  is distinct from the Need Data rule below.
- A DAE binding snapshot is not permission to change the live definition while
  the statement is in Need Data. `SQLBindParameter` and associated descriptor
  setters are DM-enforced `HY010` errors in that state (see their
  [diagnostic contract](https://learn.microsoft.com/sql/odbc/reference/syntax/sqlbindparameter-function#diagnostics)).
  Cover valid mutations before DAE starts or after it ends, not a new deferred
  invalidation protocol for out-of-contract rebinding.

## 8. FFI boundary conventions

- Every exported function goes through `exports.rs` as a thin
  `pub extern "C"` wrapper.
- The wrapper calls a `pub(crate)` implementation function that contains
  the real logic.
- **Every FFI implementation function MUST wrap its body in the
  `crate::ffi_entry!` macro.** This is non-negotiable — it is the single
  panic boundary that converts a Rust panic into `SQL_ERROR` instead of
  unwinding across the C ABI (undefined behavior).
  Shape:

  ```rust
  pub(crate) unsafe fn sql_xxx(/* raw args */) -> SqlReturn {
      debug!(/* all args */, "SQLXxx called");
      crate::ffi_entry!("SQLXxx", unsafe { sql_xxx_impl(/* raw args */) })
  }

  // Thin unsafe shim: raw pointers -> validated references, then delegate.
  unsafe fn sql_xxx_impl(/* raw args */) -> SqlReturn {
      if handle.is_null() { return SQL_INVALID_HANDLE; }
      let h = unsafe { handle_from_raw::<XxxHandle>(handle) };
      debug_assert_eq!(h.object_type, HandleType::Xxx);
      sql_xxx_safe(h, /* scalar/decoded args */)
  }

  // Safe core: all business logic; only small unsafe out-pointer writes.
  fn sql_xxx_safe(handle: &XxxHandle, /* args */) -> SqlReturn {
      // ...
  }
  ```
- Keep the `*_impl` shim limited to validating handles, converting raw pointers
  to references, decoding input strings, and delegating. Put scalar validation,
  locking, state mutation, and value mapping in the safe core.
- Check Driver Manager-enforced preconditions with `debug_assert!`; do not turn
  them into release-build error paths. Runtime-check application inputs that the
  Driver Manager does not validate.
- The first line of every FFI implementation function must be a `debug!` log
  of every argument (pointers logged with `?` — no deref).
- Do not call `crate::init_tracing()` from the `pub extern "C"` wrapper in
  `exports.rs`. `ffi_entry!` already calls it as the first statement inside its
  `catch_unwind` (`src/lib.rs`), so the wrappers stay thin delegates; adding a
  second call would both duplicate initialization and move it outside the panic
  boundary.
- Never call `std::panic::catch_unwind` directly in this crate; always go
  through `ffi_entry!` so the panic-log message, return-code mapping, and
  trailing trace are uniform.
- Pointer parameters from C must be treated as potentially null, invalid, or
  misaligned — validate before use.
- **When an entry point answers the same request in more than one place, route
  the answer through one shared function rather than repeating the rule.**

## 9. Types and casts

- Use the explicit FFI aliases (`SqlSmallInt`, `SqlHandle`, `SqlReturn`) rather
  than raw `i16` / `*mut c_void` in business logic. Internal functions that
  return an ODBC status use `SqlReturn` to keep intent clear.
- Avoid `as` casts for numeric conversions — use `TryFrom` / `TryInto` and
  handle the error. `as` silently truncates.
- Pointer casts between handle types must go through the well-defined
  conversion functions in `crate::handles`: `handle_to_raw`,
  `handle_from_raw`, `handle_from_raw_mut`, `free_handle`.

## 10. Testing

- Unit tests for pure logic go in `#[cfg(test)]` modules inside the source file.
- Cover the exported entry point as the application calls it; an inner-function
  test alone does not prove that production traffic reaches the tested branch.
- Allocate ODBC handles in unit tests **only** through
  `crate::test_support::TestHandles`:
  - Use `with_env()`, `with_env_dbc()`, `with_env_dbc_stmt()`, or
    `alloc_extra_stmt()` to get the handle chain you need; access via
    `.env` / `.dbc` / `.stmt`.
  - Never free handles manually — `TestHandles::Drop` frees them
    child-before-parent (the order `SQLFreeHandle` requires). Manual
    `sql_free_handle` calls risk double-frees.
  - If you need a handle shape the constructors don't cover, extend
    `TestHandles` rather than open-coding allocation in the test.
- End-to-end tests that exercise the loadable `.so`/`.dll` through a real
  Driver Manager live in `tests/e2e/` as a CMake-built C++ suite (run via
  `tests/e2e/run_e2e.sh` / `.ps1`).
- Tag any live e2e test that can only assert the observable *outcome* (a value
  round-trips, the connection stays healthy) and cannot see the underlying TDS
  RPC sequence with a `Benefits-from-mock-tds:` comment above the `TEST_F`,
  noting what a byte-level mock TDS server would let it assert (e.g. that an
  `sp_unprepare` / `sp_prepexec` `@handle` drop actually fired). Pin the exact
  behavior with a Rust unit test meanwhile; `grep -rn Benefits-from-mock-tds`
  surfaces every such test to tighten once mock-TDS support lands.
- If an e2e test asserts mssql-odbc-specific behavior the full msodbcsql driver
  does not share (e.g. a Phase-1 "not implemented" response), start it with the
  `SKIP_IF_COMPARING_MSODBCSQL()` macro so it self-skips on the msodbcsql leg of
  a `--compare-with-msodbcsql` run instead of failing the parity binary. That is
  the first of the three reasons the macro is admissible; see §2.1 for the other
  two and for the preference against skipping when the test exists solely to pin
  one registered divergence.
- Every new `SQLXxx` function must have at least:
  - A success-path test.
  - A null-output-handle test.
  - An invalid-handle-type or invalid-input test.
- PLP routing is gated by `is_plp()` alone, not size, so an e2e targeting
  `deliver_bound_plp` does not need a large payload. Do not confuse this with
  `PLP_TYPED_MATERIALIZE_LIMIT`, the separate 1 MiB cap on how much a typed
  conversion will materialize (see deviation 7 in the
  [parity decision registry](../../mssql-odbc/docs/parity-deviations.md)).
- Use `cargo nextest` (via `cargo btest`), not `cargo test`.
