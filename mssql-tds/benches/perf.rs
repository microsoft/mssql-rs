// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::env;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds::{
    connection::{
        client_context::ClientContext,
        tds_client::{ResultSet, ResultSetClient},
    },
    connection_provider::tds_connection_provider::TdsConnectionProvider,
    core::{EncryptionOptions, EncryptionSetting},
};

static QUERY_TO_BENCHMARK: &str = "SELECT * FROM sys.databases; select * from sys.columns";

const ROW_COUNT: u64 = 10_000;

/// Cross-join sys.columns to produce ROW_COUNT rows with 20 columns (mixed types).
fn iter_rows_query() -> String {
    format!(
        r#"SELECT TOP ({ROW_COUNT})
    c1.object_id, c1.name, c1.column_id, c1.system_type_id,
    c1.user_type_id, c1.max_length, c1.precision, c1.scale,
    c1.collation_name, c1.is_nullable, c1.is_ansi_padded, c1.is_rowguidcol,
    c1.is_identity, c1.is_computed, c1.is_filestream, c1.is_replicated,
    c1.is_non_sql_subscribed, c1.is_merge_published, c1.is_dts_replicated, c1.is_xml_document
FROM sys.columns c1
CROSS JOIN sys.columns c2
ORDER BY c1.object_id, c1.column_id"#
    )
}

fn iter_rows(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("row_iteration");
    group.throughput(Throughput::Elements(ROW_COUNT));
    group.bench_function("iter_rows", |b| {
        b.iter(|| {
            let context = create_context();
            rt.block_on(async move {
                let provider = TdsConnectionProvider {};
                let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
                let port = env::var("DB_PORT")
                    .expect("DB_PORT environment variable not set")
                    .parse::<u16>()
                    .expect("DB_PORT must be a valid u16");
                let datasource = format!("tcp:{},{}", host, port);
                let mut client = provider
                    .create_client(context, &datasource, None)
                    .await
                    .unwrap();

                client.execute(iter_rows_query(), None, None).await.unwrap();

                let mut _row_count = 0u64;
                loop {
                    while client.next_row().await.unwrap().is_some() {
                        _row_count += 1;
                    }
                    if !client.move_to_next().await.unwrap() {
                        break;
                    }
                }
            });
        })
    });
    group.finish();
}

fn connect_fetch_multiple_packets(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    c.bench_function("connect_fetch_multiple_packets", |b| {
        b.iter(|| {
            let context = create_context();
            rt.block_on(async move {
                let provider = TdsConnectionProvider {};
                let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
                let port = env::var("DB_PORT")
                    .expect("DB_PORT environment variable not set")
                    .parse::<u16>()
                    .expect("DB_PORT must be a valid u16");
                let datasource = format!("tcp:{},{}", host, port);
                let mut client = provider
                    .create_client(context, &datasource, None)
                    .await
                    .unwrap();

                let mut _row_count = 0;
                client
                    .execute(QUERY_TO_BENCHMARK.to_string(), None, None)
                    .await
                    .unwrap();
                // let mut row_count = 0;
                loop {
                    while client.next_row().await.unwrap().is_some() {
                        _row_count += 1;
                    }

                    if !client.move_to_next().await.unwrap() {
                        break;
                    }
                }
            });
        })
    });
}

pub fn create_context() -> ClientContext {
    dotenv::dotenv().ok();
    let mut context = ClientContext::default();
    context.user_name = env::var("DB_USERNAME").expect("DB_USERNAME environment variable not set");
    context.password = env::var("SQL_PASSWORD")
        .or_else(|_| {
            std::fs::read_to_string("/tmp/password")
                .map(|s| s.trim().to_string())
                .map_err(|_| std::env::VarError::NotPresent)
        })
        .expect("SQL_PASSWORD environment variable not set and /tmp/password could not be read");
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::On,
        trust_server_certificate: env::var("TRUST_SERVER_CERTIFICATE")
            .map(|v| v.parse().unwrap_or(false))
            .unwrap_or(false),
        host_name_in_cert: env::var("CERT_HOST_NAME").ok(),
        server_certificate: None,
    };
    context
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .measurement_time(std::time::Duration::from_secs(120));
    targets = iter_rows, connect_fetch_multiple_packets
}
criterion_main!(benches);
