// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end NBCROW row-decode throughput against `mssql-mock-tds`.
//!
//! The mock pre-serializes each response before the timed loop, so the measured
//! path is query dispatch, loopback transport, and complete client row decoding.

use std::env;
use std::net::SocketAddr;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mssql_mock_tds::{
    ColumnDefinition, ColumnValue, MockTdsServer, QueryResponse, Row, SqlDataType,
};
use mssql_tds::{
    connection::{client_context::ClientContext, tds_client::TdsClient},
    connection_provider::tds_connection_provider::TdsConnectionProvider,
    core::{EncryptionOptions, EncryptionSetting},
};
use mssql_tds_bench::{criterion_config, drain, runtime};
use tokio::sync::oneshot;

const QUERY: &str = "SELECT nbcrow_benchmark";
const NULL_PERCENT: usize = 25;

#[derive(Clone, Copy)]
enum RowShape {
    FixedWidth,
    TextHeavy,
    Mixed,
}

impl RowShape {
    fn name(self) -> &'static str {
        match self {
            Self::FixedWidth => "fixed_width",
            Self::TextHeavy => "text_heavy",
            Self::Mixed => "mixed",
        }
    }

    fn data_type(self, column: usize) -> SqlDataType {
        match self {
            Self::FixedWidth => {
                if column.is_multiple_of(2) {
                    SqlDataType::Int
                } else {
                    SqlDataType::BigInt
                }
            }
            Self::TextHeavy => SqlDataType::NVarChar,
            Self::Mixed => match column % 4 {
                0 | 3 => SqlDataType::Int,
                1 => SqlDataType::BigInt,
                2 => SqlDataType::NVarChar,
                _ => unreachable!(),
            },
        }
    }
}

fn response(shape: RowShape, column_count: usize, row_count: usize) -> QueryResponse {
    let columns: Vec<_> = (0..column_count)
        .map(|column| ColumnDefinition::new(format!("c{column}"), shape.data_type(column)))
        .collect();
    let rows = (0..row_count)
        .map(|row| {
            let values = (0..column_count)
                .map(|column| {
                    if (row + column).is_multiple_of(100 / NULL_PERCENT) {
                        return ColumnValue::Null;
                    }

                    match shape.data_type(column) {
                        SqlDataType::Int => ColumnValue::Int(row as i32 ^ column as i32),
                        SqlDataType::BigInt => {
                            ColumnValue::BigInt((row as i64) << 32 | column as i64)
                        }
                        SqlDataType::NVarChar => {
                            ColumnValue::NVarChar(format!("row_{row}_column_{column}"))
                        }
                        SqlDataType::TinyInt | SqlDataType::SmallInt => unreachable!(),
                    }
                })
                .collect();
            Row::new(values)
        })
        .collect();

    QueryResponse::new(columns, rows).with_nbc_rows()
}

async fn connect_mock(address: SocketAddr) -> TdsClient {
    let mut context = ClientContext::default();
    context.user_name = "sa".to_string();
    context.password = "MockBenchmark1!".to_string();
    context.database = "master".to_string();
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::PreferOff,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };

    TdsConnectionProvider {}
        .create_client(
            context,
            &format!("tcp:{},{}", address.ip(), address.port()),
            None,
        )
        .await
        .expect("failed to connect to mock TDS server")
}

fn nbcrow_row_throughput(c: &mut Criterion) {
    let row_count = env::var("BENCH_NBC_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000usize);
    let rt = runtime();
    let server = rt
        .block_on(MockTdsServer::new("127.0.0.1:0"))
        .expect("failed to start mock TDS server");
    let address = server.local_addr();
    let registry = server.query_registry();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server_handle = rt.spawn(server.run_with_shutdown(shutdown_rx));
    let mut client = rt.block_on(connect_mock(address));

    let mut group = c.benchmark_group("nbcrow_row_throughput");
    group.throughput(Throughput::Elements(row_count as u64));

    for shape in [RowShape::FixedWidth, RowShape::TextHeavy, RowShape::Mixed] {
        for column_count in [4usize, 16, 64] {
            rt.block_on(async {
                registry
                    .lock()
                    .await
                    .register(QUERY, response(shape, column_count, row_count));
            });

            group.bench_with_input(
                BenchmarkId::new(
                    shape.name(),
                    format!("{column_count}_columns_{NULL_PERCENT}_percent_null"),
                ),
                &column_count,
                |b, _| {
                    b.iter(|| {
                        rt.block_on(async {
                            client
                                .execute(QUERY.to_string(), ())
                                .await
                                .expect("execute failed");
                            assert_eq!(drain(&mut client).await, row_count as u64);
                        });
                    });
                },
            );
        }
    }

    group.finish();
    drop(client);
    shutdown_tx
        .send(())
        .expect("mock TDS server stopped before benchmark shutdown");
    rt.block_on(server_handle)
        .expect("mock TDS server task failed")
        .expect("mock TDS server shutdown failed");
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = nbcrow_row_throughput
}
criterion_main!(benches);
