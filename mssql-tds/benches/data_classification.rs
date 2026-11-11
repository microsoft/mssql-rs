// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DATACLASSIFICATION (`0xA3`) token parse microbenchmarks.
//!
//! Each group parses one token shape with the crate's parser in two ways:
//!
//! - **unit_by_unit** reads every UTF-16 unit of a label or information-type
//!   string on its own, then decodes. Every string took this path before the
//!   parser learned to take resident runs whole, and a string that straddles
//!   two packets still does.
//! - **resident** hands the parser each string's bytes in one run, as the
//!   network reader does inside a received packet, and decodes them in place.
//!
//! [`check`] asserts both produce the same metadata before anything is timed.
//! The token is parsed once per result set, never per row, so this measures a
//! one-off cost that grows with the number of distinct classifications.
//!
//! Run with `cargo bench --bench data_classification`.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds::test_client_support::parse_data_classification;
use mssql_tds::token::data_classification::{
    DataClassificationMetadata, DataClassificationVersion,
};
use std::hint::black_box;

const VERSION: DataClassificationVersion = DataClassificationVersion::Version2;

fn b_varchar(bytes: &mut Vec<u8>, value: &str) {
    let units: Vec<u16> = value.encode_utf16().collect();
    bytes.push(u8::try_from(units.len()).unwrap());
    units
        .iter()
        .for_each(|unit| bytes.extend_from_slice(&unit.to_le_bytes()));
}

/// A V2 payload with `distinct` labels and as many information types, every
/// name and id `length` ASCII characters, and `columns` columns with one
/// property each, column `i` pointing at entry `i % distinct`.
fn token(columns: usize, distinct: usize, length: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for kind in ["L", "T"] {
        bytes.extend_from_slice(&u16::try_from(distinct).unwrap().to_le_bytes());
        for index in 0..distinct {
            let prefix = format!("{kind}{index:05}");
            let value = format!("{prefix}{}", "x".repeat(length - prefix.len()));
            b_varchar(&mut bytes, &value);
            b_varchar(&mut bytes, &value);
        }
    }
    bytes.extend_from_slice(&30i32.to_le_bytes());
    bytes.extend_from_slice(&u16::try_from(columns).unwrap().to_le_bytes());
    for column in 0..columns {
        let entry = u16::try_from(column % distinct).unwrap().to_le_bytes();
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&entry);
        bytes.extend_from_slice(&entry);
        bytes.extend_from_slice(&30i32.to_le_bytes());
    }
    bytes
}

fn parse(
    runtime: &tokio::runtime::Runtime,
    bytes: &[u8],
    resident: bool,
) -> DataClassificationMetadata {
    runtime
        .block_on(parse_data_classification(bytes, VERSION, resident))
        .unwrap()
}

fn check(runtime: &tokio::runtime::Runtime, bytes: &[u8], columns: usize) {
    let unit_by_unit = parse(runtime, bytes, false);
    assert_eq!(unit_by_unit.columns().len(), columns);
    assert_eq!(parse(runtime, bytes, true), unit_by_unit);
}

fn bench_parse(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    // (name, columns, distinct classifications, string length)
    let shapes = [
        // A handful of classified columns sharing one label.
        ("typical", 3, 1, 12),
        // 252 distinctly classified columns with sysname-length strings: the
        // largest shape measured against SQL Server 2022.
        ("distinct_252", 252, 252, 128),
        // The same at the 4096-column result-set maximum.
        ("distinct_4096", 4096, 4096, 128),
        // Every column shares one classification, so strings are a small
        // part of the token and this change should not matter.
        ("shared_4096", 4096, 1, 128),
    ];
    for (name, columns, distinct, length) in shapes {
        let bytes = token(columns, distinct, length);
        check(&runtime, &bytes, columns);

        let mut group = c.benchmark_group(format!("data_classification_parse/{name}"));
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        if bytes.len() > 1024 * 1024 {
            group.sample_size(20);
        }
        group.bench_function("unit_by_unit", |b| {
            b.iter(|| black_box(parse(&runtime, black_box(&bytes), false)))
        });
        group.bench_function("resident", |b| {
            b.iter(|| black_box(parse(&runtime, black_box(&bytes), true)))
        });
        group.finish();
    }
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
