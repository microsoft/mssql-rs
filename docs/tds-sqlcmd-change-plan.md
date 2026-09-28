# Rust sqlcmd: change plan and review baseline

Review baseline for the two pull requests that add a Rust `sqlcmd`. Review the
code against this list: it states what is intended to change, so anything in the
diff that is not here is worth questioning.

Work item:
[AB#48820 — update mssql-tds to support the new mssql-tds-cli features](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/48820)

| | PR | Branch | Target | Size |
|---|---|---|---|---|
| 1 | [#617](https://github.com/microsoft/mssql-rs/pull/617) — protocol | `dev/shiwanigupta/tds-batch-errors-and-login-name` | `main` | 8 files, +829 / −17 |
| 2 | [#622](https://github.com/microsoft/mssql-rs/pull/622) — the CLI | `dev/shiwanigupta/tds-cli-sqlcmd` | #617's branch | 47 files, +12296 / −261 |

#622 is stacked on #617 and retargets to `main` once #617 merges. The split is
deliberate: #617 is protocol surface that outlives the CLI and deserves review
on its own terms, while #622 is a self-contained binary that touches **zero**
`mssql-tds` files.

[#584](https://github.com/microsoft/mssql-rs/pull/584) is the old combined PR,
superseded by this split, and should be closed.

---

## Part 1 — `mssql-tds` protocol changes (#617)

Four independent changes. Each exists because `sqlcmd` cannot be built without
it; none is CLI-specific in its API. All are additive and opt-in.

### 1.1 Per-statement row counts

`sqlcmd` prints `(N rows affected)` once per statement. The client aggregated
DONE row counts per command type, so the per-statement sequence was lost.

```rust
pub fn take_done_row_counts(&mut self) -> Vec<Option<u64>>
```

One entry per DONE, in order. `None` means the server reported no count at all —
what `SET NOCOUNT ON` produces — which is distinct from `Some(0)`, a statement
that ran and affected no rows. Draining is deliberate: the log is per batch.

**Scope.** Covers the DONE tokens seen while iterating results
(`advance_to_rows` / `next_row`). Bulk copy returns its total directly, prepared
RPC batches carry per-row counts in `PreparedBatchResult`, and transaction
control has no statement counts — recording those here would double-report.

**`SQLSELECT` suppression.** SQL Server compiles variable assignment
(`SET @x = 1`, `SELECT @x = col FROM t`) as a `SQLSELECT` command and still sets
`DONE_COUNT`. Neither msodbcsql nor .NET SqlClient surfaces those counts. A real
row-returning `SELECT` carries the same tag and *does* need its count, so the
tag alone cannot separate them; what differs is whether the statement produced a
result set. Row-returning statements keep their count, assignment statements
record `None` — matching msodbcsql's `RSTypeOld != RS_SELECTION && Info != SQLSELECT`.

### 1.2 Deferred batch errors

The first ERROR token ends the batch today, so
`SELECT 1; RAISERROR('boom',16,1); SELECT 2` loses the second result set.

```rust
pub fn set_defer_batch_errors(&mut self, defer: bool)
pub fn take_pending_errors(&mut self) -> Vec<SqlErrorInfo>
```

Opt-in; off by default, so existing callers are unaffected.

**Contract worth a reviewer's attention.** While deferral is on, *every* error is
collected rather than returned, including the one that ends the batch: iteration
reports end-of-results, not `Err`, so an empty `take_pending_errors()` is the
only "no error" signal. Splitting terminal from mid-batch errors would route the
same condition down two paths depending on where it happened, which is the
ambiguity this mode exists to remove. **This is a design decision — say so if
you prefer the stricter contract.**

Prepared RPC batches stay on their own per-row channel and are never
double-reported. A fatal error (severity ≥ 20) still retires the connection,
matching msodbcsql's `MINFATALERR`, so a deferred one cannot be pooled.

### 1.3 LOGIN7 server-name override

```rust
pub struct ClientContext { pub login_server_name: Option<String>, /* … */ }
```

Separates *where the socket dials* from *what name is presented at login* — what
a tunnel, proxy or port-forward needs. `None` reproduces today's behaviour
exactly. The override is taken verbatim, not reformatted.

LOGIN7 stores this as an offset/length pair separate from its payload, and the
record `Length` and feature-extension offset are computed from it too, so all
three go through one accessor. A length from the dialled address with bytes from
the override would corrupt the packet while looking correct client-side.

### 1.4 Entra authentication methods

Five variants added: `ActiveDirectoryAzCli`, `ActiveDirectoryAzureDeveloperCli`,
`ActiveDirectoryAzurePipelines`, `ActiveDirectoryEnvironment`,
`ActiveDirectoryClientAssertion`. All resolve to a bearer token out of band, so
they share the fedauth arm with `ActiveDirectoryDefault`. `ActiveDirectoryMSI`
had no arm at all and fell through to the unsupported-method error — it only
worked because the ODBC binding rewrites the keyword first — so it now shares
the managed-identity arm.

**Scope note.** This makes the methods *nameable and negotiable*. `mssql-tds`
acquires no tokens: `auth_method_map` is a public, empty map and the embedder
registers a factory. The credentials themselves are implemented in #622.

### Compatibility

Additive. No signature changes, no behavioural change unless a caller opts in.
The one cross-crate consequence is `mssql-py-core`, which matches
`TdsAuthenticationMethod` exhaustively and needed the five new arms.

---

## Part 2 — the CLI (#622)

Replaces the `mssql-tds-cli` REPL with a Rust `sqlcmd`, shipping as binary
`sqlcmd`. Roughly 40 modules:

| Area | Modules |
|---|---|
| Argument parsing | `cli/{args,spec,usage,validate}` |
| Batch handling | `batch/{scanner,substitute}`, `commands`, `vars` |
| Execution | `exec/{connect,runner,entra/oauth}`, `session` |
| Output | `fmt/{table,layout,value,widths,color,regional,report,schemes}` |
| Modern sub-commands | `modern/{config,open,server}_cmds`, `sqlconfig`, `yaml`, `container` |
| Misc | `dsn`, `exitcode`, `io`, `messages`, `tracing`, `ffi` |

### Entra credentials

All seven methods go-sqlcmd names are implemented against the `azure_identity`
Rust SDK: `AzureCliCredential`, `AzureDeveloperCliCredential`,
`AzurePipelinesCredential` (needs `SYSTEM_ACCESSTOKEN`), `ClientAssertionCredential`
(assertion via `-P`), `WorkloadIdentityCredential`, device code, and a
hand-built `ActiveDirectoryEnvironment`.

**Known narrowing:** the Rust SDK has no `EnvironmentCredential`, so ours is
built from `AZURE_CLIENT_ID` / `AZURE_TENANT_ID` / `AZURE_CLIENT_SECRET` and
covers the service-principal-with-secret shape only. Other SDKs also honour the
certificate and username/password forms. Worth confirming against go-sqlcmd
before claiming parity.

### The `compat-go` build feature

ODBC `sqlcmd` and `go-sqlcmd` disagree in places — notably `(1 row affected)`
versus `(1 rows affected)`. Default builds follow ODBC; `--features compat-go`
follows go-sqlcmd with no runtime flag. `--compat <odbc|go>` and `SQLCMDCOMPAT`
override at run time either way.

### Known gap

The crate has **no automated tests of its own**. Behaviour is verified by
differential comparison against ODBC `sqlcmd` (below) and by manual exercise.
Agreeing the coverage bar before merge is the main open item.

---

## Verification

Windows, against SQL Server 2025 in Docker.

| Check | Result |
|---|---|
| `cargo bfmt` | pass |
| `cargo bclippy` | pass |
| `cargo nextest run --workspace --all-targets` | 4588 run, 4564 passed, **24 failed**, 14 skipped |
| Differential vs ODBC `sqlcmd` | **10 / 10 identical** |

The 24 failures are pre-existing and environmental — named pipe, shared memory,
mock TLS, and live-connectivity tests on this box — plus
`test_bulk_copy_table_lock_actual_locking_behavior`, which reproduces as flaky
in isolation (pass / fail / pass). None are attributable to these changes. CI is
the authoritative result.

Reproducing locally requires generating `mssql-tds/tests/test_certificates/`
first (gitignored), or seven certificate tests fail for unrelated reasons.
`mssql-py-core` is excluded from the workspace and needs a real Python
interpreter, so on Windows it must be verified under WSL — `cargo bclippy`
alone does not cover it.

### Differential testing

The same SQL run through ODBC `sqlcmd` and the Rust binary, output compared
byte-for-byte:

| Scenario | Result |
|---|---|
| variable assignment + `SELECT` | identical |
| `SET NOCOUNT ON` | identical |
| DML sequence (INSERT / UPDATE / DELETE) | identical |
| mid-batch `RAISERROR` | identical |
| zero rows vs. no count | identical |
| `PRINT` message | identical |
| multiple result sets | identical |
| NULL and mixed types | identical |
| empty result set | identical |
| severity 11 error only | identical |

**This is the test class that matters for a compatibility tool, and it is the
one that found the `SQLSELECT` bug** — which had passed unit tests, e2e tests, a
full 4,400-test suite, and automated review. Whatever coverage #622 lands with
should include it.

### Tests added in #617

23 new tests: 14 unit, 5 end-to-end, 4 mock-server wire tests.

The LOGIN7 tests assert on bytes read back off the wire rather than client
state, because the failure they guard against is a malformed packet that looks
correct from the client side.

Six were written in response to review, and each was confirmed to fail against
the unfixed code before being accepted:

- `the_record_length_follows_the_server_name_actually_written`
- `a_deferred_fatal_error_still_retires_the_connection`
- `a_prepared_batch_keeps_its_errors_off_the_deferred_queue`
- `a_prepared_batch_keeps_its_counts_off_the_done_log`
- `a_sqlselect_count_is_reported_only_for_a_row_set`
- `variable_assignment_counts_are_not_reported` (end-to-end)

---

## Review guidance

Suggested order for #617:

1. `client_context.rs` — the new field, accessor, and enum variants
2. `login.rs` — LOGIN7 serialization and record sizing
3. `tds_client.rs` — deferral and row-count collection (the largest diff)
4. `fedauth.rs` — Entra method mapping
5. `tests/` — `test_login_server_name.rs`, `query_results.rs`, `connectivity.rs`

Points where input is genuinely wanted:

- **Deferral semantics (§1.2)** — all errors collected vs. terminal errors as
  `Err`. Deliberate, but the stricter contract is defensible.
- **Row-count API surface (§1.1)** — `TdsClient` now has three public
  row-count accessors (`last_rows_affected`, `take_dml_result_counts`,
  `take_done_row_counts`). The third exists because the others drop the
  positional `None` that `SET NOCOUNT ON` requires, but consolidating is worth
  discussing.
- **CLI test strategy (§2)** — what automated coverage gates the merge.
- **`compat-go` as a build feature** vs. run-time-only selection.

## Review history

Ten findings from automated review are addressed and their threads resolved:
the exhaustive `mssql-py-core` match; two ERROR arms bypassing
`record_error_token()` (fatal retirement and prepared-row association); the
LOGIN7 record `Length`; the terminal-error contract; the prepared-batch error
channel; the row-count contract scope; a fixed-delay race in the LOGIN7 wire
tests; and the prepared-batch row-count channel.

Two of these were real bugs that existing tests passed straight through, and the
`SQLSELECT` bug was not found by review at all.

## Open items

- [ ] Close #584 as superseded
- [ ] Agree CLI test strategy, including differential coverage
- [ ] Confirm `ActiveDirectoryEnvironment` parity with go-sqlcmd
- [ ] Mark both PRs ready and request review (#617 merges first)
