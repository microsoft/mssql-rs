---
applyTo: "mssql-odbc/**"
---

# mssql-odbc — Rust Guidelines

Rules for writing safe, panic-free Rust in the ODBC driver. This crate produces
a C shared library loaded via `dlopen` into arbitrary host processes — panics
unwinding across the FFI boundary, undefined behavior, and memory errors are all
fatal and unrecoverable.

## Project context

Before making changes, also read [mssql-odbc/README.md](../../mssql-odbc/README.md)
for architecture, supported features, and build/run instructions.

## Parity reference: the classic C++ msodbcsql driver

The classic C++ **msodbcsql** driver (Microsoft ODBC Driver for SQL Server) is the
authoritative parity reference for this crate. Its source lives in the
`SqlClientDrivers` Azure DevOps org (`msodbcsql` project/repo, `master`).

- Before adding, changing, or **rejecting** any behavior for parity reasons — auth
  keywords, connection-string attributes, error/SQLSTATE mapping, Driver Manager
  interaction — verify it against the actual msodbcsql source. Do **not** rely on
  MS Learn docs or sibling drivers (JDBC/.NET/go-sqlcmd), which frequently differ
  from what the C++ driver actually does.
- When reporting a parity finding, cite the msodbcsql source (file + what it does),
  and state explicitly whether the decision **matches**, **exceeds**, or **diverges
  from** msodbcsql so the trade-off is visible.
- Deliberate deviations (exceed-parity) are allowed with product-owner sign-off;
  record the rationale in code comments and the tracking work item.
- Deliberate deviations are listed below:
  - `ActiveDirectoryManagedIdentity` is accepted as an alias for managed-identity
    authentication. msodbcsql recognizes only `ActiveDirectoryMSI`
    (`Sql/Ntdbms/sqlncli/msdart/inc/dlgattr.h` → `OPTIONADMSI L"ActiveDirectoryMSI"`);
    `ActiveDirectoryManagedIdentity` does not appear anywhere in the msodbcsql source.
    Added to match MS Learn and the sibling drivers (JDBC/.NET/go-sqlcmd). Tracked in AB#46066.

## No panics

- **Never** use `.unwrap()` or `.expect()` on `Result` or `Option` in
  non-test code. Tests under `#[cfg(test)]` may use them since panics
  there are caught by the test harness.
- Use `.unwrap_or()`, `.unwrap_or_else()`, `.unwrap_or_default()`, or
  pattern matching instead.
- For `Mutex::lock()`, return `SQL_ERROR` on poison — use `let Ok(state) = handle.inner.lock() else { return SQL_ERROR; }`. Do **not** recover via `e.into_inner()`.
- Every FFI entry point must be wrapped in the `crate::ffi_entry!` macro
  (see [FFI boundary conventions](#ffi-boundary-conventions)). The macro is
  a last-resort safety net — write code that cannot panic in the first place.
- Never use `unreachable!()`, `todo!()`, or `unimplemented!()` in non-test code.
  Use explicit error returns instead.
- Array/slice access: prefer `.get()` over indexing (`[]`), which panics on
  out-of-bounds.

## Error handling

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
    `SERVER_ERROR_TO_SQL_STATE_MAP`; for other variants it posts a single
    record using `default_sqlstate`. Pick `08001` for connect-time
    failures and `HY000` for execution/fetch failures.
  Never hand-roll `post_sql_error` over a `TdsError` — you lose the
  per-server-error fan-out and the SQLSTATE mapping.
- Every ODBC entry point must clear the handle's diagnostic records at API
  entry by calling `free_errors(...)` after acquiring the handle lock, so a
  fresh call starts without stale diagnostics.

## Unsafe code

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
- Use `#[unsafe(no_mangle)]` only in `exports.rs` — keep implementations
  in separate modules as `pub(crate)` safe functions.

### Safe-core / unsafe-shell split

Push `unsafe` to the edges. Each FFI implementation is split into two layers:

- A thin `unsafe fn sql_xxx_impl(...)` **shim** whose only job is to turn raw
  C pointers into validated Rust references:
  1. Null-check the handle → `SQL_INVALID_HANDLE`.
  2. `unsafe { handle_from_raw::<T>(handle) }` to obtain `&T`.
  3. `debug_assert_eq!(h.object_type, HandleType::X)` to catch DM contract
     violations in debug builds.
  4. Decode any input strings (`read_utf16`, etc.).
  5. Delegate everything else to the safe core.
- A safe `fn sql_xxx_safe(handle: &T, ...) -> SqlReturn` **core** that holds all
  business logic: locking, state mutation, value mapping. It receives validated
  references (never raw handle pointers) and only opens small, locally-justified
  `unsafe { write_if_some(...) }` / `unsafe { copy_with_nul(...) }` blocks to
  write to caller out-pointers.

Rules of thumb:

- A function should be a **safe `fn`** (even if it contains `unsafe {}` blocks)
  whenever it can discharge the safety obligation from its own arguments and
  invariants — e.g. an accessor like `StmtHandle::parent_dbc(&self) -> &DbcHandle`,
  where `&self` already guarantees a valid handle.
- A function should be an **`unsafe fn`** only when it relies on an
  unverifiable caller promise — e.g. the validity of a raw pointer passed
  across the FFI boundary (the `*_impl` shims).
- Validation that only inspects scalar arguments (e.g.
  `debug_assert!(buffer_length >= 0, ...)`) belongs in the safe core, not the
  shim. The shim should be limited to null-checks and pointer→reference
  conversion.
- Preconditions the DM is contractually required to enforce (non-null required
  pointers, valid length/option values, correct handle type) are checked with
  `debug_assert!` only — **do not** promote them to a release-build
  `if`/error-return. The assert documents the DM contract and catches
  violations in debug builds; in release the driver trusts the DM, matching
  msodbcsql (which asserts rather than re-validates). Asserts worded
  *"... — DM should have rejected this"* are intentionally debug-only; leave
  them as `debug_assert!`. Only values the DM does **not** validate (genuine
  application inputs) get a runtime check.

## Memory management

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

## Concurrency

- The ODBC spec allows Driver Manager to call functions on the same handle
  from different threads. Protect mutable state with `Mutex` or `RwLock`.
- Keep lock scopes narrow — lock, copy/update, unlock. Never hold a lock
  across an FFI call or I/O operation.
- Handle poison explicitly with `std::sync::Mutex` — see the no-panics
  rule above for the canonical `let Ok(state) = ... else { return SQL_ERROR; }`
  pattern.

### Cross-handle thread safety (alloc / free)

ODBC handles form a parent–child hierarchy (ENV → DBC → STMT → DESC). The
Driver Manager (DM) provides serialization guarantees that the driver relies on
- verified against msodbcsql's behavior:

#### DM guarantees we rely on

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

#### Locking rules (mirroring msodbcsql)

- **Alloc path**: Lock the parent's mutex to register the new child in its list.
- **Free path**: Lock the parent's mutex to unregister from its child list.
- **Lock ordering**: Always lock parent before child (ENV before DBC, DBC before
  STMT) to prevent deadlocks. Always acquire the parent lock before the child lock.
- **`debug_assert!` for DM invariants**: The free path uses `debug_assert!` to
  verify the DM upheld its guarantees (e.g., no outstanding children). These
  fire in debug builds only — in release builds the driver trusts the DM and
  frees unconditionally, matching msodbcsql.

## FFI boundary conventions

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

  See [Safe-core / unsafe-shell split](#safe-core--unsafe-shell-split) for the
  full rationale.

- The first line of every FFI implementation function must be a `debug!` log
  of every argument (pointers logged with `?` — no deref).
- The `pub extern "C"` wrapper in `exports.rs` must call
  `crate::init_tracing()` before delegating to the impl — `ffi_entry!` does
  not initialize tracing itself.
- Never call `std::panic::catch_unwind` directly in this crate; always go
  through `ffi_entry!` so the panic-log message, return-code mapping, and
  trailing trace are uniform.
- Use `SqlReturn` (not raw `i16`) as the return type of internal functions
  to keep intent clear.
- Pointer parameters from C must be treated as potentially null, invalid, or
  misaligned — validate before use.

## Types and casts

- Use explicit types for FFI: `SqlSmallInt`, `SqlHandle`, `SqlReturn` — never
  raw `i16` / `*mut c_void` in business logic.
- Avoid `as` casts for numeric conversions — use `TryFrom` / `TryInto` and
  handle the error. `as` silently truncates.
- Pointer casts between handle types must go through the well-defined
  conversion functions in `crate::handles`: `handle_to_raw`,
  `handle_from_raw`, `handle_from_raw_mut`, `free_handle`.

## Testing

- Unit tests for pure logic go in `#[cfg(test)]` modules inside the source file.
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
- Every new `SQLXxx` function must have at least:
  - A success-path test.
  - A null-output-handle test.
  - An invalid-handle-type or invalid-input test.
- Use `cargo nextest` (via `cargo btest`), not `cargo test`.

## Code style

- Follow the conventions in the repo-level
  [copilot-instructions.md](../.github/copilot-instructions.md).
- Every `.rs` file starts with the copyright header.
- Prefer `pub(crate)` over `pub` for internal APIs.
- No AI-slop comments — don't restate what the code already says.
