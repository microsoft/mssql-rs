# Arrow-Direct TDS Serialization
## Slide Deck Narrative

---

## Slide 1 — The Problem

**Bulk copying data into SQL Server is slower than it needs to be.**

When an application holds data in Apache Arrow format — the industry-standard
columnar memory layout used by Pandas, Spark, Polars, and DuckDB — our current
driver converts every value into an intermediate Rust representation before
serializing it onto the wire.

That intermediate step costs time and memory we don't need to spend.

---

## Slide 2 — What Happens Today

```
Arrow RecordBatch (columnar, contiguous memory)
        │
        ▼
  ┌─────────────────────────────────┐
  │  Convert each value per row:    │
  │    i32  → ColumnValues::Int     │  ← enum construction
  │    &str → Vec<u8> → SqlString   │  ← heap allocation per row
  │    i128 → Vec<i32> → Decimal    │  ← heap allocation per row
  │                                 │
  │  Build Vec<ColumnValues> / row  │  ← heap allocation per row
  └────────────┬────────────────────┘
               │
               ▼
  ┌─────────────────────────────────┐
  │  Serialize each value to TDS:   │
  │    match on enum variant        │  ← branch per value
  │    call serializer per value    │  ← async overhead × 5 columns
  └────────────┬────────────────────┘
               │
               ▼
         SQL Server
```

**Three heap allocations per row. Five async serializer calls per row.
For 1M rows, that's 3M allocations and 5M async dispatches.**

---

## Slide 3 — The Insight

Arrow stores data in typed, contiguous column buffers. A column of 100K
integers is a single contiguous block of 400KB in memory.

TDS (SQL Server's wire protocol) also expects typed, length-prefixed values.

**We can go directly from one to the other.**

An `Int32Array` of 100K values can be serialized to TDS wire bytes in a
tight loop — no intermediate objects, no enum wrapping, no per-value
function calls. One pass over the column buffer, one pass of output bytes.

---

## Slide 4 — What We Built

```
Arrow RecordBatch (columnar, contiguous memory)
        │
        ▼
  ┌─────────────────────────────────┐
  │  Tight loop per column type:    │
  │    Int32Array  → [0x04][LE]     │  ← direct memory read
  │    Int64Array  → [0x08][LE]     │  ← no conversion
  │    Utf8Array   → [len][UTF-16]  │  ← inline transcode
  │    Decimal128  → [0x09][s][LE]  │  ← bit shift only
  │                                 │
  │  Output: TDS bytes per row      │
  └────────────┬────────────────────┘
               │
               ▼
  ┌─────────────────────────────────┐
  │  Single bulk write per row      │
  │  directly to packet buffer      │  ← one call, not five
  └────────────┬────────────────────┘
               │
               ▼
         SQL Server
```

**Zero per-row heap allocations. One write call per row.
Columnar iteration is cache-friendly.**

---

## Slide 5 — The Results

Benchmark: 100,000 rows × 10 iterations against SQL Server 2022.
Schema: INT, BIGINT, FLOAT, NVARCHAR(200), DECIMAL(18,2).

```
                      Throughput (thousand rows/sec)
              0       100      200      300      400
              ├────────┼────────┼────────┼────────┤

 Current      ██████████████████░░░░░░░░░░░░░░░░░░  174 Krows/s
 (materialized)

 Arrow-direct █████████████████████████████████████  363 Krows/s
 (pre-serialized)
```

| Metric | Current Path | Arrow-Direct |
|--------|-------------|--------------|
| Avg latency (100K rows) | 577 ms | 277 ms |
| Throughput | 174 Krows/s | 363 Krows/s |
| **Speedup** | — | **2.09×** |
| Heap allocs per row | 3 | 0 |
| Async calls per row | 5+ | 1 |

---

## Slide 6 — Why It's 2× Faster

| Overhead eliminated | Impact |
|---|---|
| `Vec<ColumnValues>` per row | No per-row heap allocation |
| `SqlString` UTF-16 buffer per string | No intermediate byte vector |
| `DecimalParts` int_parts Vec per decimal | No decomposition allocations |
| Enum match per column value | No branch dispatch |
| 5 separate async serialize calls per row | Single bulk write |
| Row-major value extraction | Cache-friendly columnar loops |

The two biggest wins:
1. **Eliminating allocations** — 3 heap allocs/row × 100K rows = 300K allocs saved
2. **Batching writes** — one `write_raw_bytes` per row vs 5+ `serialize_*` calls

---

## Slide 7 — We Also Tested Zero-Alloc Streaming

We explored a third approach: writing TDS bytes directly into the packet
buffer as we decode each Arrow value — no intermediate buffer at all.

| Approach | Speedup | Throughput |
|---|---|---|
| Current (materialized) | 1.0× | 174 Krows/s |
| Streaming (zero-alloc) | 1.5× | 255 Krows/s |
| Pre-serialized | **2.1×** | **363 Krows/s** |

Streaming eliminates all allocation but loses on small per-code-unit writes
(especially for UTF-8→UTF-16 transcoding) and row-major iteration patterns.
Pre-serialization wins because columnar passes + bulk memcpy > many small writes.

---

## Slide 8 — What This Means at Scale

| Rows | Current Path | Arrow-Direct | Time Saved |
|------|-------------|-------------|-----------|
| 100K | 577 ms | 277 ms | 300 ms |
| 1M | ~5.8 s | ~2.8 s | ~3.0 s |
| 10M | ~58 s | ~28 s | ~30 s |
| 100M | ~9.7 min | ~4.6 min | ~5.1 min |

For ETL pipelines and data warehouse loads, this is the difference between
meeting and missing an SLA window.

---

## Slide 9 — A Bug We Found Along the Way

During this work we discovered and fixed a latent bug in the NVARCHAR
serializer: it was writing the **character count** as the length prefix
instead of the **byte count**.

- TDS spec requires byte count (chars × 2 for UTF-16)
- SQL Server returned error 4895: *"Unicode data is odd byte size"*
- This bug would have affected any NVARCHAR column with non-ASCII data
- Fixed in `serialize_string_utf16`

**This kind of bug is exactly what you find when you get close to the wire format.**

---

## Slide 10 — Path Forward

**Near-term:**
- Integrate `pre_serialize_arrow_to_tds` as a first-class API
  (`ArrowBulkLoadRow`) in the driver
- Extend to all TDS types (Date, Time, DateTime2, UUID, Binary)

**Medium-term:**
- Multi-row pre-serialization (batch multiple rows per write call)
- Expose via Python and Node.js FFI bindings — Arrow-native callers
  get the benefit automatically

**Long-term:**
- Arrow IPC → TDS transcoding for cross-process/cross-network scenarios
- Vectorized encoding (SIMD for UTF-8→UTF-16 transcoding)

---

## Slide 11 — Key Takeaway

> When your input is already columnar and your output is a typed wire
> format, the fastest path is a direct translation — no intermediate
> object model, no per-value dispatch, no per-value allocation.
>
> **Arrow → TDS direct serialization: 2× throughput, 0 allocations per row.**
