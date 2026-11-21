# Async Query Execution Design for mssql-python

This document explains the existing preview async API, not a proposal to add
async support. `PyAsyncConnection` and `PyAsyncCursor` already use the shared
Tokio runtime and `pyo3-async-runtimes`. Integrating the private Rust pool with
that API remains separate work governed by the
[pool integration design](./async-connection-pooling-mssql-python-integration.md).
---
## Table of Contents

- [Async Query Execution Design for mssql-python](#async-query-execution-design-for-mssql-python)
  - [Table of Contents](#table-of-contents)
  - [Glossary](#glossary)
  - [1. Why "async"? The core idea](#1-why-async-the-core-idea)
  - [2. Problem statement \& goals](#2-problem-statement--goals)
    - [Current baseline and remaining work](#current-baseline-and-remaining-work)
    - [Goals](#goals)
    - [Non-goals](#non-goals)
    - [Success criteria](#success-criteria)
  - [3. Background \& context](#3-background--context)
    - [Current architecture of `mssql-python`](#current-architecture-of-mssql-python)
    - [Complexities with asyncio + Tokio](#complexities-with-asyncio--tokio)
    - [Prior art](#prior-art)
  - [4. Existing architecture / interop design](#4-existing-architecture--interop-design)
  - [5. Design rules \& invariants](#5-design-rules--invariants)
  - [6. Mental model: tasks vs. threads](#6-mental-model-tasks-vs-threads)
  - [7. Async in Rust + Python](#7-async-in-rust--python)
    - [7.1 The core problem: two event loops](#71-the-core-problem-two-event-loops)
    - [7.2 The layered architecture](#72-the-layered-architecture)
      - [Understanding `pycore` vs. `pyo3-async-runtimes`](#understanding-pycore-vs-pyo3-async-runtimes)
    - [7.3 pyo3-async-runtimes as generic bridge plugin (the contract)](#73-pyo3-async-runtimes-as-generic-bridge-plugin-the-contract)
    - [7.4 Example Walkthrough - Two connections, two cursors (mssql-python)](#74-example-walkthrough---two-connections-two-cursors-mssql-python)
    - [7.5 When is a thread assigned? When is a task assigned? When is the GIL held?](#75-when-is-a-thread-assigned-when-is-a-task-assigned-when-is-the-gil-held)
      - [Native Rust — thread vs. task timeline](#native-rust--thread-vs-task-timeline)
      - [Python async — thread + task + GIL timeline](#python-async--thread--task--gil-timeline)
      - [GIL flow (Python)](#gil-flow-python)
    - [7.6 How PyO3 bridges the schedulers (and where it briefly blocks)](#76-how-pyo3-bridges-the-schedulers-and-where-it-briefly-blocks)
      - [The two schedulers PyO3 connects](#the-two-schedulers-pyo3-connects)
      - [Step-by-step: threads, tasks, and GIL at each crossing](#step-by-step-threads-tasks-and-gil-at-each-crossing)
      - [Where does momentary blocking actually happen?](#where-does-momentary-blocking-actually-happen)
      - [Two kinds of "blocking" — only one matters](#two-kinds-of-blocking--only-one-matters)
      - [The GIL is a lock on objects, not on a code layer](#the-gil-is-a-lock-on-objects-not-on-a-code-layer)
      - [The one place to watch: result materialization (point E)](#the-one-place-to-watch-result-materialization-point-e)
  - [8. Sync vs. existing async in pycore](#8-sync-vs-existing-async-in-pycore)
    - [8.1 The synchronous model](#81-the-synchronous-model)
    - [8.2 The existing async model](#82-the-existing-async-model)
    - [8.3 Invariants preserved by the async surface](#83-invariants-preserved-by-the-async-surface)
  - [9. End-to-end walkthrough between Python and Rust](#9-end-to-end-walkthrough-between-python-and-rust)
  - [10. Preview delivery and integration boundaries](#10-preview-delivery-and-integration-boundaries)
    - [Existing namespace](#existing-namespace)
    - [Versioning \& deprecation](#versioning--deprecation)
    - [Documentation \& migration](#documentation--migration)
  - [11. Appendix](#11-appendix)
    - [11.1 How async works in native Rust (mssql-tds)](#111-how-async-works-in-native-rust-mssql-tds)
      - [11.1.1 The lifecycle of a query](#1111-the-lifecycle-of-a-query)
      - [11.1.2 Example Walkthrough: Two connections, two cursors (Async)](#1112-example-walkthrough-two-connections-two-cursors-async)
      - [11.1.3 What the Rust app looks like](#1113-what-the-rust-app-looks-like)
    - [11.2 Concurrency model: connections vs. cursors](#112-concurrency-model-connections-vs-cursors)

---

## Glossary

| Term                      | Meaning                                                                                                                        |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| **Thread (OS thread)**    | Real kernel-scheduled worker. Few of them (~CPU count). Does CPU work.                                                         |
| **Task**                  | A cheap future/state machine describing a unit of async work. Many of them.                                                    |
| **Future**                | A value that will be ready later; polled to completion by the runtime.                                                         |
| **Tokio runtime**         | The async executor: a worker thread pool plus a reactor.                                                                       |
| **Reactor**               | The layer (epoll on Linux, IOCP on Windows) that watches sockets and wakes tasks when I/O is ready.                            |
| **`.await`**              | The point where a task can yield its thread while waiting.                                                                     |
| **MARS**                  | Multiple Active Result Sets — multiple in-flight requests on one connection. Parsed at login but **not yet implemented** here. |
| **GIL**                   | Python's Global Interpreter Lock — only one thread runs Python bytecode at a time. Released during Rust I/O.                   |
| **`pyo3-async-runtimes`** | Crate that bridges a Tokio future to a Python `asyncio` awaitable (`future_into_py`).                                          |
| **`block_on`**            | Blocking call that parks the current thread until a future completes — the opposite of true async.                             |
| **Statement handle**      | An `i32` returned by `sp_prepare`; identifies a server-side prepared plan.                                                     |

---

## 1. Why "async"? The core idea

A database query spends **almost all of its time waiting** for the network — the client sends a request, then waits for SQL Server to compute and stream back results. Traditional ("blocking") code glues a whole **OS thread** to that wait: the thread sits idle, doing nothing, until the bytes arrive.

**Async execution removes that waste.** Instead of parking a thread, the code *lets go* of the thread while it waits, so the thread can do other useful work. When the network data finally arrives, the work is picked back up.

**The single most important idea in this document:**

> **A query does NOT need its own thread. Waiting on the network should consume ZERO threads.**

That is what we mean by *true async*.

---

## 2. Problem statement & goals

### Current baseline and remaining work

The synchronous DDBC/ODBC path blocks its calling thread during database I/O,
even when it releases the GIL. `PyCoreConnection` and `PyCoreCursor` are also
synchronous bindings: they use a per-connection runtime and `block_on`.

The preview async path already exists alongside those sync APIs:

- [Module registration](../mssql-py-core/src/lib.rs) exports `PyAsyncConnection`
  and `PyAsyncCursor`.
- [The async runtime](../mssql-py-core/src/async_runtime.rs) initializes one shared
  Tokio executor for async connections and the `future_into_py` bridge.
- [Async execution](../mssql-py-core/src/async_execute.rs) and
  [fetching](../mssql-py-core/src/async_fetch.rs) expose awaitable operations.
  Execution positions on a result; fetching reads and converts its rows.

The private Rust pool's safe return is implemented, but wiring that pool into
Python/AQE ownership, close barriers and result delivery remains deferred.
Native async support itself is not that deferred slice.

### Goals

- Explain the existing native awaitable connection/cursor APIs and their integration with the running `asyncio` loop.
- **Never block the event loop** during a query — the loop stays free to run other coroutines while a query is in flight.
- **True concurrency across connections** without a thread-per-query model.
- Preserve the existing **`pyo3-async-runtimes`** bridge over `mssql-tds`, rather than making the synchronous ODBC path async.
- Preserve **one shared Tokio runtime for the async surface**; synchronous connections still own their separate runtimes.

### Non-goals

- **Not** rewriting or removing the existing **DDBC/ODBC sync** call path — it remains the default for sync consumers.
- **Not** changing **connection-pooling semantics** or transaction behavior.
- **Not** making the **ODBC path** itself async — async is delivered on the Rust (`mssql-py-core`) path.
- **Not** implementing **MARS** (multiple active result sets on one connection) — parallelism stays per-connection.
- **Not** redesigning the TDS protocol layer or the result-materialization format.

### Success criteria

- An `asyncio` app can run **N queries across N connections concurrently** with a single event-loop thread and **no `run_in_executor`** (meaning its natively async).
- While a query awaits the network, the event loop demonstrably **runs other coroutines** (no stall).
- The GIL is held only at the **argument-in / rows-out** boundaries (microsecond scale), never during the network round-trip.
- **No regression** in the existing sync API.

---

## 3. Background & context

### Current architecture of `mssql-python`

- **Primary path today — DDBC.** Query execution goes through **DDBC**, a **C++ extension** that drives the bundled **Microsoft ODBC Driver for SQL Server** directly (no external driver manager). Python `connect()`, `cursor.execute()`, and `fetchall()` map onto **synchronous ODBC calls**; the C++ layer releases the GIL around the blocking call, but the calling thread/coroutine is blocked for the round-trip.
- **Synchronous Rust bindings.** `PyCoreConnection` and `PyCoreCursor` use `runtime.block_on(...)` inside `py.detach()`, with a runtime per connection.
- **Existing async Rust bindings.** `PyAsyncConnection` and `PyAsyncCursor` use the shared async runtime and return awaitables through `pyo3-async-runtimes`. Logical connection state and operation claims guard access to the physical client's async mutex.
- **Scope.** This document explains that async path. The ODBC path stays synchronous, and Python/AQE pool integration has its own design and approval gates.

### Complexities with asyncio + Tokio

- **Two runtimes.** Python's `asyncio` and Rust's **Tokio** are independent schedulers, each with its own event loop and notion of "task."
- **Two threading models.** `asyncio` is **single-threaded cooperative**; Tokio is **multi-threaded work-stealing**. Values crossing the boundary must be thread-safe (`Send`).
- **The GIL.** Only one thread may touch Python objects at a time. The bridge must **release** the GIL during the query and **re-acquire** it only to build result objects and wake the loop — otherwise the "async" is fake.

### Prior art

| Project                                    | Approach                                                                    | Takeaway for us                                                                                   |
| ------------------------------------------ | --------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| **asyncpg**                                | Pure-Python asyncio Postgres driver with a C-accelerated protocol           | Shows the value of a first-class async API; but it is not a Rust-core binding.                    |
| **tokio-postgres**                         | Native Rust async Postgres driver on Tokio                                  | Confirms the Tokio `Future` + reactor model our core already uses.                                |
| **pyo3-asyncio / pyo3-async-runtimes**     | Generic crate bridging Rust futures to Python awaitables (`future_into_py`) | **The exact mechanism we adopt** — no need to hand-roll the asyncio↔Tokio bridge.                 |
| **`mssql-js` (MS owned - unreleased lib)** | NAPI binding exposing `#[napi] async fn` returning JS Promises              | In-repo proof that the async core bridges cleanly to a host runtime; the Python model mirrors it. |

---

## 4. Existing architecture / interop design

This section is the **overview**; the detailed mechanics, mental models, and diagrams follow in **sections 4–11**.

- **Event-loop handoff (asyncio → Tokio).** A pycore method hands its Rust `Future` to **`pyo3-async-runtimes`** via `future_into_py`, which **spawns it on a shared, process-wide Tokio runtime** and returns a Python awaitable. All async connections share that worker pool; synchronous connections do not use it. See [§7.1](#71-the-core-problem-two-event-loops)–[§7.3](#73-pyo3-async-runtimes-as-generic-bridge-plugin-the-contract) and [§8](#8-sync-vs-existing-async-in-pycore).
- **Future / coroutine bridging.** A Rust `async` block *is* a `Future`; the bridge converts it into an `asyncio.Future`. The contract is just **`Future` in → awaitable out** (plus `IntoPyObject` for the result). See [§7.3](#73-pyo3-async-runtimes-as-generic-bridge-plugin-the-contract).
- **GIL management.** The GIL is **held** only to read Python args in and build Python rows out; **released** for the whole network round-trip and TDS parsing. See [§7.5](#75-when-is-a-thread-assigned-when-is-a-task-assigned-when-is-the-gil-held)–[§7.6](#76-how-pyo3-bridges-the-schedulers-and-where-it-briefly-blocks).
- **Threading model (which thread runs what).** The **Python loop thread** runs coroutines; **Tokio worker threads** run queries; the **reactor** wakes tasks. Detailed timelines are in [§7.5](#75-when-is-a-thread-assigned-when-is-a-task-assigned-when-is-the-gil-held); the full crossing diagram is in [§7.6](#76-how-pyo3-bridges-the-schedulers-and-where-it-briefly-blocks).
- **Data flow for a single async query.** The end-to-end path (call → spawn → run → send → wake → build objects → resolve) is walked with diagrams in [§7.4](#74-example-walkthrough---two-connections-two-cursors-mssql-python) (two-connection view) and [§9](#9-end-to-end-walkthrough-between-python-and-rust) (single-query sequence).

---

## 5. Design rules & invariants

These hold for both native Rust and the Python bridge:

1. **Tasks, not threads, carry queries.** At every `.await` on the network, the thread is released and reused.
2. **The reactor (epoll/IOCP) wakes the right task** when the server's bytes arrive; any free worker thread resumes it.
3. **Parallelism scales with connections, not cursors.** Python async cursors sharing a connection require caller-managed sequencing. Execution raises `RuntimeError` if another cursor owns the active operation or batch; it does not automatically wait.
4. **Logical ownership and operation claims guard access to the client's mutex.** The mutex serializes one connection's I/O; it does not couple different connections. A mutex alone is not a pooled close/settlement barrier.
5. **The statement handle is a plain server-side `i32`.** Keep it pinned to its originating connection; no synchronization concern.
6. **For the existing Python async API:** one shared Tokio runtime + `future_into_py`, with the GIL held at Python-object boundaries. The synchronous API retains `block_on`.
7. **The JS binding (`mssql-js`) is the working reference model** — its `#[napi] async fn` methods already return promises without blocking the Node.js event loop.

---

## 6. Mental model: tasks vs. threads

The whole system rests on distinguishing two things people often conflate.

| Concept                | What it is                                                                         | Cost                       | How many?                                      |
| ---------------------- | ---------------------------------------------------------------------------------- | -------------------------- | ---------------------------------------------- |
| **Thread (OS thread)** | A real worker scheduled by the OS kernel. Does actual CPU work.                    | Heavy (~MBs of stack)      | **Few** — Tokio uses ~one per CPU core, fixed. |
| **Task (Tokio task)**  | A *future* / state machine describing "what work to do and what it's waiting for." | Cheap (~hundreds of bytes) | **Many** — hundreds of thousands possible.     |

**Key takeaways:**
- A task is **not** a thread. A task is *a unit of work* that threads pick up, run for a bit, and put down.
- A thread is only tied to a task **while that task is actively running CPU work** (building a packet, parsing rows).
- **During the network wait — 99% of a query's life — no thread is assigned to the task at all.**

---

## 7. Async in Rust + Python

Python complicates things because it has **its own event loop** (`asyncio`), separate from Tokio. True async means making **two event loops cooperate** so neither blocks the other.

### 7.1 The core problem: two event loops

```mermaid
flowchart LR
    subgraph Py["Python process"]
        AL["asyncio event loop, single thread, runs coroutines"]
    end
    subgraph Rust["Rust pycore native"]
        TK["Tokio runtime, worker thread pool and reactor"]
    end
    AL <-. must cooperate .-> TK
```

- **Python's asyncio loop** runs coroutines (`async def`) on **one thread**. If anything blocks it, the whole app freezes.
- **Tokio** runs tasks on its own worker threads (Section 6).
- The bridge's job: when Python does `await cursor.execute(...)`, hand the real work to **Tokio**, and give Python back an **awaitable** that resolves when Tokio finishes — **without blocking Python's loop**.

This is the bridge the existing async API already uses. It does not replace the synchronous API's `block_on` calls.

### 7.2 The layered architecture

```mermaid
flowchart TB
    subgraph L1["Layer 1 - Your Python app"]
        APP["async def main, await asyncio.gather q_a, q_b"]
    end
    subgraph L2["Layer 2 - asyncio event loop"]
        LOOP["Python event loop, 1 thread"]
    end
    subgraph L3["Layer 3 - pycore pyclass"]
        CONN["PyAsyncConnection"]
        CUR["PyAsyncCursor operations call future_into_py"]
    end
    subgraph L4["Layer 4 - pyo3-async-runtimes BRIDGE"]
        BR["Converts Tokio future to Python awaitable, handles GIL and wakers"]
    end
    subgraph L5["Layer 5 - Shared Tokio runtime"]
        RT["Worker threads and Reactor"]
    end
    subgraph L6["Layer 6 - mssql-tds core async"]
        TDS["Arc-Mutex-TdsClient to socket"]
    end
    APP --> LOOP --> CUR --> BR --> RT --> TDS
```

#### Understanding `pycore` vs. `pyo3-async-runtimes`

Layers 3 and 4 are **two different crates with different jobs**, and it's important not to conflate them:

|                 | **pycore** (`mssql-py-core`) — Layer 3                                                                                    | **pyo3-async-runtimes** — Layer 4                                            |
| --------------- | ------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| **What it is**  | *Existing* crate in mssql-rs which is SQL-specific binding                                                                | A *generic* third-party library                                              |
| **Knows about** | TDS, `TdsClient`, connections, cursors, rows                                                                              | Nothing about databases — only futures & loops                               |
| **Contains**    | `PyAsyncConnection`, `PyAsyncCursor`, operation claims, TDS execution/fetching, error mapping and Python conversion | `future_into_py`, runtime registration and the Tokio↔asyncio waker plumbing |
| **Role**        | Defines *what* Python can call and *what* the query does                                                                  | Defines *how* a Rust future becomes an awaitable Python can `await`          |
| **Dependency**  | Uses pyo3-async-runtimes                                                                                                  | Used by pycore (and any other async binding)                                 |

**The relationship:** pycore is the **application layer**; pyo3-async-runtimes is the **generic bridge** it calls into.

The following Rust-only helper demonstrates a completion-only bridge. It owns
an already connected client, discards rows and drains the entire batch before
resolving. Failure or abandonment drops that owned client; it is not cached.
This is not an implementation of `PyAsyncCursor`: production cursor methods
also maintain logical ownership, operation/cancellation guards and fetch state.

```rust
use mssql_tds::connection::tds_client::TdsClient;
use pyo3::prelude::*;

use crate::async_errors::map_tds_error;

fn execute_and_drain<'py>(
    py: Python<'py>,
    mut client: TdsClient,
    sql: String,
) -> PyResult<Bound<'py, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        client
            .execute(sql, ())
            .await
            .map_err(|error| map_tds_error("execute", error, client.take_info_messages()))?;
        client
            .close_query()
            .await
            .map_err(|error| map_tds_error("drain", error, client.take_info_messages()))?;
        Ok(())
    })
}
```

`map_tds_error` is the existing internal DB-API mapper in
[async_errors.rs](../mssql-py-core/src/async_errors.rs), not a public extension
point. A delayed error encountered while draining also rejects the awaitable.

- **pycore** wrote everything except `future_into_py`.
- **pyo3-async-runtimes** provides *only* `future_into_py` (and the runtime/waker machinery behind it): it takes your Rust future, spawns it on Tokio, and returns a Python awaitable.

**Analogy:** pycore is the *chef* who knows the recipe (SQL/TDS); pyo3-async-runtimes is the *dumbwaiter* that carries the finished plate between the kitchen (Tokio) and the dining room (asyncio) — it doesn't cook, it just moves things across the boundary correctly.

### 7.3 pyo3-async-runtimes as generic bridge plugin (the contract)

You never modify `pyo3-async-runtimes` — you **add it as a dependency and call it**. A generic library like this works through a **contract** (Rust traits + a function signature), not domain knowledge. You conform to its interface; it does the rest.

**Import it** (in `mssql-py-core/Cargo.toml`):

```toml
[dependencies]
pyo3 = { version = "0.29.0", features = ["extension-module"] }
pyo3-async-runtimes = { version = "0.29.0", features = ["tokio-runtime"] }
tokio = { version = "1", features = ["full", "rt-multi-thread"] }
```

> Keep the `pyo3` and `pyo3-async-runtimes` **versions aligned** (both `0.29.x` here) — the bridge is pinned to a specific pyo3 version.

**The one-line deal:**

> You give it a Rust `Future`. It gives you back a Python awaitable.

It doesn't care whether that future runs a SQL query or downloads a file. Three generic mechanisms carry everything it needs:

| Mechanism                                  | What it is                             | How it conveys "what you need"                                                                                                                                 |
| ------------------------------------------ | -------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **The `Future` trait**                     | Rust's standard async abstraction      | Your `async { client.execute(...).await }` block *is* a `Future`. The lib only needs "something pollable" and drives it to completion — it never looks inside. |
| **Trait bound `IntoPyObject`**             | "I know how to become a Python object" | This is how results get handed back. You (or pyo3's derive macros) provide the conversion; the lib just calls it.                                              |
| **A Cargo feature flag** (`tokio-runtime`) | Compile-time config                    | Tells the lib *which* async runtime to spawn on.                                                                                                               |

Plus one runtime detail: at call time it grabs the **currently running asyncio loop** (`get_running_loop`), so it knows where to schedule the wakeup.

```mermaid
flowchart LR
    subgraph YOURS["pycore - you write"]
        F["async block = a Future<br/>output implements IntoPyObject"]
    end
    subgraph LIB["pyo3-async-runtimes - unchanged"]
        FIP["future_into_py<br/>poll future, convert output, wake loop"]
    end
    PYAW["Python awaitable"]
    F -->|pass Future in| FIP -->|hand awaitable out| PYAW
```

**Execution is not row delivery.** `TdsClient::execute(sql, options)` returns a
`StatementResult` navigation boundary, not a row collection. The existing
Python cursor keeps execution and fetching separate:

```python
cursor = connection.cursor()
try:
    await cursor.execute("SELECT 1 AS value")
    rows = await cursor.fetchall()
finally:
    await cursor.close()
```

`fetchall()` reads and converts the current result set; `nextset()` navigates
additional results. Cursor close drains remaining results when the caller is
finished. See [async_execute.rs](../mssql-py-core/src/async_execute.rs) and
[async_fetch.rs](../mssql-py-core/src/async_fetch.rs) for the existing error mapping,
row conversion and guarded result delivery. Do not add an unconditional drain
to cursor `execute`: that would discard rows intended for subsequent fetches.

**What the bridge does (conceptual pseudocode, not its implementation):**

```text
// ---- WHAT THE LIBRARY DOES (unchanged, generic) ----
future_into_py(py, fut):
    1. loop      = get_running_loop(py)       // read the asyncio loop
    2. py_future = asyncio.Future()           // the awaitable it returns
    3. tokio::spawn(async {
           let result = fut.await;            // polls YOUR future
           Python::with_gil(|py| {            // acquires the GIL
               let obj = result.into_pyobject(py);    // uses YOUR conversion
               loop.call_soon_threadsafe(py_future.set_result, obj)
           })
       })
    4. return py_future                       // Python can now `await` it
```

**The plumbing is the trait boundary.** You and the library meet at exactly two points: `Future` (input) and `IntoPyObject` (output). Neither side knows the other's internals — no forking, no patching, no teaching it about TDS.

**Analogy:** a **power adapter**. It doesn't know if you plug in a laptop or a lamp; it just conforms to a standard plug (`Future`) on one side and a standard socket (asyncio `Future`) on the other. You make your code fit the plug; the adapter handles the conversion across the boundary.

The dependency, shared runtime registration and awaitable bridge already exist.
The bridge does not implement database ownership, cancellation settlement,
DB-API errors or row conversion; those remain pycore responsibilities.

### 7.4 Example Walkthrough - Two connections, two cursors (mssql-python)

These diagrams combine `execute` followed by `fetchall` on each connection.
They are not one operation returning rows: execution and each fetch have
separate awaitables, with cursor/session state retained between operations.

```mermaid
flowchart TB

subgraph PY["Python asyncio Loop"]
    GA["Coroutine A<br/>await execute, then await fetchall"]
    GB["Coroutine B<br/>await execute, then await fetchall"]
end

subgraph PC["mssql-py-core"]
    CA["PyAsyncCursor A<br/>Read arguments<br/>Hold GIL"]
    CB["PyAsyncCursor B<br/>Read arguments<br/>Hold GIL"]
end

subgraph BR["pyo3-async-runtimes"]
    WA["future_into_py()<br/>Creates Python Awaitable A"]
    WB["future_into_py()<br/>Creates Python Awaitable B"]
end

subgraph TK["Tokio Runtime"]
    TA["Tokio Task A"]
    TB["Tokio Task B"]
end

GA -->|"execute, then fetch"| CA
GB -->|"execute, then fetch"| CB

CA -->|"future_into_py()"| WA
CB -->|"future_into_py()"| WB

WA -->|"Spawn Tokio task"| TA
WB -->|"Spawn Tokio task"| TB
```

```mermaid
flowchart LR

subgraph TK["Shared Tokio Runtime"]
    TA["Tokio Task A<br/>Runs TdsClient A"]
    TB["Tokio Task B<br/>Runs TdsClient B"]
    RE["Tokio Reactor<br/>epoll / IOCP"]
end

SA["SQL Server<br/>Connection A"]
SB["SQL Server<br/>Connection B"]
TA -->|"Send SQL"| SA
TB -->|"Send SQL"| SB
SA -.->|"Rows available"| RE
SB -.->|"Rows available"| RE
RE -.->|"Wake Task A"| TA
RE -.->|"Wake Task B"| TB
```

```mermaid
flowchart LR

subgraph TK["Tokio Runtime"]
    TA["Tokio Task A<br/>Parse rows"]
    TB["Tokio Task B<br/>Parse rows"]
end

subgraph BR["pyo3-async-runtimes"]
    WA["Python Awaitable A"]
    WB["Python Awaitable B"]
end

subgraph PC["mssql-py-core"]
    CA["Build Python objects<br/>Acquire GIL"]
    CB["Build Python objects<br/>Acquire GIL"]
end

subgraph PY["Python asyncio"]
    GA["Coroutine A resumes"]
    GB["Coroutine B resumes"]
end

TA -.->|"Fetched row writers"| CA
TB -.->|"Fetched row writers"| CB
CA -.->|"Complete fetch future"| WA
CB -.->|"Complete fetch future"| WB
WA -.->|"Resume coroutine with rows"| GA
WB -.->|"Resume coroutine with rows"| GB
```

**Follow the workflow path** (shown for Query A; Query B mirrors it):

1. **call** — the coroutine awaits `cursor.execute()` and subsequently a fetch operation, entering **pycore** for each.
2. **spawn** — pycore reads the SQL/params (**GIL held** briefly), then hands a Rust future to **pyo3-async-runtimes**, which spawns it as a Tokio task and returns an awaitable.
3. **run** — the task runs on a **Tokio worker thread**.
4. **send SQL** — the task writes the query to SQL Server over **socket A** (pure Rust, **no GIL**), then `await`s the reply and **frees the thread**.
5. **result boundary** — execution parses the first result boundary and resolves its awaitable, positioning the cursor.
6. **fetch** — the next awaitable reads rows; the reactor wakes its task when more bytes arrive.
7. **build objs** — pycore converts fetched row writers to Python objects on Tokio's blocking pool under `Python::attach`.
8. **resolve** — the fetch awaitable resolves with rows. The bridge does not implement row conversion itself.

**Why this is truly parallel:**
- A and B use **separate sockets** and **separate tasks** with no shared lock, so steps 3–6 **overlap**.
- The Python loop thread is **free during steps 3–6**, so it runs Query B while Query A is in flight — that's what `asyncio.gather(A.execute(), B.execute())` exploits.
- Execution and fetch complete independently on each connection; only the fetch awaitable returns rows.
- **Parallelism = number of connections** — the same rule as native Rust.

### 7.5 When is a thread assigned? When is a task assigned? When is the GIL held?

The mental rule: **each awaited operation has a future; a thread is borrowed only for the busy moments.**

- **Task — per operation.** Execution and fetching use separate bridged futures. Cursor/session state owns the query between them; conversion may also use blocking-pool tasks.
- **Thread — only intermittently.** A thread is borrowed only while real CPU work happens — building/sending the query bytes and parsing the reply rows. During the network wait (the long part), **no thread is assigned at all** — just the reactor watching the socket.

#### Native Rust — thread vs. task timeline

| Phase                       | Task assigned? | Thread assigned?                         |
| --------------------------- | -------------- | ---------------------------------------- |
| 1. Build & send query bytes | ✅ yes          | ✅ yes (a worker thread)                  |
| 2. **Network wait**         | ✅ yes          | ❌ **no thread** — reactor watches socket |
| 3. Parse reply rows         | ✅ yes          | ✅ yes (any free worker thread)           |
| 4. Done                     | freed          | freed                                    |

#### Python async — thread + task + GIL timeline

There are two execution domains: the Python event-loop thread and Tokio.
Network I/O runs on Tokio workers; Python row conversion runs on Tokio's
blocking pool. The following crossing applies to execution and fetching
separately; only a fetch operation returns rows.

| Phase                                             | Python coroutine    | Tokio task | Which thread is busy           | GIL                                  |
| ------------------------------------------------- | ------------------- | ---------- | ------------------------------ | ------------------------------------ |
| 1. Enter `execute()` or a fetch operation          | running             | not yet    | Python loop thread             | 🔒 **HELD** — read args, build future |
| 2. `future_into_py` returns awaitable             | suspended (`await`) | spawned    | Python loop thread → **freed** | 🔓 **RELEASED**                       |
| 3. Send query / read result bytes                 | pending             | running    | Tokio worker                   | 🔓 not held                           |
| 4. **Network wait**                               | pending             | parked     | **none**                       | 🔓 not held                           |
| 5. Parse result boundary / fetch Rust row writers | pending             | running    | Tokio worker                   | 🔓 not held                           |
| 6. Materialize metadata or fetched rows, complete future | resolving      | finishing  | Tokio blocking pool / bridge   | 🔒 **RE-ACQUIRED** for Python objects |
| 7. Coroutine resumes, returns                     | running             | done       | Python loop thread             | 🔒 **HELD**                           |

**Key insight:** native network I/O and TDS parsing do not hold the GIL.
Python-object input, metadata and fetched-row conversion do. Execution does not
convert a row collection before returning.

> **Note on step 6:** fetched-row conversion uses Tokio's blocking pool and `Python::attach`, not the Python event-loop thread. Building Python objects needs the GIL even when initiated in Rust. See [Section 7.6](#76-how-pyo3-bridges-the-schedulers-and-where-it-briefly-blocks).

#### GIL flow (Python)

```mermaid
sequenceDiagram
    participant PL as Python loop thread
    participant TW as Tokio worker thread
    participant SQL as SQL Server

    Note over PL: GIL HELD - read execute or fetch args
    PL->>TW: spawn Tokio task, return awaitable
    Note over PL: GIL RELEASED - loop runs other coroutines
    TW->>SQL: execute sends query, fetch reads results
    Note over TW: no GIL needed
    TW->>TW: await socket - thread freed, no GIL
    SQL-->>TW: result bytes ready
    TW->>TW: parse result boundary or fetched rows - no GIL
    Note over TW: fetch converts rows on blocking pool with GIL
    TW->>PL: complete the awaitable
    Note over PL: execution positions cursor, fetch returns rows
```

**One-line takeaways:**
- **In Rust:** a task lives the whole time; a thread is grabbed only to send bytes and parse rows, never during the wait.
- **In Python:** same, plus the query runs on a **Tokio worker thread — not** the Python loop thread, so the loop stays free.
- **GIL:** taken only at the in/out boundaries, released for the entire network round-trip — that release is what makes concurrency real.

### 7.6 How PyO3 bridges the schedulers (and where it briefly blocks)

PyO3 has to connect **two different concurrency worlds**: Python's single-threaded cooperative `asyncio` loop, and Tokio's multi-threaded work-stealing pool. Every crossing of that boundary is where the GIL and any momentary blocking occur.

#### The two schedulers PyO3 connects

```mermaid
flowchart LR
    subgraph PyWorld["Python side - ONE thread"]
        LOOP["asyncio event loop, cooperative scheduler"]
        FUT["asyncio.Future, the awaitable"]
    end
    subgraph Bridge["PyO3 + pyo3-async-runtimes"]
        SPAWN["future_into_py, spawns Rust future"]
        WAKE["completion callback, call_soon_threadsafe"]
    end
    subgraph TokWorld["Tokio side - MANY threads"]
        WORK["work-stealing worker pool"]
        REACT["reactor, epoll or IOCP"]
    end
    LOOP --> SPAWN --> WORK
    WORK --> REACT
    REACT --> WAKE --> FUT --> LOOP
```

#### Step-by-step: threads, tasks, and GIL at each crossing

This crossing occurs separately for execution and fetching. Execution publishes
cursor/result metadata; a fetch operation materializes rows.

```mermaid
sequenceDiagram
    participant PL as Python loop thread
    participant BR as PyO3 bridge
    participant TW as Tokio worker thread
    participant RE as Tokio reactor
    participant SQL as SQL Server

    Note over PL: GIL HELD, normal Python execution
    PL->>BR: call execute or fetch, read arguments
    Note over PL,BR: brief GIL-bound work, reading Python args into Rust
    BR->>TW: tokio spawn the Rust future
    BR-->>PL: return asyncio.Future, pending
    Note over PL: GIL RELEASED, loop runs other coroutines
    TW->>SQL: send query or read results, no GIL
    TW->>RE: await socket, worker thread released
    Note over RE: no thread, no GIL, just kernel polling
    SQL-->>RE: response bytes ready
    RE->>TW: wake task on any free worker
    TW->>TW: parse TDS tokens into Rust values, no GIL
    Note over TW: Python attachment for metadata or fetched-row conversion
    TW->>BR: operation complete with Python-convertible output
    Note over BR: BLOCK POINT 2, take loop lock, signal wakeup fd
    BR->>PL: schedule set_result on the loop
    Note over PL: loop resumes, execution positioned or fetch rows returned
```

#### Where does momentary blocking actually happen?

| #   | Point                                                         | Layer (control)             | Who blocks                 | On what                                                    | Cost (not measured here)      |
| --- | ------------------------------------------------------------- | --------------------------- | -------------------------- | ---------------------------------------------------------- | ----------------------------- |
| A   | **Enter execute/fetch** — read arguments from Python          | **PyO3** (Rust holding GIL) | Python loop thread         | Holds GIL while marshalling args into Rust                 | argument-size dependent       |
| B   | **Network round-trip**                                        | **Rust / Tokio**            | nobody                     | —                                                          | the whole query (no blocking) |
| C   | **TDS parsing (Rust values)**                                 | **Rust**                    | Tokio worker               | pure Rust, no GIL                                          | depends on rows               |
| D   | **`Python::attach`** for metadata or fetched rows              | **PyO3**                    | Tokio blocking-pool thread | Waits to access the Python object heap                     | GIL-contention dependent      |
| E   | **Fetch: materialize rows → Python objects in chunks**        | **PyO3**                    | whichever thread holds GIL | GIL held during each conversion chunk                     | scales with fetched rows      |
| F   | **`call_soon_threadsafe`**                                    | **PyO3** (bridge)           | completion thread          | Schedules the completion on the Python loop                | scheduler dependent           |
| G   | **Resume coroutine, return**                                  | **Python**                  | Python loop thread         | Holds GIL as normal                                        | microseconds                  |

Control starts in **PyO3** (A), crosses fully into **Rust/Tokio** for the expensive part (B–C), returns to **PyO3** at the boundary to build objects and wake the loop (D–F), and finally lands back in **Python** (G).

#### Two kinds of "blocking" — only one matters

| Kind                                      | What it is                                                        | Impact                                                                              |
| ----------------------------------------- | ----------------------------------------------------------------- | ----------------------------------------------------------------------------------- |
| **Network blocking** (the expensive one)  | Waiting on SQL Server                                             | ❌ **Never happens** — no thread, no GIL parked.                                     |
| **GIL boundary blocking**                | GIL acquire/release and Python-object work (points A, D, F, G)     | ✅ Can wait for contention and grows with conversion work.                           |

GIL contention and conversion can delay completion; this document does not
measure their latency. Native network waits do not park the Python loop thread.

#### The GIL is a lock on objects, not on a code layer

A common misconception is "the GIL is held only when a thread runs Python code." The real rule is broader:

> **Any thread must hold the GIL whenever it creates, reads, mutates, or changes the refcount of a Python object — even a native Rust/Tokio thread.**

| Activity                                               | Holds GIL? | Why                                                          |
| ------------------------------------------------------ | ---------- | ------------------------------------------------------------ |
| Running Python bytecode                                | ✅ Yes      | Touches Python objects                                       |
| **Rust code building a `PyList` / `PyLong` via PyO3**  | ✅ **Yes**  | Calls the CPython C-API to allocate objects + bump refcounts |
| Pure Rust logic (parse TDS bytes into `i64`, `String`) | ❌ No       | No Python object involved                                    |
| Network wait / socket I/O                              | ❌ No       | No Python object involved                                    |

So result materialization happens "in PyO3" because PyO3 is the only thing that can translate Rust values into Python objects — and that translation must hold the GIL, not because we are "in Python," but because **building Python objects touches the CPython object heap**:

```text
fetch TDS rows into Rust row writers       // no Python objects
submit a conversion chunk to spawn_blocking
    Python::attach: convert writers to Python tuples and append to the list
return the Python result through the awaitable bridge
```

This is conceptual pseudocode for the existing
[fetch materialization](../mssql-py-core/src/async_fetch.rs), not an `execute`
return type. The implementation already converts list results in chunks.

**Mental model:** the GIL is a lock on the **Python object heap**, not a lock on "Python code." Any thread — Python or Rust — that touches that heap must hold it.

#### The one place to watch: result materialization (point E)

This is the **only** GIL section whose cost scales with data size:

```mermaid
flowchart LR
    R1["Rust has parsed rows, no GIL"] --> R2["acquire GIL"]
    R2 --> R3["convert one chunk to Python rows - GIL HELD"]
    R3 --> R4["release GIL"]
```

- For a small result, negligible. For a **huge result set** (e.g. 1M rows), the conversion holds the GIL long enough to **momentarily stall other coroutines**, because only one thread can build Python objects at a time.
- This does **not** break concurrency of the *queries* (they already overlapped during the network phase) — it only **serializes the final row-materialization**.
- The existing API offers `fetchmany()`, and list materialization already converts in chunks. This reduces long conversion sections but is not a measured upper bound on GIL latency.

---

## 8. Sync vs. existing async in pycore

### 8.1 The synchronous model

`PyCoreConnection` and `PyCoreCursor` bridge with `runtime.block_on(...)` inside
`py.detach()`. Their synchronous calls block the calling Python thread. This
does not describe `PyAsyncConnection` or `PyAsyncCursor`. The DDBC/ODBC path is
also synchronous.

**Existing synchronous path:**

```mermaid
flowchart LR
    N1["cursor.execute"] --> N2["py.detach"]
    N2 --> N3["runtime.block_on - BLOCKS the Python thread"]
    N3 --> N4["execute completes, cursor positioned for fetch"]
```

**Existing async path:**

```mermaid
flowchart LR
    A1["await cursor.execute"] --> A2["future_into_py"]
    A2 --> A3["returns an awaitable immediately"]
    A3 --> A4["Tokio runs the query in background"]
    A4 --> A5["awaitable resolves, Python resumes"]
```

### 8.2 The existing async model

| Piece         | Synchronous Rust API                    | Existing async Rust API                                   |
| ------------- | --------------------------------------- | --------------------------------------------------------- |
| Tokio runtime | one `Runtime::new()` **per connection** | **one shared** multi-thread runtime for the whole process |
| Call style    | `runtime.block_on(...)`                 | `future_into_py(...)` returns an awaitable                |
| Python thread | **blocked** during query                | **free** — the loop keeps running                         |
| API           | `def execute`                           | `async def execute`                                       |

The async native methods return awaitables; the Python wrapper exposes awaited
connection/cursor operations. Pool integration must reuse this runtime and
surface, not replace either API or collapse execution and fetching into one call.

### 8.3 Invariants preserved by the async surface

1. **One shared async Tokio runtime.** `async_runtime::init` already registers it with the bridge, and async connections reuse it. Synchronous connections retain their own runtimes.
2. **GIL discipline.** Hold the GIL only at the **boundaries** — reading the query string in, and converting result rows out. During the network wait and TDS parsing, the GIL is **released**, so other Python coroutines run. The bridge acquires the GIL only when it needs to wake the awaitable.
3. **The statement handle.** A prepared-statement handle is just a server-side `i32` that SQL Server owns. Hold it as a plain integer field on the cursor and send it back on the next `sp_execute`, always on the **same connection** that created it. No thread affinity, no locking — the bridge never needs to touch it specially.

---

## 9. End-to-end walkthrough between Python and Rust

What happens on `await cursor.execute("SELECT ...")` followed by fetching in the existing async API:

```mermaid
sequenceDiagram
    participant App as Python app
    participant EL as asyncio loop
    participant Cur as pycore Cursor
    participant Bridge as pyo3-async-runtimes
    participant Tok as Tokio
    participant SQL as SQL Server

    App->>EL: await cursor.execute SELECT A
    EL->>Cur: call execute
    Cur->>Bridge: future_into_py wraps client.execute
    Bridge->>Tok: spawn the Rust future
    Bridge-->>EL: return Python awaitable, NOT done yet
    Note over EL: GIL RELEASED here - Python loop free, runs other coroutines Query B
    Tok->>SQL: write query packet - pure Rust, no GIL needed
    Tok->>Tok: await socket then Tokio thread freed
    SQL-->>Tok: first result boundary ready, reactor wakes task
    Tok->>Tok: parse result boundary and metadata
    Tok->>Bridge: future completed, result ready
    Bridge->>EL: wake the awaitable
    EL->>App: execute completes, cursor positioned for fetch
    App->>EL: await cursor.fetchall
    EL->>Cur: call fetchall
    Cur->>Bridge: wrap fetch future
    Bridge->>Tok: spawn fetch, read and convert rows
    Tok->>Bridge: fetch completed with Python rows
    Bridge->>EL: resolve fetch awaitable
    EL->>App: fetchall returns rows
```

**Step by step:**
1. `await cursor.execute()` calls into pycore. Instead of blocking, pycore wraps the Rust async work with `future_into_py(...)` and **immediately returns a Python awaitable**.
2. The actual query is **spawned onto the shared Tokio runtime**. The bridge links "this Python awaitable" to "this Tokio task."
3. **Python's asyncio loop is now free** — it runs your other coroutines (e.g., Query B on the other connection).
4. Tokio does the real work exactly as in the native-Rust case: send packet → yield on `.await` → reactor wakes it when SQL Server replies → parse rows. **No Python thread and no Tokio thread is parked during the network wait.**
5. The bridge wakes the execution awaitable when the cursor is positioned on a result. A subsequent fetch operation reads and converts rows; execution itself does not return a row collection.

---

## 10. Preview delivery and integration boundaries

### Existing namespace

- The preview Python entry point is `mssql_python.async_query.AsyncConnection`; the native module already exports its async classes.
- Extend that existing surface for approved pool integration. Do not introduce an `aio` namespace or a new Cargo feature based on this explanatory document.
- Runtime sharing does not make the sync API async or change sync pooling.

### Versioning & deprecation

- The existing async API is preview/unstable; this document does not promise a release version or stable signatures.
- Keep sync behavior unchanged when extending async internals. The async shared runtime is already implemented, not a pending consolidation of sync runtimes.
- No sync APIs are deprecated by this work. If that ever changes, follow a standard **deprecate → warn (one minor cycle) → remove (next major)** policy.

### Documentation & migration

- Keep async examples on the existing connect/execute/fetch surface. Concurrency scales with connections; the proposed async pool is not yet available through Python.
- Provide a **migration guide** for asyncio users currently using `run_in_executor`, showing the before/after.
- Document the **one-shared-runtime** model and the result-materialization caveat ([§7.6](#76-how-pyo3-bridges-the-schedulers-and-where-it-briefly-blocks)) for large result sets.
- Cross-link this design doc from the `mssql-py-core` README.

---

## 11. Appendix

Reference material on how the async model works underneath the Python bindings. The main design is in sections 1–10; this background explains the native-Rust foundation those sections build on.

### 11.1 How async works in native Rust (mssql-tds)

The core `TdsClient` in `mssql-tds` is **fully async, built on Tokio**. A native Rust app gets true async out of the box — no bridges, no blocking.

#### 11.1.1 The lifecycle of a query

One query moves through four phases. **Only phases 1 and 4 use a thread** — the long network wait in phase 3 uses no thread at all.

```mermaid
flowchart LR
S1["Phase 1 - RUN<br/>A worker thread runs the task,<br/>serializes the query, and writes<br/>the bytes to the socket"]
S2["Phase 2 - YIELD<br/>The task reaches .await on the<br/>socket read and returns the<br/>thread to the runtime"]
S3["Phase 3 - WAIT<br/>No thread is assigned.<br/>The Tokio reactor watches<br/>the socket for incoming data"]
S4["Phase 4 - RESUME<br/>Reply bytes arrive, the reactor<br/>wakes the task, and any free<br/>worker thread resumes it"]
S1 --> S2 --> S3 --> S4
```

**In words:**
1. **Who sends the query?** The task itself calls `client.execute()`, which serializes a `SqlBatch`/RPC and writes bytes to the socket. There is no separate "sender thread."
2. **Nobody is parked.** The instant the task hits `.await` on the socket read, it **releases the worker thread**. The thread goes and runs other tasks.
3. **Who receives the results?** The **Tokio reactor** (built on the OS's epoll/IOCP) watches the socket. When SQL Server sends bytes back, the reactor **wakes the matching task**, and *any free worker thread* resumes it to parse the token stream.
4. The query may be **sent by one thread and have its results parsed by a different thread** — that is normal and fine on the default multi-threaded runtime.

#### 11.1.2 Example Walkthrough: Two connections, two cursors (Async)

Each connection has its own socket and `TdsClient`. In the native example below,
each task owns its client without a shared mutex. The caller launches both
tasks; each sends its query and drains the response before completing.
The Tokio reactor watches both sockets and wakes the task whose bytes arrive.

```mermaid
sequenceDiagram
    participant App as async main
    participant TA as Task A Conn A
    participant TB as Task B Conn B
    participant Rt as Tokio Reactor
    participant SA as SQL Server socket A
    participant SB as SQL Server socket B

    App->>TA: spawn execute SELECT A
    App->>TB: spawn execute SELECT B
    TA->>SA: write query packet
    TA->>Rt: await read then yield thread
    TB->>SB: write query packet
    TB->>Rt: await read then yield thread
    Note over Rt: Both tasks waiting, threads are FREE
    SA-->>Rt: rows ready socket A
    Rt->>TA: wake, free thread resumes
    TA-->>App: batch A drained, task completes
    SB-->>Rt: rows ready socket B
    Rt->>TB: wake, free thread resumes
    TB-->>App: batch B drained, task completes
```

Both connections run genuinely in parallel — separate sockets, separate tasks — even on a single-threaded runtime, because the *waiting* is overlapped.

**What the participants are:**

| Participant                 | What it actually is                                                                                                                                                                                                      |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Caller**                  | Runs on Tokio, launches both queries with `tokio::spawn` and waits for both (`tokio::join!`). |
| **Task A**                  | Owns Connection A, sends its query, awaits the response and drains the batch. A unit of work, not a thread. |
| **Task B**                  | The same for the second query on **Connection B** — an independent future. A and B share no lock, so they run concurrently.                                                                                              |
| **Tokio Reactor**           | The runtime's I/O watcher, built on the OS's `epoll` (Linux) / `IOCP` (Windows). It monitors the sockets and **wakes the specific task** when its bytes arrive — this is what lets a task sleep with no thread assigned. |
| **SQL Server socket A / B** | The two **TCP connections** to SQL Server, one per `TdsClient`. Separate byte pipes, so queries A and B travel and return in parallel.                                                                                   |

#### 11.1.3 What the Rust app looks like

This completion-only function accepts two already connected clients. Each task
owns its client, applies a 30-second execution timeout and drains the batch,
discarding rows. Row-returning applications fetch rows before draining instead.

```rust
use mssql_tds::connection::tds_client::{ExecuteOptions, TdsClient};

async fn run_two_queries(
    mut conn_a: TdsClient,
    mut conn_b: TdsClient,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let a = tokio::spawn(async move {
        conn_a
            .execute(
                "SELECT 1 AS value".into(),
                ExecuteOptions {
                    timeout: Some(30),
                    ..Default::default()
                },
            )
            .await?;
        conn_a.close_query().await
    });
    let b = tokio::spawn(async move {
        conn_b
            .execute(
                "SELECT 2 AS value".into(),
                ExecuteOptions {
                    timeout: Some(30),
                    ..Default::default()
                },
            )
            .await?;
        conn_b.close_query().await
    });

    let (ra, rb) = tokio::join!(a, b);
    ra??;
    rb??;
    Ok(())
}
```

### 11.2 Concurrency model: connections vs. cursors

Each Python async connection has operation claims guarding its client mutex.
Independent connections can overlap; cursors sharing a connection must be
sequenced by the caller.

```mermaid
flowchart TB
    subgraph ConnA["Connection A"]
        MA["Logical connection A<br/>operation claims + client mutex"]
        CA["Cursor A1"]
        CA --> MA
    end
    subgraph ConnB["Connection B"]
        MB["Logical connection B<br/>operation claims + client mutex"]
        CB["Cursor B1"]
        CB --> MB
    end
    MA -. socket A .-> NET["Network and SQL Server"]
    MB -. socket B .-> NET
```

- Operation claims reject a second cursor's execution **before client-mutex access** while another cursor owns an active operation or batch. The Python async API raises `RuntimeError` rather than queueing it. Callers must finish active work and exhaust its batch or close its cursor before another cursor executes.
- The client mutex protects admitted I/O within one connection; it is not an automatic cursor queue. MARS (Multiple Active Result Sets) is parsed at login but **not yet implemented**.
- **Across connections there is NO shared lock.** A and B never wait on each other.
- **Parallelism scales with the number of connections, NOT the number of cursors.**

| Scenario                                           | Result                                                                                                          |
| -------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| **One cursor per connection** (2 conns, 2 cursors) | ✅ Zero contention — fully parallel                                                                              |
| **Two cursors on one connection**                  | ⚠️ Caller-managed sequencing — execution raises `RuntimeError` while another cursor owns the active operation or batch; no automatic waiting |
| **N independent connections**                      | ✅ N-way parallelism                                                                                             |

> **Rule of thumb:** Want *N* parallel queries → use *N* connections (a connection pool).

---

*This document describes the existing async architecture of `mssql-tds` and `mssql-py-core`. Python/AQE pool integration remains a separate implementation slice.*
