---
name: verify-odbc-changes
description: Validate mssql-odbc feature implementations against safety, guidelines, and parity standards
applyTo: "mssql-odbc/**"
---

# Verify mssql-odbc Changes

## Checklist

### 1. Project Context
- [ ] Read [mssql-odbc/README.md](README.md) to understand architecture, supported features, and build/run instructions

### 2. Guidelines Compliance
- [ ] Review changes against [.github/instructions/mssql-odbc.instructions.md](../../.github/instructions/mssql-odbc.instructions.md)

### 3. msodbcsql Parity Check
- [ ] Use the MCP server #ec-mini-odbc (check .vscode/mcp.json) to search msodbcsql source for equivalent patterns
  - Look for comparable error handling, state transitions, or FFI patterns in the reference driver
  - Document any deviations and rationale

### 4. Code Quality
- [ ] Run `cargo fmt` to format changes:
  ```bash
  cargo fmt --all
  ```
- [ ] Run `cargo bclippy` to lint (warnings are errors):
  ```bash
  cargo bclippy
  ```
- [ ] Fix all warnings before proceeding

### 5. Testing
- [ ] Run mssql-odbc unit tests:
  ```bash
  cargo nextest run -p mssql-odbc
  ```
- [ ] Run mssql-odbc end-to-end tests:
  ```bash
  cd mssql-odbc/tests/e2e
  ./run_e2e.sh
  ```
  e2e tests need a running SQL Server instance. You can run `./dev/dev-launchsql.sh` to launch a local SQL Server container for testing.
- [ ] Verify all tests pass with `exit 0`
- [ ] If adding new features, include corresponding test cases
