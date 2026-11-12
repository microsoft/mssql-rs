---
type: idea
sources:
  - "https://github.com/deadpool-rs/deadpool"
  - "https://github.com/djc/bb8"
  - "https://github.com/ssrlive/connection-pool"
  - "https://crates.io/crates/deadpool"
  - "https://crates.io/crates/bb8"
  - "https://crates.io/crates/connection-pool"
verified_at: 2026-11-05
---

# Async Connection Pool Core: Build vs. Reuse Analysis

| | |
|---|---|
| **Status** | Deadpool preferred; adapter proof-of-fit and dependency approval required |
| **Date** | 2026-11-05 |
| **Scope** | How the pool core (capacity, waiting, cancellation) inside `mssql-py-core` is implemented |
| **Tracking** | AB#50002 (async connection pooling) |

## 1. Background

The `mssql-python` async API (internal preview) runs on the Rust path: Python calls `mssql-py-core`, which drives `mssql-tds` directly over TDS. Today every async `connect()` opens a new physical connection (TCP, TLS, login, and a token fetch for Entra) and every `close()` discards it. For async web workloads that connection cost often exceeds the cost of the query itself.

Async connection pooling adds a pool inside `mssql-py-core`, in Rust, consistent with the direction of consolidating the driver on the Rust core. Behavior and scope are defined by the two async pooling design documents: one for the Rust pool engine, one for the `mssql-python` integration. This analysis does not change that scope. It examines one implementation question:

> Should the pool core be written from scratch as the design describes, or built on an existing Rust pooling crate?

**Recommendation:** use `deadpool::managed` behind a driver-owned capacity-slot
adapter, subject to the gates below. It is a better fit than bb8 for caller-owned
work and lifecycle control, not a complete implementation of the design.
The [pool-engine design](./async-connection-pooling-mssql-rs.md) section 5.0 is
the authoritative integration contract; the
[AQE integration design](./async-connection-pooling-mssql-python-integration.md)
retains responsibility for logical ownership, Python credentials and DB-API behavior.

The work is split into seven tasks. Only the first two are affected by the answer.

| Task | Area | Affected by this question |
|---|---|---|
| Capacity limits and fair waiting | Pool core | Yes, substantially |
| Enable, disable, shutdown and idle cleanup | Pool lifecycle | Yes, slightly |
| Reset on checkout, cleanup on return | SQL session handling | No |
| Revoke stale cursor access | Async connection ownership | No |
| Safe close and transaction handling | Async connection close | No |
| Connect and close through the pool | Native binding | No |
| AsyncPool API, pool key, tests and benchmark | `mssql-python` | No |

## 2. Requirements

Each option is assessed against the guarantees the pool design depends on.

| # | Requirement | Why it matters |
|---|---|---|
| 1 | **Each request opens its own connection** | Login, deadline and cancellation belong to the caller. Entra login in particular is bound to the caller's event loop and credential. |
| 2 | **Fair waiting** | When the pool is full, requests are served in arrival order. Prevents starvation and keeps tail latency predictable. |
| 3 | **A real cap** | `max_size` per key must hold for connections that are opening, in use, idle or closing. It is a promise to the server, and Azure SQL enforces connection limits per tier. |
| 4 | **Safe cancellation** | asyncio cancels tasks routinely (timeouts, client disconnects). A cancelled request must never leak or lose a slot. |
| 5 | **No background tasks** | Pool work is driven by callers and explicit shutdown, so nothing runs after the application's event loop is gone. |
| 6 | **Maturity** | This is core infrastructure for the Rust driver; it should be proven in production. |

## 3. Options Considered

| Option | Description | Assessment |
|---|---|---|
| A. Build from scratch | Own waiting line, slot accounting and cancellation handling, as the design specifies | Viable |
| B. `deadpool` + wrappers | Reuse managed slot waiting/storage/accounting; driver enforces physical-client and lifecycle policy | Preferred, conditional on section 7 gates |
| C. `bb8` | Async Tokio pool originally based on r2d2 | Poorer fit: spawned opens, notification competition and no equivalent public close |
| D. `connection-pool` | Small generic async pool, suggested during design review | Does not fit: fails requirements 3, 5 and 6 |
| E. `r2d2` | Synchronous pool; blocks threads; last release 2022 | Screened out |
| F. `mobc` | Async pool with lower adoption (~2.5M downloads) | Screened out, not source-reviewed |
| G. Pool in Python (asyncio) | Pool logic in `mssql-python` over native connections | Conflicts with consolidating on the Rust core |
| H. Reuse an existing sync pool | The C++ pool in `mssql-python` or the `mssql-odbc` pooling primitives | Blocking, and on the path being retired |

## 4. Evaluation

**Method:** release-source/API review of `bb8` 0.9.1 and `deadpool` 0.13.1,
checked on 2026-11-05. The `connection-pool` 0.3.9 notes below are retained from
the earlier analysis, not independently revalidated in this update. No adapter
prototype, benchmark, live-database execution or cross-platform test was run.
Download totals and unmeasured delivery estimates are not correctness evidence.

### 4.1 Summary

| Requirement | connection-pool | bb8 | deadpool | Build from scratch |
|---|---|---|---|---|
| **1. Opens its own connection** | ✅ | ❌ Background worker opens connections for whoever waits | ✅ | ✅ |
| **2. Fair waiting** | ✅ Fair queue | ❌ Newcomers can jump the queue | ✅ Fair queue | ✅ Ours to prove |
| **3. Real cap** | Earlier review: releases capacity before cleanup | Bounds managed connections; strict retirement contract still needs review | Driver slot guard must retain capacity through physical destruction and generations | Ours to prove |
| **4. Safe cancellation** | Earlier review: generic permit cleanup | Generic checkout safety; spawned opens can outlive get | Generic object safety; driver must supervise all TDS/Python work | Ours to prove |
| **5. No background tasks** | Earlier review: background cleanup/returns | Spawns opens; default idle/lifetime settings also start reaper | Managed core requires no background reaper/open task | Ours to prove |
| **6. Maturity** | Not revalidated | Established external implementation, MIT | Established external implementation, MIT OR Apache-2.0 | New driver-owned scheduler |

### 4.2 Candidate Notes

**connection-pool (0.3.9).** Earlier review observations, not revalidated here:

- The returned connection's capacity permit lives in `ManagedConnection`. Its `Drop` releases the permit and spawns a background task to validate and shelve the connection, so live connections can exceed `max_size` while those tasks run.
- `MAX_IDLE_KEEP` is a compile-time constant of 8. With `max_size = 100`, all but 8 returned connections are closed, so bursty load re-logs in constantly.
- A background cleanup task runs on an interval.
- Cancellation releases the permit automatically. An idle connection being validated at the moment of cancellation is dropped rather than returned.

**bb8 (0.9.1).**

- `get()` calls `spawn_replenishing_approvals`, which opens connections inside `tokio::spawn`. Cancelling/timing out one get does not inherently cancel that open. Per-caller login context needs additional explicit machinery.
- Waiters wait on `Notify::notified()` and re-check for a connection when woken, so a new arrival can take a freed connection first.
- Default idle/lifetime settings start a reaper. Disabling both avoids that timer, but does not remove spawned connection creation.
- Strengths: it counts pending opens toward the limit, exposes statistics, and applies one `connection_timeout` to the whole checkout.
- Async `is_valid` can run TDS reset, but normal return has only synchronous `has_broken`; awaited cleanup remains driver code. There is no equivalent public close method for waking/closing all acquisitions.

**deadpool (0.13.1).** Licensed MIT OR Apache-2.0; its required library
dependencies are Tokio sync primitives and `deadpool-runtime`. Optional features
can add dependencies. Enable only the needed features.

- `timeout_get` takes a permit from a fair tokio `Semaphore`, then recycles an idle object or creates a new one **inside the caller's future**.
- The `UnreadyObject` guard cleans up correctly when a create or recycle is cancelled.
- `Manager::recycle` is an async checkout hook where a conventional adapter can reset sessions. Our stricter per-call, typed-error and retry requirements favor driver-owned reset after slot checkout instead.
- `retain` (pruning), `close` and `status` (counters) are available.
- Built-in wait/create/recycle timeouts are separate phase budgets, not the complete acquisition deadline.
- A recycle error discards a candidate and retries; fatal identity/provider failures must not be hidden in that path.
- Behaviors that differ from the design are covered in section 5.

## 5. Gap Analysis: deadpool

| Gap | Required driver treatment |
|---|---|
| Object accounting is not the complete physical-reservation contract | Hold the managed slot through local client destruction; never free its capacity first |
| Ordinary Object Drop returns without awaited cleanup | Driver guard destroys the client, then returns an empty slot; explicit release is the only reuse path |
| No per-call create/recycle arguments | Pool slots, then explicitly call the current factory/reset/isolation code while owning a slot |
| Recycle errors are candidate rejection, not fatal caller errors | Keep identity/provider errors and bounded TDS candidate retry in the outer acquisition path |
| Separate phase timeouts | One outer absolute deadline and cancellation policy, including Python pre-key work |
| No exact queued-only `max_waiting` API | Prototype admission bookkeeping against actual queue transitions; a counter around get/status is not sufficient |
| Close is irreversible and does not await graceful TDS shutdown | Reversible driver gate retains the same slot pool; permanent shutdown joins driver-tracked work |
| Retain/take/resize can release accounting before removed clients are destroyed | Do not remove live client-bearing slots during active admission; maintenance barrier and off-lock destruction required |
| Metrics measure creation/recycle, not return | Own monotonic `last_returned` and token-expiry metadata |
| Generic cancellation cannot supervise Python/executor jobs | AQE bridge cancels/joins tracked async work, bounds executor submissions and ignores late results |

These are design obligations, not five already-proven small wrappers. No fork is
proposed, but feasibility of the exact queue/lifecycle contract remains an A1
acceptance gate. If it requires rebuilding a full second scheduler, reconsider
reuse rather than quietly weakening the design.

### 5.1 Slot-based integration

```text
async_query / PyO3 logical connection
  -> AsyncPoolManager: key, lifecycle, generation, deadline and queue policy
  -> per-key Deadpool managed slot pool: bounded checkout and returned storage
  -> driver acquisition/lease guard: slot plus zero or one unique TdsClient
  -> current-call ConnectionFactory / TDS reset, isolation and cleanup
```

`Manager::create()` creates an empty capacity slot; `recycle()` is non-I/O slot
bookkeeping. Once get completes, the driver's guard owns the slot before any
further await. It either validates/resets its cached client or opens a physical
client through the current call's explicit factory. Both paths apply and fully
drain isolation initialization before publication.

On explicit release, await drain then residual rollback, check generation and
store the clean client with its return timestamp before returning the slot.
On error, timeout, cancellation or bare Drop, destroy the physical client before
returning an empty slot. Never put a dirty client back and hope a later recycle
will destroy it; never use `Object::take()` to free capacity while its client lives.

Keep the same underlying slot pool across reversible disable/re-enable, with
driver generation cancellation and atomic publication/return checks. Do not
close/recreate it while old clients remain. Permanent eviction/closure requires
driver lifecycle coordination and destruction outside locks. Empty cached slots
are not live physical connections; Deadpool `status().size` is not our physical
connection metric.

This avoids both a second capacity semaphore and a task-local credential bridge.
The more conventional raw-client Manager adapter is not selected because it
needs extra machinery for per-call context, fatal recycle errors and retirement.

### 5.2 Responsibility split

| Deadpool supplies | Developers must implement and verify |
|---|---|
| Bounded managed slots and fair semaphore wait | Exact physical cap, FIFO boundary semantics, max_waiting and retry policy |
| Returned-object storage and wakeup/accounting | Clean versus empty slot handling, synchronous discard guards and idle maintenance |
| Generic async manager hooks | TDS factory, liveness/reset/ACK/drain/isolation and auth metadata |
| Generic close/retain/status primitives | Reversible generations, atomic return/publication and graceful shutdown |
| Generic library cancellation handling | End-to-end Rust/Python cancellation and one total deadline |
| No Python-specific behavior | Complete keying, logical lease cells, close barrier, credentials and DB-API errors |

## 6. Effort and Risk

- Reuse removes the need to implement a generic slot scheduler and storage.
  Driver lifecycle and exact admission semantics still need substantial tests.
- SQL Server session handling and AQE/Python ownership work are not removed.
- Do not estimate a percentage saving, a halving of work, or lower runtime
  overhead without an implementation spike and workload evidence.
- Upstream maturity reduces the amount of new generic machinery, but it does not
  prove the adapter preserves its guarantees. Dependency/license onboarding and
  version-upgrade contract tests are additional work.

## 7. Proposal

**Prefer Deadpool's managed slot core, contingent on an adapter proof-of-fit.**

### 7.1 The case for it

- Its inline manager execution, fair semaphore and lack of mandatory background
  tasks fit the design better than bb8's conventional replenishing architecture.
- Generic slot checkout/storage stays external; SQL Server and AQE policy stays
  explicit and testable in the driver.
- Private library types preserve the public API if the prototype instead selects
  the custom core. No customer-facing Deadpool API is introduced.

### 7.2 What we give up

- The custom-core structs/dispatcher in Doc 1 become a reference contract, not
  code to implement alongside Deadpool.
- We own an adapter around a library whose queue transitions are not public.
  Exact max_waiting and maintenance semantics are not yet proven.
- We accept dependency/version maintenance, not weaker physical capacity,
  delayed discard, hidden authentication errors or implicit background work.

### 7.3 Alternatives if not adopted

- The custom core remains the fallback if the exact requirements cannot be
  expressed through supported Deadpool APIs without recreating the scheduler.
- bb8 is appropriate for conventional fixed-manager pools that accept spawned
  replenishment; choosing that model here would require explicit design changes.
- The earlier connection-pool assessment is not a substitute for revalidation
  if it is reconsidered.

### 7.4 Proposed guardrails

- Start the prototype against reviewed 0.13.1; select the dependency requirement
  through normal onboarding and review source/changelog plus tests on upgrades.
- Never expose raw Deadpool Objects/client Arcs, duplicate its capacity scheduler,
  rely on status for physical accounting, or swallow fatal errors in recycle.
- Prove exact queue boundaries, FIFO/no bypass, retry-tail order and cancellation
  before/after assignment. Queue limits count only live queued requests.
- Instrument client destruction and slot reuse to verify the strict cap during
  open/reset/release cancellation, pruning and disable/re-enable with old leases.
- Test repeated asyncio loops, current-call credential context, no stale finalizer
  access, and auth cancellation/late results.
- Use live SQL for SPID/reset/isolation/transaction effects, not only mock success.
- Record unverified OS/topology cases and measure eager carrier/cleanup cost.

### 7.5 Questions for discussion

- Timeout scope is already settled by Doc 1: a distinct overall acquisition
  deadline; the three numeric budget defaults still require agreement.
- Can the pinned API preserve exact queue accounting, maintenance and reversible
  lifecycle without a second scheduler? A1 must answer before production work.
- If a gate fails, retain the custom core or explicitly approve a changed
  contract. Disable/re-enable and strict physical accounting remain requirements
  unless the design owners approve otherwise.
- Update AB#50002's pool-core/lifecycle task descriptions after approval; this
  document update does not itself change work items or claim implementation.

## 8. Reviewed upstream evidence

- [Deadpool manager hooks](https://docs.rs/deadpool/0.13.1/deadpool/managed/trait.Manager.html)
- [Deadpool acquisition/lifecycle/accounting source](https://docs.rs/deadpool/0.13.1/src/deadpool/managed/pool.rs.html)
- [Object Drop and take](https://docs.rs/deadpool/0.13.1/src/deadpool/managed/object.rs.html)
- [Metrics timestamps](https://docs.rs/deadpool/0.13.1/src/deadpool/managed/metrics.rs.html)
- [Phase timeouts](https://docs.rs/deadpool/0.13.1/deadpool/managed/struct.Timeouts.html)
- [Generic cancellation tests](https://docs.rs/crate/deadpool/0.13.1/source/tests/managed_cancellation.rs)
- [bb8 checkout and spawned connection creation](https://docs.rs/bb8/0.9.1/src/bb8/inner.rs.html)
- [bb8 public pool API](https://docs.rs/bb8/0.9.1/bb8/struct.Pool.html)
