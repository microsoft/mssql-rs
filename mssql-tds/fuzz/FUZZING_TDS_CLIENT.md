# TdsClient Fuzzing

## Overview

This directory contains fuzz targets for the mssql-tds library, specifically targeting the `TdsClient` component.

## Fuzz Targets

### fuzz_tds_client

Tests the TdsClient with simulated SQL Server responses without requiring an actual SQL Server connection.

**What it tests:**
- Query execution (`execute()`)
- Query execution followed by response draining (`close_query()`)
- Parameterized execution (`execute_sp_executesql()`)
- Stored-procedure execution (`execute_stored_procedure()`)
- State management across operations
- Error handling with malformed tokens

**Input Format:**
```
Byte 0: Scenario selector (0-3)
  0 = execute() with token response
  1 = execute() followed by close_query()
  2 = execute_sp_executesql()
  3 = execute_stored_procedure()
  
Bytes 1+: Token stream data (TDS protocol tokens)
```

## Running Fuzz Tests

### Prerequisites

```bash
# Install cargo-fuzz
cargo install cargo-fuzz

# Install nightly toolchain
rustup install nightly
```

### Run fuzzing

Run from `mssql-tds/fuzz`:

```bash
# Fuzz TdsClient (recommended - tests client logic)
RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_tds_client

# Fuzz TokenStreamReader (already implemented)
RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_token_stream

# Run with specific timeout (e.g., 1 hour)
RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_tds_client -- -max_total_time=3600

# Run with custom corpus directory
RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_tds_client corpus/fuzz_tds_client
```

### View coverage

```bash
# Generate coverage report
RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz coverage fuzz_tds_client

# View coverage with llvm-cov
llvm-cov show target/*/release/fuzz_tds_client \
    --format=html \
    --instr-profile=fuzz/coverage/fuzz_tds_client/coverage.profdata \
    > coverage.html
```

### Minimize corpus

```bash
# Minimize the corpus to remove redundant test cases
RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz cmin fuzz_tds_client
```

### Triage crashes

```bash
# If fuzzing finds a crash, reproduce it
RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_tds_client \
    fuzz/artifacts/fuzz_tds_client/crash-<hash>
```

## Architecture

### Refactoring for Fuzzing

To enable fuzzing without SQL Server connections, we introduced the `TdsTransport` trait:

```rust
pub(crate) trait TdsTransport: TdsTokenStreamReader + Send + Sync + Debug {
    fn as_writer(&mut self) -> &mut dyn NetworkWriter;
    fn reset_reader(&mut self);
    fn packet_size(&self) -> u32;
    async fn close_transport(&mut self) -> TdsResult<()>;
}
```

**Implementations:**
- `NetworkTransport` - Real network I/O (production)
- `MockTransport` - Simulated I/O (fuzzing)

### Mock Infrastructure

**MockWriter:**
- Captures all write operations
- No actual network sends
- Returns success for all writes

**MockTransport:**
- Reads tokens from fuzzer-provided bytes
- Uses `TokenStreamReader` internally
- Provides mock writer for packet writing

## Creating Test Corpus

### Manual Corpus Files

Create binary files in `corpus/fuzz_tds_client/`:

```bash
# Simple DONE token (scenario 0)
printf '\x00\xfd\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00' > corpus/fuzz_tds_client/done_token

# Column metadata + row (scenario 1)
# [scenario][0x81=ColMetadata][columns...][0xd1=Row][values...]
```

### Token Types Reference

Common TDS token types:
- `0x81` - COLMETADATA
- `0xD1` - ROW
- `0xD2` - NBCROW
- `0xFD` - DONE
- `0xFE` - DONEPROC
- `0xFF` - DONEINPROC
- `0xAA` - ERROR
- `0xAB` - INFO
- `0xE3` - ENVCHANGE
- `0xAD` - LOGINACK

## Best Practices

1. **Start small:** Use small corpus files (< 1KB)
2. **Run continuously:** Let fuzzer run overnight for best results
3. **Monitor memory:** Inputs over 2048 bytes are rejected; this is not a bound on total process memory
4. **Triage promptly:** Investigate crashes as soon as they're found
5. **Minimize corpus:** Run `cmin` periodically to keep corpus efficient

## CI Integration

```bash
# Add to CI pipeline (e.g., GitHub Actions)
- name: Run fuzz tests
  run: |
    RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_tds_client -- \
      -max_total_time=300 \
      -rss_limit_mb=2048
```

## Troubleshooting

### "No fuzz targets found"
- Ensure you're in the `fuzz` directory
- Check `Cargo.toml` has correct `[[bin]]` entries

### "Out of memory"
- Reduce corpus size
- Check for allocation bugs; input size alone does not bound allocations

### "Timeout"
- Limit input size (max 2048 bytes in fuzzer)
- Check for infinite loops in token parsing

## References

- [cargo-fuzz documentation](https://rust-fuzz.github.io/book/cargo-fuzz.html)
- [LibFuzzer documentation](https://llvm.org/docs/LibFuzzer.html)
- [TDS Protocol Specification](https://docs.microsoft.com/en-us/openspecs/windows_protocols/ms-tds/)
