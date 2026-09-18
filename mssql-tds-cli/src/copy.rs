// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    future::Future,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use clap::Args;
use futures::stream;
use mssql_tds::{
    connection::{
        bulk_copy::{BulkCopy, BulkCopyResult, BulkLoadRow},
        client_context::ClientContext,
        tds_client::{CursorColumn, ResultSet, StatementResult, TdsClient},
    },
    connection_provider::tds_connection_provider::TdsConnectionProvider,
    core::{EncryptionOptions, EncryptionSetting, TdsResult},
    datatypes::{
        bulk_copy_metadata::{BulkCopyColumnMetadata, SqlDbType},
        column_values::ColumnValues,
    },
    error::Error,
    message::bulk_load::StreamingBulkLoadWriter,
};
use tokio::sync::Mutex;

const COPY_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Args, Debug)]
pub(crate) struct CopyArgs {
    #[arg(long)]
    source_server: String,
    #[arg(long)]
    source_database: String,
    #[arg(long)]
    source_user: String,
    /// Environment variable containing the source SQL authentication password.
    #[arg(long, default_value = "MSSQL_SOURCE_PASSWORD")]
    source_password_env: String,
    #[arg(long)]
    destination_server: String,
    #[arg(long)]
    destination_database: String,
    #[arg(long)]
    destination_user: String,
    /// Environment variable containing the destination SQL authentication password.
    #[arg(long, default_value = "MSSQL_DESTINATION_PASSWORD")]
    destination_password_env: String,
    /// A single SELECT, with columns in destination order.
    #[arg(long)]
    query: String,
    /// Existing destination table (optionally schema-qualified).
    #[arg(long)]
    destination_table: String,
    /// Rows per transaction; streaming MAX values do not buffer a whole batch.
    #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u32).range(1..=1_000_000))]
    batch_size: u32,
    /// Bulk timeout in seconds (whole operation when streaming); zero means unlimited.
    #[arg(long, default_value_t = 30)]
    timeout: u32,
    #[arg(long)]
    table_lock: bool,
    #[arg(long)]
    keep_identity: bool,
    /// Disable TLS certificate verification on both connections (development only).
    #[arg(long)]
    trust_server_certificate: bool,
}

async fn connect(
    server: &str,
    database: &str,
    user: &str,
    password_env: &str,
    trust_server_certificate: bool,
) -> Result<TdsClient, Box<dyn std::error::Error>> {
    let password = std::env::var(password_env).map_err(|_| {
        std::io::Error::other(format!("Set password environment variable {password_env}"))
    })?;
    let mut context = ClientContext::default();
    context.user_name = user.to_owned();
    context.password = password;
    context.database = database.to_owned();
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::On,
        trust_server_certificate,
        host_name_in_cert: None,
        server_certificate: None,
    };
    Ok(TdsConnectionProvider::new()
        .create_client(context, server, None)
        .await?)
}

struct CopyRow(Vec<ColumnValues>);

#[async_trait]
impl BulkLoadRow for CopyRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        for value in &self.0 {
            writer.write_column_value(*column_index, value).await?;
            *column_index += 1;
        }
        Ok(())
    }
}

struct StreamingSource<'a> {
    client: &'a mut TdsClient,
    buffer: Vec<u8>,
    column_count: usize,
    deadline: Option<tokio::time::Instant>,
}

struct StreamingCopyRow<'a, 'b>(&'a Mutex<StreamingSource<'b>>);

async fn source_read<T>(
    deadline: Option<tokio::time::Instant>,
    read: impl Future<Output = TdsResult<T>>,
) -> TdsResult<T> {
    if let Some(deadline) = deadline {
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::UsageError(
                "Source read timed out during streaming copy".into(),
            ));
        }
        tokio::time::timeout_at(deadline, read)
            .await
            .map_err(|_| Error::UsageError("Source read timed out during streaming copy".into()))?
    } else {
        read.await
    }
}

#[async_trait]
impl BulkLoadRow for StreamingCopyRow<'_, '_> {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        let mut source = self.0.lock().await;
        let StreamingSource {
            client,
            buffer,
            column_count,
            deadline,
        } = &mut *source;
        for index in 0..*column_count {
            match source_read(*deadline, client.read_row_column(index)).await? {
                CursorColumn::Value { value, .. } => {
                    writer.write_column_value(*column_index, &value).await?;
                }
                CursorColumn::PlpStreaming { .. } => {
                    // Source and destination encodings were checked before the load.
                    // Reframe payload bytes using PLP_UNKNOWNLEN, not source packet headers.
                    writer
                        .write_raw_bytes(&(u64::MAX - 1).to_le_bytes())
                        .await?;
                    loop {
                        let chunk =
                            source_read(*deadline, client.read_active_plp_chunk(buffer)).await?;
                        if chunk.read != 0 {
                            writer
                                .write_raw_bytes(&(chunk.read as u32).to_le_bytes())
                                .await?;
                            writer.write_raw_bytes(&buffer[..chunk.read]).await?;
                        }
                        if chunk.reached_end {
                            break;
                        }
                    }
                    writer.write_raw_bytes(&0u32.to_le_bytes()).await?;
                }
                CursorColumn::AlreadyConsumed | CursorColumn::RowEnded => {
                    return Err(Error::ProtocolError(
                        "Source row ended before all copy columns were read".into(),
                    ));
                }
            }
            *column_index += 1;
        }
        Ok(())
    }
}

fn can_stream_columns(metadata: &[BulkCopyColumnMetadata]) -> bool {
    metadata.iter().any(|column| column.length_type.is_plp())
        && metadata.iter().all(|column| {
            !column.is_encrypted
                && (!column.length_type.is_plp()
                    || matches!(
                        column.sql_type,
                        SqlDbType::VarBinary | SqlDbType::VarChar | SqlDbType::NVarChar
                    ))
        })
}

async fn stream_rows(
    source: &mut TdsClient,
    bulk: &mut BulkCopy<'_>,
    column_count: usize,
    timeout: Duration,
) -> TdsResult<u64> {
    let source = Mutex::new(StreamingSource {
        client: source,
        buffer: vec![0; COPY_BUFFER_SIZE],
        column_count,
        deadline: (!timeout.is_zero()).then(|| tokio::time::Instant::now() + timeout),
    });
    let rows = stream::try_unfold(&source, |source| async move {
        let mut state = source.lock().await;
        let deadline = state.deadline;
        let has_row = source_read(deadline, state.client.next_row_cursor()).await?;
        drop(state);
        Ok::<_, Error>(has_row.then_some((StreamingCopyRow(source), source)))
    });
    Ok(Box::pin(bulk.write_to_server_stream(rows))
        .await?
        .rows_affected)
}

fn validate_schema(
    source: &[BulkCopyColumnMetadata],
    destination: &[BulkCopyColumnMetadata],
) -> TdsResult<()> {
    if source.len() != destination.len() || source.is_empty() {
        return Err(Error::UsageError(
            "Source and destination must have the same nonzero column count; omit destination identity columns from the SELECT unless using --keep-identity".into(),
        ));
    }
    for (source, destination) in source.iter().zip(destination) {
        if source.column_name != destination.column_name
            || source.sql_type != destination.sql_type
            || source.length != destination.length
            || source.precision != destination.precision
            || source.scale != destination.scale
            || source.collation != destination.collation
            || source.is_encrypted
            || destination.is_encrypted
        {
            return Err(Error::UsageError(format!(
                "Column {:?} must match destination {:?} in name, order, type, length, precision, scale and collation; encrypted columns are not supported",
                source.column_name, destination.column_name
            )));
        }
    }
    Ok(())
}

async fn read_batch(source: &mut TdsClient, batch_size: usize) -> TdsResult<Vec<CopyRow>> {
    let mut rows = Vec::new();
    while rows.len() < batch_size {
        match source.next_row().await? {
            Some(row) => rows.push(CopyRow(row)),
            None => break,
        }
    }
    Ok(rows)
}

async fn pipeline_batch(
    read: impl Future<Output = TdsResult<Vec<CopyRow>>>,
    write: impl Future<Output = TdsResult<BulkCopyResult>>,
) -> TdsResult<(Vec<CopyRow>, BulkCopyResult)> {
    // Source errors wait for the destination transaction to finish; destination errors
    // abandon the source read immediately. Neither connection is reused on failure.
    let (next, written) = tokio::try_join!(async { Ok::<_, Error>(read.await) }, write)?;
    Ok((next?, written))
}

async fn transfer(
    source: &mut TdsClient,
    destination: &mut TdsClient,
    args: &CopyArgs,
) -> TdsResult<u64> {
    if !matches!(
        source.execute(args.query.clone(), ()).await?,
        StatementResult::Rows
    ) {
        return Err(Error::UsageError(
            "Source query must return one rowset".into(),
        ));
    }
    let source_metadata: Vec<_> = source
        .get_metadata()
        .iter()
        .map(BulkCopyColumnMetadata::from)
        .collect();
    let mut bulk = BulkCopy::new(destination, &args.destination_table)
        .batch_size(args.batch_size as usize)
        .timeout(Duration::from_secs(args.timeout.into()))
        .table_lock(args.table_lock)
        .keep_identity(args.keep_identity)
        .keep_nulls(true)
        .check_constraints(true)
        .use_internal_transaction(true);
    let destination_metadata: Vec<_> = bulk
        .retrieve_destination_metadata()
        .await?
        .into_iter()
        .filter(|column| args.keep_identity || !column.is_identity)
        .collect();
    validate_schema(&source_metadata, &destination_metadata)?;

    let copied = if can_stream_columns(&source_metadata) {
        Box::pin(stream_rows(
            source,
            &mut bulk,
            source_metadata.len(),
            Duration::from_secs(args.timeout.into()),
        ))
        .await?
    } else {
        let mut rows = read_batch(source, args.batch_size as usize).await?;
        let mut copied = 0;
        while !rows.is_empty() {
            // Overlap the next source read with the destination write, keeping at most two batches.
            let (next, written) = Box::pin(pipeline_batch(
                read_batch(source, args.batch_size as usize),
                bulk.write_to_server_zerocopy(rows),
            ))
            .await?;
            copied += written.rows_affected;
            rows = next;
        }
        copied
    };
    loop {
        match source.advance().await? {
            StatementResult::End => break,
            StatementResult::NoRows { .. } => {}
            StatementResult::Rows => {
                return Err(Error::UsageError(
                    "Source query returned more than one rowset; only the first was copied".into(),
                ));
            }
        }
    }
    Ok(copied)
}

pub(crate) async fn run(args: CopyArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut source = connect(
        &args.source_server,
        &args.source_database,
        &args.source_user,
        &args.source_password_env,
        args.trust_server_certificate,
    )
    .await?;
    let mut destination = connect(
        &args.destination_server,
        &args.destination_database,
        &args.destination_user,
        &args.destination_password_env,
        args.trust_server_certificate,
    )
    .await?;
    let start = Instant::now();
    let copied = transfer(&mut source, &mut destination, &args)
        .await
        .map_err(|error| {
            std::io::Error::other(format!(
                "Copy failed; earlier batches may already be committed: {error}"
            ))
        })?;
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "Copied {copied} rows in {elapsed:.3}s ({:.0} rows/s)",
        copied as f64 / elapsed.max(f64::EPSILON)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mssql_tds::datatypes::bulk_copy_metadata::{SqlDbType, TypeLength};

    fn binary_column() -> BulkCopyColumnMetadata {
        BulkCopyColumnMetadata::new("payload", SqlDbType::VarBinary, 0xA5)
            .with_length(-1, TypeLength::Plp)
    }

    #[test]
    fn matching_binary_schema_is_accepted() {
        assert!(validate_schema(&[binary_column()], &[binary_column()]).is_ok());
    }

    #[test]
    fn streaming_requires_compatible_plp_encodings() {
        assert!(can_stream_columns(&[binary_column()]));
        for sql_type in [SqlDbType::VarChar, SqlDbType::NVarChar] {
            let mut column = binary_column();
            column.sql_type = sql_type;
            assert!(can_stream_columns(&[binary_column(), column]));
        }
        for sql_type in [SqlDbType::Xml, SqlDbType::Json, SqlDbType::Udt] {
            let mut column = binary_column();
            column.sql_type = sql_type;
            assert!(!can_stream_columns(&[binary_column(), column]));
        }
        assert!(!can_stream_columns(&[binary_column().with_encrypted(true)]));
        assert!(!can_stream_columns(&[
            binary_column().with_length(8000, TypeLength::Variable(8000))
        ]));
        assert!(!can_stream_columns(&[]));
    }

    #[tokio::test]
    async fn stalled_source_read_times_out() {
        assert!(
            source_read::<()>(
                Some(tokio::time::Instant::now() + Duration::from_millis(1)),
                std::future::pending(),
            )
            .await
            .is_err()
        );
        assert!(source_read(None, async { Ok(()) }).await.is_ok());
    }

    #[tokio::test]
    async fn source_reads_share_one_deadline() {
        let deadline = Some(tokio::time::Instant::now() + Duration::from_millis(1));
        assert!(
            source_read::<()>(deadline, std::future::pending())
                .await
                .is_err()
        );
        assert!(source_read(deadline, async { Ok(()) }).await.is_err());
    }

    #[test]
    fn mismatched_schema_is_rejected() {
        assert!(validate_schema(&[], &[]).is_err());
        assert!(validate_schema(&[binary_column()], &[]).is_err());
        for column in [
            BulkCopyColumnMetadata::new("other", SqlDbType::VarBinary, 0xA5)
                .with_length(-1, TypeLength::Plp),
            binary_column().with_length(10, TypeLength::Variable(10)),
            binary_column().with_precision_scale(18, 2),
            binary_column().with_encrypted(true),
            BulkCopyColumnMetadata::new("payload", SqlDbType::NVarChar, 0xE7)
                .with_length(-1, TypeLength::Plp),
        ] {
            assert!(validate_schema(&[binary_column()], &[column]).is_err());
        }
    }

    #[tokio::test]
    async fn destination_failure_does_not_wait_for_source() {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            pipeline_batch(std::future::pending(), async {
                Err(Error::UsageError("destination failed".into()))
            }),
        )
        .await
        .expect("Destination failure must not wait for a blocked source");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn source_failure_waits_for_destination_cleanup() {
        let mut finished = false;
        let result = pipeline_batch(
            async { Err(Error::UsageError("source failed".into())) },
            async {
                tokio::task::yield_now().await;
                finished = true;
                Ok(BulkCopyResult::new(2, Duration::ZERO))
            },
        )
        .await;
        assert!(result.is_err());
        assert!(finished);
    }
}
