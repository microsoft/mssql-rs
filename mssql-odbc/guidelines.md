# mssql-odbc — Rust Guidelines

Rules for writing safe, panic-free Rust in the ODBC driver. This crate produces
a C shared library loaded via `dlopen` into arbitrary host processes — panics
unwinding across the FFI boundary, undefined behavior, and memory errors are all
fatal and unrecoverable.

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
  can report it — don't discard error details. Two posters, choose by source:
  - `post_sql_error(state, sqlstate, native, message)` — for errors the
    driver itself raises (invalid arg, sequence error, truncation, etc.).
    Posts exactly one record.
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
  entry by calling `free_errors(...)` after acquiring the handle lock —
  mirrors msodbcsql's `FreeErrors`.

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

## Memory management

- Heap allocations handed to C (via `Box::into_raw` / `handle_to_raw`) must
  have a corresponding deallocation path (e.g., `SQLFreeHandle`).
- Never use `std::mem::forget` to skip destructors unless the ownership has
  been explicitly transferred to C.
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
  STMT) to prevent deadlocks. This mirrors msodbcsql's documented rule:
  *"get parent lock before child lock"* (`csEnv` → `csDbc`).
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
  `crate::test_support::TestHandles` — never hand-roll the
  `sql_alloc_handle` / `sql_set_env_attr` / `sql_free_handle` sequence in a
  test module:
  - Use `TestHandles::with_env()`, `with_env_dbc()`, or `with_env_dbc_stmt()`
    to allocate the handle chain you need; access the handles via the
    `.env` / `.dbc` / `.stmt` fields.
  - Use `handles.alloc_extra_stmt()` when a test needs a second statement on
    the same connection.
  - Never free the handles manually — `TestHandles` frees them
    child-before-parent on `Drop`, which is also the order `SQLFreeHandle`
    requires. Adding manual `sql_free_handle` calls risks double-frees.
  - If you need a handle shape the constructors don't cover, extend
    `TestHandles` rather than open-coding allocation in the test.
- End-to-end tests that exercise the loadable `.so`/`.dll` through a real
  Driver Manager live in `tests/e2e/` as a CMake-built C++ suite (run via
  `tests/e2e/run_e2e.sh` / `.ps1`). There is no Rust integration test
  directory — the loaded-driver entry points are best exercised in C++.
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
