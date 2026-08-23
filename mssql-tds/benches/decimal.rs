// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `decimal` / `numeric` decode and format microbenchmarks.
//!
//! These isolate the two per-value costs that a consumer streaming a wide
//! numeric result set pays on every row, with no server or socket involved:
//!
//! - **decode** — turning the sign byte plus 4/8/12/16 little-endian magnitude
//!   bytes into a [`DecimalParts`].
//! - **format** — rendering that value as decimal text.
//!
//! Each group times the shape this crate used to have against the shape it has
//! now. The `previous` variants are reproduced here rather than imported,
//! because they no longer exist in the crate; [`check`] asserts every shape
//! agrees byte for byte before anything is timed, so a drifted copy fails
//! loudly instead of flattering the new code.
//!
//! Run with `cargo bench --bench decimal`.

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds::datatypes::decoder::{DECIMAL_STR_LEN, DecimalParts};
use std::hint::black_box;

/// A decoded value in the previous representation: the magnitude as a heap
/// `Vec` of little-endian 32-bit words.
#[derive(Clone)]
struct PreviousParts {
    is_positive: bool,
    scale: u8,
    #[allow(dead_code)]
    precision: u8,
    int_parts: Vec<i32>,
}

impl PreviousParts {
    fn magnitude(&self) -> u128 {
        self.int_parts
            .iter()
            .enumerate()
            .fold(0u128, |acc, (i, &part)| {
                acc | ((part as u32 as u128) << (i * 32))
            })
    }

    /// The previous rendering: `u128::to_string`, then splice in the point,
    /// then prepend the sign. Three allocations in the common signed case.
    fn to_decimal_string(&self) -> String {
        let value_str = self.magnitude().to_string();

        let result = if self.scale == 0 {
            value_str
        } else {
            let scale_pos = self.scale as usize;
            if value_str.len() <= scale_pos {
                format!("0.{}{}", "0".repeat(scale_pos - value_str.len()), value_str)
            } else {
                let split_pos = value_str.len() - scale_pos;
                format!("{}.{}", &value_str[..split_pos], &value_str[split_pos..])
            }
        };

        if self.is_positive {
            result
        } else {
            format!("-{result}")
        }
    }
}

/// The previous decode: a zeroed `Vec<u8>` staging buffer, then a second `Vec`
/// for the words. Two allocations to move at most 16 bytes.
fn decode_previous(buf: &[u8], at: &mut usize, precision: u8, scale: u8) -> Option<PreviousParts> {
    let length = buf[*at] as usize;
    *at += 1;
    if length == 0 {
        return None;
    }
    let is_positive = buf[*at] == 1;
    *at += 1;

    let magnitude_len = length - 1;
    let mut magnitude = vec![0u8; magnitude_len];
    magnitude.copy_from_slice(&buf[*at..*at + magnitude_len]);
    *at += magnitude_len;

    let int_parts: Vec<i32> = magnitude
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    Some(PreviousParts {
        is_positive,
        scale,
        precision,
        int_parts,
    })
}

/// The current decode: a stack array and one `u128::from_le_bytes`.
fn decode_current(buf: &[u8], at: &mut usize, precision: u8, scale: u8) -> Option<DecimalParts> {
    let length = buf[*at] as usize;
    *at += 1;
    if length == 0 {
        return None;
    }
    let is_positive = buf[*at] == 1;
    *at += 1;

    let magnitude_len = length - 1;
    let mut magnitude = [0u8; 16];
    magnitude[..magnitude_len].copy_from_slice(&buf[*at..*at + magnitude_len]);
    *at += magnitude_len;

    Some(DecimalParts::new(
        is_positive,
        precision,
        scale,
        u128::from_le_bytes(magnitude),
    ))
}

#[derive(Clone, Copy)]
struct Column {
    precision: u8,
    scale: u8,
}

/// Appends one wire value: length byte, sign byte, little-endian magnitude at
/// the width the declared precision implies.
fn encode(buf: &mut Vec<u8>, magnitude: u128, is_positive: bool, precision: u8) {
    let width = match precision {
        1..=9 => 4usize,
        10..=19 => 8,
        20..=28 => 12,
        _ => 16,
    };
    buf.push((width + 1) as u8);
    buf.push(u8::from(is_positive));
    buf.extend_from_slice(&magnitude.to_le_bytes()[..width]);
}

/// The four `numeric` columns of the foreign-data-wrapper workload this change
/// came from:
///
/// ```sql
/// CAST((n % 10000000) * 0.01  AS decimal(15,2)),
/// CAST((n % 1000000) * -0.01  AS decimal(15,2)),
/// CAST((n % 100000)  * 0.001  AS decimal(18,3)),
/// CAST((n % 10000)   * 0.0001 AS decimal(19,4))
/// ```
fn workload_fdw(rows: u128) -> (Vec<u8>, Vec<Column>) {
    let mut buf = Vec::new();
    let mut cols = Vec::new();
    for n in 1..=rows {
        encode(&mut buf, n % 10_000_000, true, 15);
        encode(&mut buf, n % 1_000_000, false, 15);
        encode(&mut buf, n % 100_000, true, 18);
        encode(&mut buf, n % 10_000, true, 19);
        cols.extend_from_slice(&[
            Column {
                precision: 15,
                scale: 2,
            },
            Column {
                precision: 15,
                scale: 2,
            },
            Column {
                precision: 18,
                scale: 3,
            },
            Column {
                precision: 19,
                scale: 4,
            },
        ]);
    }
    (buf, cols)
}

/// The four-word path: `decimal(38,10)` with a magnitude just under `10^38`,
/// the true maximum for precision 38.
///
/// The scale is deliberately not a multiple of 4. That is the combination that
/// catches a base-10000 encoder which pre-multiplies the magnitude by `10^pad`:
/// `10^38 * 100` overflows a `u128` and wraps silently in release builds.
fn workload_wide(rows: u128) -> (Vec<u8>, Vec<Column>) {
    let mut buf = Vec::new();
    let mut cols = Vec::new();
    for n in 1..=rows {
        let magnitude = 99_999_999_999_999_999_999_999_999_999_999_999_999u128 - n;
        encode(&mut buf, magnitude, n % 2 == 0, 38);
        cols.push(Column {
            precision: 38,
            scale: 10,
        });
    }
    (buf, cols)
}

/// Every shape must agree, and must consume the buffer exactly, before any of
/// them is timed.
fn check(buf: &[u8], cols: &[Column]) {
    let (mut a, mut b) = (0usize, 0usize);
    for col in cols {
        let previous = decode_previous(buf, &mut a, col.precision, col.scale).unwrap();
        let current = decode_current(buf, &mut b, col.precision, col.scale).unwrap();

        assert_eq!(previous.magnitude(), current.magnitude(), "magnitude");
        assert_eq!(previous.is_positive, current.is_positive, "sign");

        let expected = previous.to_decimal_string();
        let mut scratch = [0u8; DECIMAL_STR_LEN];
        assert_eq!(expected, current.format_into(&mut scratch), "format_into");
        assert_eq!(expected, current.to_decimal_string(), "to_decimal_string");
        assert_eq!(expected, current.to_string(), "Display");
    }
    assert_eq!(a, buf.len());
    assert_eq!(b, buf.len());
}

const ROWS: u128 = 4096;

fn bench_decode(c: &mut Criterion) {
    for (name, (buf, cols)) in [("fdw", workload_fdw(ROWS)), ("wide", workload_wide(ROWS))] {
        check(&buf, &cols);

        let mut group = c.benchmark_group(format!("decimal_decode/{name}"));
        group.throughput(Throughput::Elements(cols.len() as u64));

        group.bench_function("previous_two_allocations", |b| {
            b.iter(|| {
                let mut at = 0usize;
                for col in &cols {
                    black_box(decode_previous(&buf, &mut at, col.precision, col.scale));
                }
                at
            })
        });

        group.bench_function("current_stack_array", |b| {
            b.iter(|| {
                let mut at = 0usize;
                for col in &cols {
                    black_box(decode_current(&buf, &mut at, col.precision, col.scale));
                }
                at
            })
        });

        group.finish();
    }
}

fn bench_format(c: &mut Criterion) {
    for (name, (buf, cols)) in [("fdw", workload_fdw(ROWS)), ("wide", workload_wide(ROWS))] {
        check(&buf, &cols);

        let mut previous = Vec::with_capacity(cols.len());
        let mut current = Vec::with_capacity(cols.len());
        let (mut a, mut b) = (0usize, 0usize);
        for col in &cols {
            previous.push(decode_previous(&buf, &mut a, col.precision, col.scale).unwrap());
            current.push(decode_current(&buf, &mut b, col.precision, col.scale).unwrap());
        }

        let mut group = c.benchmark_group(format!("decimal_format/{name}"));
        group.throughput(Throughput::Elements(cols.len() as u64));

        group.bench_function("previous_to_string_and_splice", |b| {
            b.iter(|| {
                for value in &previous {
                    black_box(value.to_decimal_string());
                }
            })
        });

        group.bench_function("current_to_decimal_string", |b| {
            b.iter(|| {
                for value in &current {
                    black_box(value.to_decimal_string());
                }
            })
        });

        group.bench_function("current_display", |b| {
            b.iter(|| {
                for value in &current {
                    black_box(value.to_string());
                }
            })
        });

        // The allocation-free path: one caller-owned buffer for the whole run.
        group.bench_function("current_format_into", |b| {
            b.iter_batched_ref(
                || [0u8; DECIMAL_STR_LEN],
                |scratch| {
                    for value in &current {
                        black_box(value.format_into(scratch));
                    }
                },
                BatchSize::SmallInput,
            )
        });

        group.finish();
    }
}

criterion_group!(benches, bench_decode, bench_format);
criterion_main!(benches);
