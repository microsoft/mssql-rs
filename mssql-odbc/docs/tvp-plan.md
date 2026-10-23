# Table-Valued Parameter (TVP) Support in mssql-odbc

**Status:** Proposed · **Owner:** mssql-odbc (Rust ODBC driver) · **Consumer:** mssql-python
**Work item:** AB#49353 (TVP support in mssql-odbc)
**Related:** AB#47790 (mssql-odbc parameter matrix) · AB#48177 (mssql-python TVP feature)

---

## 1. Purpose

This is a high-level design for adding SQL Server **table-valued parameter (TVP)** support to
the `mssql-odbc` Rust driver, so that mssql-python can offer TVPs through its synchronous
(ODBC) cursor against **both** ODBC providers it ships.

It is written for two audiences: the **mssql-python team**, to state what the driver will
expose and guarantee; and the **mssql-odbc team**, to record the scope of the work.

The design goal is **behavioral parity with msodbcsql18 for the supported TVP flow at the
ODBC surface**. mssql-python drives TVPs through standard ODBC calls, and those calls must
produce identical observable results whether the active provider is msodbcsql18 or
mssql-odbc — with no provider-specific code path. The unsupported cases are listed in
[Section 6](#6-out-of-scope).

---

## 2. Context: two adapters behind one mssql-python contract

mssql-python exposes a single public `TableValuedParameter` value. Behind it, the driver
selects a backend adapter:

| Adapter | Transport | Owner | This document |
| --- | --- | --- | --- |
| Synchronous cursor → ODBC | ODBC → **msodbcsql18** | mssql-python | Not in scope (native TVP support) |
| Synchronous cursor → ODBC | ODBC → **mssql-odbc (Rust)** | **mssql-odbc team** | **In scope** |
| Asynchronous cursor → Rust/TDS | Rust/TDS | Separate owner | Not in scope |

The ODBC adapter is **driver-agnostic**: mssql-python issues the same ODBC sequence
regardless of which driver is loaded. msodbcsql18 supports the SQL Server ODBC TVP contract
natively. This document covers bringing the Rust `mssql-odbc` driver to the same contract, so
the ODBC adapter runs unchanged on it. The asynchronous / Rust-TDS adapter is delivered
separately and is out of scope here.

### 2.1 ODBC call sequence (conceptual)

| # | Flow | Operation |
| --- | --- | --- |
| 1 | App → Driver | Prepare the statement |
| 2 | App → Driver | Describe the TVP parameter |
| 3 | Driver → SQL Server | Request parameter metadata |
| 4 | SQL Server → Driver | Returns `SQL_SS_TABLE` + the table type's three-part name |
| 5 | App → Driver | Read the table type identity |
| 6 | App → Driver | Bind the table parameter (`SQL_SS_TABLE`) with its type name and row count |
| 7 | App → Driver | Set parameter focus to the TVP |
| 8 | App → Driver | Bind each TVP column's values |
| 9 | App → Driver | Restore parameter focus to 0 |
| 10 | App → Driver | Execute |
| 11 | Driver | Assemble the rows and send the TVP |
| 12 | Driver → SQL Server | Send the statement carrying the TVP |
| 13 | SQL Server → Driver | Procedure result set(s) |
| 14 | App → Driver | Fetch the procedure results |

---

## 3. Current state in mssql-odbc

TVP support is **not available today**. The driver recognizes the table type at describe
time, but binding a table parameter and setting parameter focus are rejected, so a TVP
cannot be sent. This work makes the ODBC TVP contract functional end to end.

---

## 4. The ODBC contract mssql-odbc will support

At a high level, mssql-python performs the following, identical to msodbcsql18:

- **Describe** — `SQLDescribeParam` reports `SQL_SS_TABLE` and exposes the table type's
  three-part (catalog / schema / type) name.
- **Bind the TVP** — the parameter is bound as `SQL_SS_TABLE`, input-only, carrying the type
  name and the number of rows (or an empty-table marker).
- **Bind the columns under focus** — parameter focus is set to the TVP, each column's values
  are bound, and focus is then restored. `SQL_ATTR_PARAMSET_SIZE`,
  `SQL_ATTR_PARAM_BIND_TYPE`, and `SQL_ATTR_PARAM_BIND_OFFSET_PTR` under focus belong to
  that TVP's independent column layout, not the statement's outer parameter array.
- **Supply TVP content with data-at-execution** — the driver returns the table token, then
  row-control and cell tokens in the order expected by the ODBC TVP protocol. Cell bytes are
  buffered and converted into the table value before execution. Only columns marked
  data-at-execution produce cell tokens; ordinary bound values and NULLs are captured from
  their positioned row buffers when the application supplies that row. A false first
  row-control marker supplies an empty table. A row's DAE length is read when its cell
  token is offered, so mixed ordinary/DAE rows retain their declared lengths without
  reading unused future rows.
- **Execute** — the statement runs; procedure result sets are returned through the normal fetch
  path. TVPs are input-only and are never fetched back.

---

## 5. Scope of work

mssql-odbc will add the following capabilities:

- **Expose the table type identity** from describe — report `SQL_SS_TABLE` and the table
  type's three-part name, extending the driver's existing SQL Server user-type-name handling
  to table types.
- **Support the SQL Server TVP type identity fields** — catalog, schema, and type name.
- **Accept a table parameter** bound as `SQL_SS_TABLE`, validated as input-only and
  non-nullable, with msodbcsql18-compatible diagnostics.
- **Implement parameter focus** so a TVP's constituent columns can be bound and then
  restored to top-level binding.
- **Assemble the bound columns into rows** for transmission, applying the same per-cell
  conversion and null semantics as top-level scalar parameters. Numeric bindings use the
  scalar application-descriptor defaults; decimal values are rescaled to the fixed TVP
  column metadata, rounding half away from zero when reducing scale and rejecting
  precision overflow before transmission.
- **Support data-at-execution for TVP content** — consume row-control markers and per-cell
  payloads, preserving row order and NULL values while assembling the table parameter.
- **Handle empty TVPs and all-NULL columns** correctly.
- **Preserve deferred descriptor fields** — row counts/default markers use the top-level
  octet-length pointer independently of its NULL indicator. An omitted schema remains
  unqualified in the SQL declaration and resolves against the caller's default schema.
- **Preserve binding lifetime and diagnostics** — retain TVP column bindings across cursor
  close and re-prepare, as with scalar parameter bindings; clear them on
  `SQLFreeStmt(SQL_RESET_PARAMS)`. Parameter focus remains a statement attribute, so the
  application must restore it to 0 before `SQLExecute` or `SQLExecDirectW`. Rebinding a
  different table type or changing its descriptor identity invalidates the prepared
  declaration. On a bad value, report the offending row and column and keep the connection
  usable.

---

## 6. Out of scope

- **msodbcsql18 TVP support** — native to that driver; owned by mssql-python.
- **Asynchronous / Rust-TDS adapter** — delivered separately.
- **Output or input/output TVPs; fetching TVPs back** — TVPs are input-only by definition.
- **TVPs inside parameter arrays / `executemany`** — a TVP is one logical value, not a batch.
- **The per-column default-value marker** (`SQL_CA_SS_COLUMN_HAS_DEFAULT_VALUE`) — applying a
  server default to every row of a TVP column is deferred.
- **`sql_variant` columns within a TVP** — deferred.

---

## 7. Validation

TVP support is validated at the ODBC layer with driver-owned tests and confirmed end to end
against the mssql-python conformance suite:

- Describe returns the table type with its three-part name.
- Bind validation matches msodbcsql18 (input-only, non-nullable, correct diagnostics).
- Execute with mixed column types, embedded and all-NULL values, empty tables,
  schema-qualified names, large values, and multiple TVPs interleaved with scalar parameters.
- The cursor stays usable after a bad TVP value, and the error identifies the row and column.

**Parity gate:** mssql-python's TVP conformance suite (AB#48177) passes against the
mssql-odbc provider with the same expectations it uses for msodbcsql18 — no
provider-specific skips.

---

## 8. Dependencies & open items

- **Work-item tracking** — this work is tracked under AB#49353.

---

## 9. Summary for the mssql-python team

- mssql-odbc implements the supported SQL Server ODBC TVP flow: describe reports the table
  type and its name, the table parameter and its columns bind under parameter focus, and the
  statement executes with the TVP.
- Your ODBC TVP code path can run unchanged for the supported cases listed in this document.
  The exclusions in Section 6 remain unsupported.
- The asynchronous / Rust-TDS path is owned separately and is not affected by this work.
