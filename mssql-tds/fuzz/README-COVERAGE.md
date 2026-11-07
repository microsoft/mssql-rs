# Fuzz Coverage Report

This directory contains tools for running fuzz tests with coverage analysis.

## Quick Start

### Generate Combined Coverage Report

```bash
cd mssql-tds/fuzz
./run-fuzz-coverage.sh
```

This will:
1. Run coverage analysis for both `fuzz_token_stream` and `fuzz_tds_client`
2. Generate individual HTML reports for each target
3. Create a combined coverage report merging both targets

### View Results (If running with linux UI, else navigate to these folders and get them on Windows)

```bash
# View combined coverage (recommended)
xdg-open coverage-reports/combined/index.html

# View individual target coverage
xdg-open coverage-reports/fuzz_token_stream/index.html
xdg-open coverage-reports/fuzz_tds_client/index.html
```

## Corpus Files

Minimized corpus files are compressed and ready for CI/CD pipelines:

- `corpus-fuzz_token_stream.tar.gz` (~6KB, 126 files)
- `corpus-fuzz_tds_client.tar.gz` (~5.4KB, 121 files)

### Extract Corpus (for CI/CD)

```bash
tar -xzf corpus-fuzz_token_stream.tar.gz
tar -xzf corpus-fuzz_tds_client.tar.gz
```

## Running Individual Fuzz Targets

```bash
# Run with nightly toolchain (required for fuzzing)
RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_token_stream -- -max_total_time=60
RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_tds_client -- -max_total_time=60
```

## Minimizing Corpus

After running fuzzing, minimize the corpus to reduce file count:

```bash
RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz cmin fuzz_token_stream
RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz cmin fuzz_tds_client
```

Then compress for distribution:

```bash
tar -czf corpus-fuzz_token_stream.tar.gz corpus/fuzz_token_stream/
tar -czf corpus-fuzz_tds_client.tar.gz corpus/fuzz_tds_client/
```

## Requirements

- Rust nightly toolchain
- cargo-fuzz: `cargo +nightly install cargo-fuzz`
- LLVM tools: `rustup component add llvm-tools --toolchain nightly`

## Notes

- The fuzz directory uses its own workspace (separate from parent)
- Always use `+nightly` when running cargo-fuzz commands
- Coverage reports are ignored by git (see .gitignore)
- Corpus archives are checked into git for CI/CD use
