# mssql-odbc

Rust implementation of the Microsoft ODBC Driver 18 for SQL Server (`msodbcsql18`),
built on top of [mssql-tds](../mssql-tds).

## What it does

Produces a shared library (`libmsodbcsql18.so` / `libmsodbcsql.18.dylib` / `msodbcsql18.dll`) that implements
the ODBC C API. The ODBC Driver Manager (`unixODBC` on Linux/macOS, `odbc32` on Windows)
loads it via `dlopen` — applications use standard ODBC calls without knowing the driver
is written in Rust.

## Build

```bash
cargo build              # debug build
cargo build --release    # release build
```

Output location: `target/{debug,release}/` with a platform-specific filename:

| Platform | Output file |
|---|---|
| Linux | `libmsodbcsql18.so` |
| macOS | `libmsodbcsql18.dylib` |
| Windows | `msodbcsql18.dll` |

The `build.rs` script embeds platform-specific metadata:
- **Linux:** `soname` → `libmsodbcsql-18.4.so.1.1`
- **macOS:** `install_name` → `libmsodbcsql.18.dylib`
- **Windows:** no extra linker args needed

## Testing

### Rust unit tests

```bash
cargo test -p mssql-odbc
```

### C++ e2e tests (Google Test)

End-to-end tests that exercise the driver through the ODBC Driver Manager,
matching the msodbcsql gtest infrastructure. See [tests/e2e/README.md](tests/e2e/README.md).

```bash
cd tests/e2e
./run_e2e.sh               # builds driver + registers + cmake + ctest
```

Or run test binaries directly (self-registers the driver automatically):

```bash
./build/smoke_test
./build/alloc_env_test
```

## Architecture

```
Application
    ↓ ODBC C API (SQLAllocHandle, SQLDriverConnect, ...)
Driver Manager (unixODBC / odbc32)
    ↓ dlopen / LoadLibrary
libmsodbcsql18.so (this crate)
    ↓
mssql-tds (TDS protocol)
    ↓
SQL Server
```

Each ODBC entry point is a `#[unsafe(no_mangle)] pub unsafe extern "C"` function
that the Driver Manager resolves by symbol name.
