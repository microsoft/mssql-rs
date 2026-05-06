# mssql-odbc — Rust Guidelines

Rules for writing safe, panic-free Rust in the ODBC driver. This crate produces
a C shared library loaded via `dlopen` — panics, undefined behavior, and memory
errors are fatal and unrecoverable in that context.

## No panics

The ODBC driver runs inside arbitrary host processes. A panic that unwinds
across the FFI boundary is **undefined behavior**.

- **Never** use `.unwrap()` or `.expect()` on `Result` or `Option`.
- Use `.unwrap_or()`, `.unwrap_or_else()`, `.unwrap_or_default()`, or
  pattern matching instead.
- For `Mutex::lock()`, use `.lock().unwrap_or_else(|e| e.into_inner())` (poison
  recovery) or redesign to avoid the mutex.
- Every `pub extern "C"` entry point must be wrapped in
  `std::panic::catch_unwind` as a last-resort safety net — but do **not** rely
  on it; prevent panics at the source.
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
- Store diagnostic info (SQLSTATE, message) on the handle so
  `SQLGetDiagRec` can report it — don't discard error details.

## Unsafe code

- Minimize `unsafe` blocks — keep them as small as possible and comment
  the safety invariant they rely on.
- All raw-pointer writes must be guarded by a null check first.
- Never dereference a pointer received from C without validating it.
- Use `#[unsafe(no_mangle)]` only in `exports.rs` — keep implementations
  in separate modules as `pub(crate)` safe functions.

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
- Prefer `parking_lot` mutexes if added to deps (no poisoning), or handle
  poison explicitly with standard `Mutex`.

## FFI boundary conventions

- Every exported function goes through `exports.rs` as a thin
  `pub extern "C"` wrapper.
- The wrapper calls a `pub(crate)` implementation function that contains the
  real logic inside `catch_unwind`.
- `api/odbc_types.rs` declares all ODBC constants, type aliases, and
  non-function declarations (the public API contract with C).
- `api/exports.rs` declares all exported `extern "C"` functions (the driver's
  symbol table).
- Use `SqlReturn` (not raw `i16`) as the return type of internal functions
  to keep intent clear.
- Pointer parameters from C must be treated as potentially null, invalid, or
  misaligned — validate before use.

## Types and casts

- Use explicit types for FFI: `SqlSmallInt`, `SqlHandle`, `SqlReturn` — never
  raw `i16` / `*mut c_void` in business logic.
- Avoid `as` casts for numeric conversions — use `TryFrom` / `TryInto` and
  handle the error. `as` silently truncates.
- Pointer casts between handle types must go through well-defined conversion
  functions (e.g., `handle_to_raw`, `raw_to_handle`).

## Testing

- Unit tests for pure logic go in `#[cfg(test)]` modules inside the source file.
- Integration tests that exercise the exported C API go in `tests/`.
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
