// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Row-by-row writes for a table-valued parameter opened by a streamed RPC.
//!
//! The parameter header (type name and column metadata) is written when the
//! parameter opens, exactly as for a materialized table. Rows then follow as
//! the caller supplies them, so the client holds at most the cells of one call
//! rather than the whole table; `PacketWriter` sends each packet as it fills.
//!
//! A cell of a `varchar(max)`, `nvarchar(max)` or `varbinary(max)` column can
//! itself be streamed: [`TdsClient::begin_streamed_tvp_cell`] opens it, the
//! ordinary [`TdsClient::write_streamed_chunk`] /
//! [`TdsClient::write_streamed_null`] supply it, and
//! [`TdsClient::end_streamed_tvp_cell`] closes it. The table itself closes with
//! [`TdsClient::end_streamed_param`] once every started row is complete.

use super::*;
use crate::datatypes::sql_tvp::{
    TVP_ROW_TOKEN, TvpColumnDef, is_plp_tvp_column, validate_tvp_cell, write_tvp_cell,
};
use crate::error::{Error, TvpParameterRef};

/// Position inside the table-valued parameter a streamed RPC has open.
#[derive(Debug)]
pub(super) struct StreamedTableCursor {
    columns: Box<[TvpColumnDef]>,
    /// Identifies the parameter in cell diagnostics.
    parameter: TvpParameterRef,
    /// Column the next cell belongs to; zero between rows.
    next_column: usize,
    /// Rows whose row token has been written.
    rows: usize,
    /// A PLP cell is open for chunks.
    cell_open: bool,
}

impl StreamedTableCursor {
    /// The cursor for a parameter whose header was just written, or `None`
    /// when it is not a streamed table.
    pub(super) fn open(param: RpcParameter) -> Option<Box<Self>> {
        let parameter = param.tvp_parameter_ref(TvpParameterRef::Name(
            param.name.clone().unwrap_or_else(|| "unnamed".to_string()),
        ));
        let columns = param.into_streamed_table_columns()?;
        Some(Box::new(Self {
            columns: columns.into_boxed_slice(),
            parameter,
            next_column: 0,
            rows: 0,
            cell_open: false,
        }))
    }

    /// Whether the table can be closed: no cell open and no row half written.
    pub(super) fn at_row_boundary(&self) -> bool {
        self.next_column == 0 && !self.cell_open
    }

    /// Whether chunks would land in an open cell.
    pub(super) fn cell_open(&self) -> bool {
        self.cell_open
    }

    /// Zero-based row of the cell about to be written at `column`, counting a
    /// row that `column == 0` would start.
    fn row_for(&self, column: usize, rows: usize) -> usize {
        if column == 0 {
            rows
        } else {
            rows.saturating_sub(1)
        }
    }

    fn advance_column(&mut self) {
        self.next_column = (self.next_column + 1) % self.columns.len();
    }

    fn cell_error(&self, error: Error, row: usize, column: usize) -> Error {
        error
            .with_tvp_cell_context(row, column)
            .with_tvp_parameter_context(self.parameter.clone())
    }
}

impl TdsClient {
    /// Appends cells to the table-valued parameter currently open in a
    /// streamed RPC, in column order. A cell in the first column starts a new
    /// row; rows may span calls.
    ///
    /// Every cell is validated against its column before anything is written,
    /// so a rejected call leaves the stream where it was. A failure after
    /// bytes have been written withdraws the request instead, because the
    /// server must never execute a truncated table.
    ///
    /// # Errors
    /// Returns a usage error when no table is open, a cell is open, or a value
    /// does not fit its column, and an I/O error if sending fails.
    pub async fn write_streamed_tvp_cells(&mut self, cells: &[SqlType]) -> TdsResult<()> {
        let mut ctx = self.take_streamed_table("write_streamed_tvp_cells")?;
        let table = open_table(&mut ctx);
        let rejection = if table.cell_open {
            Err(UsageError(
                "write_streamed_tvp_cells called while a table cell is open.".to_string(),
            ))
        } else {
            validate_cells(table, cells)
        };
        if rejection.is_err() || cells.is_empty() {
            self.park_streamed(ctx);
            return rejection;
        }

        self.remaining_request_timeout = ctx.timeout_sec.map(|s| Duration::from_secs(u64::from(s)));
        let mut packet_writer = PacketWriter::resume(ctx.message, self.transport.as_writer());
        let table = ctx
            .table
            .as_deref_mut()
            .expect("checked by take_streamed_table");
        let db_collation = &ctx.db_collation;
        let result = async {
            for cell in cells {
                let column = table.next_column;
                if column == 0 {
                    packet_writer.write_byte_async(TVP_ROW_TOKEN).await?;
                    table.rows += 1;
                }
                let row = table.rows.saturating_sub(1);
                write_tvp_cell(
                    &mut packet_writer,
                    &table.columns[column],
                    cell,
                    db_collation,
                )
                .await
                .map_err(|error| table.cell_error(error, row, column))?;
                table.advance_column();
            }
            Ok(())
        }
        .await;
        ctx.message = packet_writer.suspend();
        self.settle_table_write(ctx, result).await
    }

    /// Opens the next cell of the streamed table-valued parameter for chunks.
    ///
    /// The cell's column must be `varchar(max)`, `nvarchar(max)` or
    /// `varbinary(max)`. Supply it with
    /// [`write_streamed_chunk`](Self::write_streamed_chunk) or
    /// [`write_streamed_null`](Self::write_streamed_null), then close it with
    /// [`end_streamed_tvp_cell`](Self::end_streamed_tvp_cell). A cell in the
    /// first column starts a new row.
    ///
    /// # Errors
    /// Returns a usage error when no table is open, a cell is already open, or
    /// the column cannot be streamed, and an I/O error if sending fails.
    pub async fn begin_streamed_tvp_cell(&mut self) -> TdsResult<()> {
        let mut ctx = self.take_streamed_table("begin_streamed_tvp_cell")?;
        let table = open_table(&mut ctx);
        let rejection = if table.cell_open {
            Some("begin_streamed_tvp_cell called while a table cell is open.".to_string())
        } else if !is_plp_tvp_column(&table.columns[table.next_column]) {
            Some(format!(
                "Table-valued parameter column {} is not varchar(max), nvarchar(max) or \
                 varbinary(max), so its cells cannot be streamed.",
                table.next_column + 1
            ))
        } else {
            None
        };
        if let Some(message) = rejection {
            self.park_streamed(ctx);
            return Err(UsageError(message));
        }

        let starts_row = table.next_column == 0;
        table.cell_open = true;
        table.rows += usize::from(starts_row);
        ctx.value_opened = false;
        ctx.null_signaled = false;
        ctx.xml_bom_state = StreamedXmlBomState::NotXml;
        if !starts_row {
            self.park_streamed(ctx);
            return Ok(());
        }

        self.remaining_request_timeout = ctx.timeout_sec.map(|s| Duration::from_secs(u64::from(s)));
        let mut packet_writer = PacketWriter::resume(ctx.message, self.transport.as_writer());
        let result = packet_writer.write_byte_async(TVP_ROW_TOKEN).await;
        ctx.message = packet_writer.suspend();
        self.settle_table_write(ctx, result).await
    }

    /// Closes the open cell of the streamed table-valued parameter: `PLP_NULL`
    /// after [`write_streamed_null`](Self::write_streamed_null), otherwise the
    /// PLP terminator (preceded by the unknown-length opener when no chunk was
    /// written, which makes the value empty rather than NULL).
    ///
    /// # Errors
    /// Returns a usage error when no table cell is open, and an I/O error if
    /// sending fails.
    pub async fn end_streamed_tvp_cell(&mut self) -> TdsResult<()> {
        let mut ctx = self.take_streamed_table("end_streamed_tvp_cell")?;
        if !open_table(&mut ctx).cell_open {
            self.park_streamed(ctx);
            return Err(UsageError(
                "end_streamed_tvp_cell called with no open table cell.".to_string(),
            ));
        }

        self.remaining_request_timeout = ctx.timeout_sec.map(|s| Duration::from_secs(u64::from(s)));
        let (null_signaled, value_opened) = (ctx.null_signaled, ctx.value_opened);
        let mut packet_writer = PacketWriter::resume(ctx.message, self.transport.as_writer());
        let result = async {
            if null_signaled {
                packet_writer.write_u64_async(PLP_NULL).await
            } else {
                if !value_opened {
                    packet_writer.write_u64_async(PLP_UNKNOWN_LEN).await?;
                }
                packet_writer.write_u32_async(PLP_TERMINATOR).await
            }
        }
        .await;
        ctx.message = packet_writer.suspend();
        if result.is_ok() {
            let table = open_table(&mut ctx);
            table.cell_open = false;
            table.advance_column();
            ctx.value_opened = false;
            ctx.null_signaled = false;
        }
        self.settle_table_write(ctx, result).await
    }

    /// Takes the active streamed write, which must have a table open.
    fn take_streamed_table(&mut self, operation: &str) -> TdsResult<StreamedWriteContext> {
        let ctx = match std::mem::replace(&mut self.streamed_write_state, StreamedWriteState::Idle)
        {
            StreamedWriteState::Active(ctx) => *ctx,
            StreamedWriteState::Idle => {
                return Err(UsageError(format!(
                    "{operation} called with no active streamed parameter."
                )));
            }
        };
        if ctx.table.is_none() {
            self.park_streamed(ctx);
            return Err(UsageError(format!(
                "{operation} requires an open table-valued parameter."
            )));
        }
        Ok(ctx)
    }

    fn park_streamed(&mut self, ctx: StreamedWriteContext) {
        self.streamed_write_state = StreamedWriteState::Active(Box::new(ctx));
    }

    /// `true` when the open parameter is a table with no cell open, so PLP
    /// chunks or a NULL have nowhere to go.
    pub(super) fn streamed_table_without_open_cell(&self) -> bool {
        matches!(
            &self.streamed_write_state,
            StreamedWriteState::Active(ctx)
                if ctx.table.as_deref().is_some_and(|table| !table.cell_open())
        )
    }

    /// Re-parks the stream after a clean write, or withdraws the request after
    /// a failed one: bytes may already have left, so it cannot be resumed.
    async fn settle_table_write(
        &mut self,
        ctx: StreamedWriteContext,
        result: TdsResult<()>,
    ) -> TdsResult<()> {
        match result.and_then(|()| ctx.message.check_cancellation()) {
            Ok(()) => {
                self.park_streamed(ctx);
                Ok(())
            }
            Err(error) => {
                self.retract_partial_request(ctx.message).await;
                Err(error)
            }
        }
    }
}

fn open_table(ctx: &mut StreamedWriteContext) -> &mut StreamedTableCursor {
    ctx.table
        .as_deref_mut()
        .expect("checked by take_streamed_table")
}

/// Validates `cells` from the table's current position without moving it.
fn validate_cells(table: &StreamedTableCursor, cells: &[SqlType]) -> TdsResult<()> {
    let mut column = table.next_column;
    let mut rows = table.rows;
    for cell in cells {
        let row = table.row_for(column, rows);
        rows += usize::from(column == 0);
        validate_tvp_cell(&table.columns[column], cell, row, column)
            .map_err(|error| error.with_tvp_parameter_context(table.parameter.clone()))?;
        column = (column + 1) % table.columns.len();
    }
    Ok(())
}
