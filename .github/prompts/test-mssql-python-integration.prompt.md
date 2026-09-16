```prompt
---
mode: agent
description: "Run mssql-python driver integration tests with mssql-py-core"
---

# Test mssql-python Integration

This prompt helps run the mssql-python driver integration tests that validate mssql-py-core functionality through the Python driver layer.

## Prerequisites

Before running tests, verify these are in place:

### 1. mssql-python Repository
The `mssql-python` repo must be checked out as a sibling directory to `mssql-tds`:

```bash
ls -d ../mssql-python 2>/dev/null && echo "✅ mssql-python found" || echo "❌ Clone mssql-python as sibling"
```

If missing:
```bash
cd .. && git clone https://github.com/microsoft/mssql-python.git
```

### 2. SQL Server Running
A local SQL Server must be running on port 1433:

```bash
docker ps | grep -q "1433" && echo "✅ SQL Server running" || echo "❌ Start SQL Server"
```

If not running, start it:
```bash
./dev/dev-launchsql.sh
```

Or manually:
```bash
docker run -e 'ACCEPT_EULA=Y' -e "SA_PASSWORD=$(cat /tmp/password)" -p 1433:1433 -d mcr.microsoft.com/mssql/server:2022-latest
```

### 3. Password Configuration
Tests read the SA password from `/tmp/password`:

```bash
cat /tmp/password 2>/dev/null || echo "❌ Create /tmp/password with SA password"
```

Set it to match your SQL Server:
```bash
echo "<your-sa-password>" > /tmp/password
```

### 4. Python 3.9+
```bash
python3 --version
```

---

## Running Tests

### Full Integration Test Suite

Run the script with `--mssql-python` flag:

```bash
./dev/test-python.sh --mssql-python
```

This script will:
1. Create/activate `.venv-pycore` virtual environment
2. Build `mssql-py-core` with maturin
3. Build `mssql-mock-tds-py` 
4. Build `ddbc_bindings` (C++ pybind11 extension in mssql-python)
5. Build `mssql-py-core` wheel and extract `.so` to mssql-python
6. Install `mssql-python` in editable mode
7. Run `mssql-py-core/tests/mssql_python/` test suite

### Run Specific Test File

After the initial build, you can run specific tests directly:

```bash
source .venv-pycore/bin/activate
pytest mssql-py-core/tests/mssql_python/test_bulkcopy.py -v
```

### Test Files

| File | Purpose |
|------|---------|
| `test_bulkcopy.py` | Basic bulkcopy operations |
| `test_bulkcopy_batch_processing.py` | Batch sizes, multi-batch scenarios |
| `test_bulkcopy_column_mappings.py` | Column mapping formats |
| `test_bulkcopy_unicode_special_chars.py` | Unicode, emoji, special characters |

---

## Troubleshooting

### ❌ Tests hang on `cursor.execute()` after `CREATE TABLE`

**Cause:** ODBC connection (autocommit=OFF) holds schema lock, blocking mssql_py_core's separate connection.

**Fix:** Ensure `conftest.py` uses `autocommit=True`:
```python
@pytest.fixture
def connection():
    conn = connect(get_connection_string(), autocommit=True)
    yield conn
    conn.close()
```

### ❌ "Timeout Error" on bulkcopy

**Cause:** Connection string missing explicit port, or SQL Server not accessible.

**Fix:** 
```bash
# Verify SQL Server responds:
docker exec sqlserver /opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "$(cat /tmp/password)" -C -Q "SELECT 1"
```

If using `localhost` without port, ensure the server is configured for TCP on 1433.

### ❌ Blocked sessions from previous test runs

**Cause:** Previous test didn't clean up transaction.

**Fix:**
```sql
-- Check for blocking:
SELECT session_id, blocking_session_id, wait_time/1000 as wait_sec 
FROM sys.dm_exec_requests WHERE blocking_session_id > 0;

-- Kill blocking session:
KILL <session_id>;
```

Or restart SQL Server container:
```bash
docker restart sqlserver
```

### ❌ "ddbc_bindings" import error

**Cause:** C++ extension not built for current Python version.

**Fix:** Re-run the full script which rebuilds ddbc_bindings:
```bash
./dev/test-python.sh --mssql-python
```

Or manually rebuild:
```bash
cd ../mssql-python/mssql_python/pybind && ./build.sh
```

### ❌ "mssql_py_core" module not found

**Cause:** Rust module not built or `.so` not copied to mssql-python.

**Fix:** Re-run the full script, or manually:
```bash
cd mssql-py-core && maturin build --release -o dist/
unzip -jo dist/*.whl "mssql_py_core/*.so" -d ../mssql-python/mssql_python/
```

### ❌ Password mismatch

**Cause:** `/tmp/password` doesn't match SQL Server SA password.

**Fix:**
```bash
# Check what password SQL Server is using (from docker):
docker logs sqlserver 2>&1 | grep -i password

# Update /tmp/password to match:
echo "<your-sa-password>" > /tmp/password
```

---

## When Tests Require mssql-python Changes

If your mssql-tds changes require corresponding changes in `mssql-python` (e.g., API signature changes, new parameters):

### 1. Create branch and PR in mssql-python
```bash
cd ../mssql-python
git checkout -b <your-branch-name>
# Make changes...
git add . && git commit -m "FEAT: <description>"
git push origin <your-branch-name>
# Create PR in GitHub
```

### 2. Validate the mssql-python branch locally

Check out the desired branch in the sibling `mssql-python` repository, then run
`./dev/test-python.sh --mssql-python` from this repository.

The Linux `Build mssql-python (cross-repo)` job has been removed: the Rust runtime
is published separately as `mssql-python-rs`, so this repository no longer builds
or publishes a combined mssql-python wheel through that job. The macOS cross-repo
tests and the mssql-odbc driver-swap compatibility job remain enabled.

### 3. Merge order

1. **First:** Merge mssql-python PR (so `main` has the changes)
2. **Then:** Merge mssql-tds PR

---

## Quick Reference

```bash
# Full integration test run:
./dev/test-python.sh --mssql-python

# Just mssql-py-core tests (no mssql-python driver):
./dev/test-python.sh

# Skip integration tests:
./dev/test-python.sh --skip-integration

# Manual test run (after initial build):
source .venv-pycore/bin/activate
pytest mssql-py-core/tests/mssql_python/ -v

# Rebuild mssql-py-core only:
source .venv-pycore/bin/activate
cd mssql-py-core && maturin develop
```
```
