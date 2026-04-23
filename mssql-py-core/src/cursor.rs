// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::connection::bulk_copy::{
    BulkCopy, ColumnMapping as TdsColumnMapping, ColumnMappingSource,
};
use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient, TdsClient};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sqldatatypes::VectorBaseType;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyIterator, PyList, PyTuple};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::bulkcopy::PythonRowAdapter;
use crate::python_logger_adapter::scoped_tracing_bridge;
use crate::utils::convert_tds_error;

/// Python Cursor class for Core TDS backend
#[pyclass]
pub struct PyCoreCursor {
    tds_client: Arc<Mutex<TdsClient>>,
    runtime_handle: Handle,
    has_resultset: bool,
}

#[pymethods]
impl PyCoreCursor {
    #[pyo3(signature = (query, params=None))]
    #[allow(unused_variables)]
    fn execute(
        &mut self,
        py: Python,
        query: String,
        params: Option<Vec<Py<PyAny>>>,
    ) -> PyResult<()> {
        info!("execute: Executing query: {}", query);

        let tds_client = self.tds_client.clone();
        let runtime_handle = self.runtime_handle.clone();

        // Execute query asynchronously
        py.detach(|| {
            runtime_handle.block_on(async {
                let mut client = tds_client.lock().await;
                info!("execute: Locked TDS client");

                // Close any open result set before executing new query
                if let Some(resultset) = client.get_current_resultset() {
                    info!(" execute: Closing previous result set before new query");
                    resultset.close().await.map_err(|e| {
                        error!("execute: Failed to close previous result set: {}", e);
                        pyo3::exceptions::PyRuntimeError::new_err(format!(
                            "Failed to close previous result set: {}",
                            e
                        ))
                    })?;
                }

                // Execute with 30 second timeout
                client.execute(query, Some(30), None).await.map_err(|e| {
                    error!("execute: Failed to execute query: {}", e);
                    pyo3::exceptions::PyRuntimeError::new_err(format!(
                        "Query execution failed: {}",
                        e
                    ))
                })?;

                info!("execute: Query executed successfully");
                Ok::<_, PyErr>(())
            })
        })?;

        self.has_resultset = true;
        Ok(())
    }

    fn fetchone(&mut self, py: Python) -> PyResult<Option<Py<PyAny>>> {
        if !self.has_resultset {
            return Ok(None);
        }

        info!("fetchone: Fetching one row");

        let tds_client = self.tds_client.clone();
        let runtime_handle = self.runtime_handle.clone();

        // Fetch one row via next_row_into → PyRowWriter (bypasses RowToken)
        let result = py.detach(|| {
            runtime_handle.block_on(async {
                let mut client = tds_client.lock().await;
                info!("fetchone: Locked TDS client");

                if let Some(resultset) = client.get_current_resultset() {
                    info!("fetchone: Got resultset, fetching next row");
                    let col_count = resultset.get_metadata().len();
                    let mut writer = crate::row_writer::PyRowWriter::new(col_count);
                    let has_row = resultset.next_row_into(&mut writer).await.map_err(|e| {
                        error!("fetchone: Failed to fetch row: {}", e);
                        pyo3::exceptions::PyRuntimeError::new_err(format!(
                            "Failed to fetch row: {}",
                            e
                        ))
                    })?;
                    if has_row {
                        info!("fetchone: Got row with {} columns", col_count);
                        return Ok(Some(writer));
                    } else {
                        info!("No more rows, closing result set");
                        resultset.close().await.map_err(|e| {
                            error!("fetchone: Failed to close result set: {}", e);
                            pyo3::exceptions::PyRuntimeError::new_err(format!(
                                "Failed to close result set: {}",
                                e
                            ))
                        })?;
                        info!("Result set closed successfully");
                    }
                }

                info!("fetchone: No more rows");
                Ok::<_, PyErr>(None)
            })
        })?;

        // Convert row to Python tuple (GIL re-acquired)
        if let Some(writer) = result {
            Python::attach(|py| {
                let py_tuple = writer.to_py_tuple(py)?;
                Ok(Some(py_tuple.into()))
            })
        } else {
            self.has_resultset = false;
            Ok(None)
        }
    }

    fn fetchall(&mut self, py: Python) -> PyResult<Vec<Py<PyAny>>> {
        if !self.has_resultset {
            return Ok(vec![]);
        }

        info!("fetchall: Fetching all rows");

        let mut results = Vec::new();
        while let Some(row) = self.fetchone(py)? {
            results.push(row);
        }

        info!("fetchall: Fetched {} rows", results.len());
        Ok(results)
    }

    fn fetchmany(&mut self, py: Python, size: Option<usize>) -> PyResult<Vec<Py<PyAny>>> {
        let fetch_size = size.unwrap_or(1);
        let mut results = Vec::new();

        for _ in 0..fetch_size {
            if let Some(row) = self.fetchone(py)? {
                results.push(row);
            } else {
                break;
            }
        }

        Ok(results)
    }

    fn close(&mut self) -> PyResult<()> {
        // TODO: Might need to drain the results.
        self.has_resultset = false;
        Ok(())
    }

    /// Perform bulk copy operation to insert data into a SQL Server table.
    ///
    /// # Arguments
    ///
    /// * `table_name` - Name of the destination table (can include schema: "schema.table")
    /// * `data_source` - Python iterator yielding tuples of data to insert
    /// * `batch_size` - Number of rows per batch. Default: 0.
    ///   - When 0: All rows are sent in a single batch
    ///   - When N > 0: Rows are sent in batches of N rows each
    ///   - Each batch sends a DONE token to the server, enabling partial commit behavior
    /// * `timeout` - Timeout in seconds (default: 30)
    /// * `column_mappings` - Optional list for column mapping. Can be:
    ///   - List of column names: `['id', 'name', 'email']` - maps by ordinal position
    ///   - List of (ordinal, name) tuples: `[(0, 'id'), (1, 'name')]` - explicit mapping
    ///   If not provided, automatic ordinal-based mapping is used (0→0, 1→1, etc.).
    /// * `keep_identity` - Preserve source identity values. When False, identity values are assigned by the destination.
    /// * `check_constraints` - Check constraints while data is being inserted. By default, constraints are not checked.
    /// * `table_lock` - Obtain a bulk update lock for the duration of the bulk copy operation. When False, row locks are used.
    /// * `keep_nulls` - Preserve null values in the destination table regardless of the settings for default values.
    /// * `fire_triggers` - When True, cause the server to fire the insert triggers for the rows being inserted into the database.
    /// * `use_internal_transaction` - When True, wraps each batch in its own transaction. Default: False.
    ///
    ///   **When True:**
    ///   - BEGIN TRANSACTION before each batch
    ///   - COMMIT TRANSACTION after successful batch completion
    ///   - ROLLBACK TRANSACTION on batch failure
    ///   - Enables partial failure recovery: if batch 3 fails, batches 1-2 remain committed
    ///
    ///   **When False (default):**
    ///   - SQL Server autocommit mode applies
    ///   - Each batch is implicitly committed after DONE packet acknowledgment
    ///   - Similar partial recovery, but server-controlled
    ///
    ///   **Note:** This setting only applies when detecting an active transaction on the same connection.
    ///   In mssql-python, BulkCopy operations use a separate connection, so external transactions
    ///   from the parent connection do not affect BulkCopy behavior.
    ///
    /// # Returns
    ///
    /// Dictionary containing:
    /// - `rows_copied` (int): Number of rows successfully copied
    /// - `batch_count` (int): Number of batches sent
    /// - `elapsed_time` (float): Time taken in seconds
    /// - `rows_per_second` (float): Throughput in rows per second
    ///
    /// # Examples
    ///
    /// ```python
    /// cursor = connection.cursor()
    ///
    /// # Basic usage - all rows in single batch
    /// data = [(1, 'Alice'), (2, 'Bob')]
    /// result = cursor.bulkcopy('Users', iter(data))
    ///
    /// # With batch size for large datasets
    /// result = cursor.bulkcopy('Users', iter(large_data), batch_size=1000)
    ///
    /// # Partial failure recovery with internal transactions
    /// # If error at row 2500: rows 1-2000 committed, 2001-2500 rolled back
    /// result = cursor.bulkcopy(
    ///     'Users',
    ///     iter(large_data),
    ///     batch_size=1000,
    ///     use_internal_transaction=True
    /// )
    ///
    /// # With column mappings
    /// result = cursor.bulkcopy('Users', iter(data), column_mappings=['id', 'name'])
    /// print(f"Copied {result['rows_copied']} rows in {result['batch_count']} batches")
    /// ```
    #[pyo3(signature = (table_name, data_source, batch_size=0, timeout=30, column_mappings=None, keep_identity=false, check_constraints=false, table_lock=false, keep_nulls=false, fire_triggers=false, use_internal_transaction=false, python_logger=None))]
    #[allow(clippy::too_many_arguments)]
    fn bulkcopy(
        &mut self,
        py: Python,
        table_name: String,
        data_source: &Bound<'_, PyIterator>,
        batch_size: usize,
        timeout: u64,
        column_mappings: Option<&Bound<'_, PyAny>>,
        keep_identity: bool,
        check_constraints: bool,
        table_lock: bool,
        keep_nulls: bool,
        fire_triggers: bool,
        use_internal_transaction: bool,
        python_logger: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyDict>> {
        // Set up tracing bridge if logger provided
        let _guard = python_logger
            .map(|logger| scoped_tracing_bridge(Arc::new(logger.clone().unbind()), file!()));

        info!("bulkcopy: Starting bulkcopy to table: {}", table_name);

        // Build options from explicit parameters (defaults handled by PyO3 signature)
        let options = BulkCopyOptions {
            batch_size,
            timeout: Duration::from_secs(timeout),
            column_mappings: Self::parse_column_mappings(column_mappings)?,
            keep_identity,
            check_constraints,
            table_lock,
            keep_nulls,
            fire_triggers,
            use_internal_transaction,
        };
        info!(
            "bulkcopy: Parsed options - batch_size={}, timeout={:?}",
            options.batch_size, options.timeout
        );

        // Clone the TdsClient Arc for async execution
        let tds_client = self.tds_client.clone();

        // Track whether we need to auto-generate mappings
        let auto_generate_mappings = options.column_mappings.is_empty();
        if auto_generate_mappings {
            info!(
                "bulkcopy: No column mappings provided, will auto-generate from first row during streaming"
            );
        }

        // Execute async bulk copy while keeping the GIL
        // This blocks the Python interpreter but allows true streaming from Python iterator
        info!("bulkcopy: Starting async execution with GIL held");
        let runtime_handle = self.runtime_handle.clone();
        let result = runtime_handle.block_on(async {
            info!("bulkcopy: Inside async block, attempting to lock TDS client");
            // Lock the TDS client
            let mut client = tds_client.lock().await;
            info!("bulkcopy: Successfully locked TDS client");

            // Create BulkCopy instance
            info!("bulkcopy: Creating BulkCopy instance");
            let mut bulk_copy = BulkCopy::new(&mut client, table_name)
                .batch_size(options.batch_size)
                .timeout(options.timeout)
                .check_constraints(options.check_constraints)
                .fire_triggers(options.fire_triggers)
                .keep_identity(options.keep_identity)
                .keep_nulls(options.keep_nulls)
                .table_lock(options.table_lock)
                .use_internal_transaction(options.use_internal_transaction);
            info!("bulkcopy: BulkCopy instance created");

            // Always retrieve destination metadata for type coercion
            // This is needed even when explicit mappings are provided, because we need
            // to know the target column types for proper value conversion
            info!("bulkcopy: Retrieving destination metadata for type coercion");
            let destination_metadata =
                bulk_copy
                    .retrieve_destination_metadata()
                    .await
                    .map_err(|e| {
                        error!("bulkcopy: Failed to retrieve destination metadata: {}", e);
                        pyo3::exceptions::PyRuntimeError::new_err(format!(
                            "Failed to retrieve destination metadata: {}",
                            e
                        ))
                    })?;

            // Peek at first row from Python iterator to determine source column count for auto-mapping
            // This enables true streaming without collecting all rows upfront
            let mut py_iter = data_source.into_iter();
            let first_row_result = py_iter.next();

            // Capture first row's column count for row consistency validation
            let first_row_col_count = match &first_row_result {
                Some(Ok(first_tuple)) => first_tuple.cast::<PyTuple>().map(|t| t.len()).ok(),
                _ => None,
            };

            let column_mappings = if auto_generate_mappings {
                if let Some(Ok(_first_tuple)) = &first_row_result {
                    let src_col_count = first_row_col_count.unwrap_or(0);

                    info!(
                        "bulkcopy: First row has {} columns, destination has {} columns",
                        src_col_count,
                        destination_metadata.len()
                    );

                    // Auto-generate ordinal mappings for min(source_columns, destination_columns)
                    let mapping_count = std::cmp::min(src_col_count, destination_metadata.len());
                    let mut mappings = Vec::with_capacity(mapping_count);
                    for (i, col_meta) in destination_metadata.iter().enumerate().take(mapping_count)
                    {
                        mappings.push(ColumnMapping::ByOrdinal {
                            source: i,
                            destination: col_meta.column_name.clone(),
                        });
                    }
                    info!("bulkcopy: Auto-generated {} column mappings", mapping_count);
                    mappings
                } else if first_row_result.is_none() {
                    info!("bulkcopy: Empty data source, no rows to copy");
                    Vec::new()
                } else {
                    return Err(pyo3::exceptions::PyRuntimeError::new_err(
                        "Failed to read first row for auto-mapping",
                    ));
                }
            } else {
                options.column_mappings
            };

            // Add column mappings
            info!("bulkcopy: Adding {} column mappings", column_mappings.len());
            for mapping in column_mappings {
                let tds_mapping = match mapping {
                    ColumnMapping::ByName {
                        source,
                        destination,
                    } => TdsColumnMapping {
                        source: ColumnMappingSource::Name(source),
                        destination,
                    },
                    ColumnMapping::ByOrdinal {
                        source,
                        destination,
                    } => TdsColumnMapping {
                        source: ColumnMappingSource::Ordinal(source),
                        destination,
                    },
                };
                bulk_copy = bulk_copy.add_column_mapping(tds_mapping);
            }
            info!("bulkcopy: Column mappings added");

            // Get resolved column mappings for the row adapter
            info!("bulkcopy: Resolving column mappings");
            let resolved_mappings = bulk_copy.get_resolved_mappings().await.map_err(|e| {
                error!("bulkcopy: Failed to resolve column mappings: {}", e);
                pyo3::exceptions::PyRuntimeError::new_err(format!(
                    "Failed to resolve column mappings: {}",
                    e
                ))
            })?;
            info!(
                "bulkcopy: Resolved {} column mappings",
                resolved_mappings.len()
            );
            let resolved_mappings_arc = Arc::new(resolved_mappings);

            // Create true streaming iterator from Python data source
            // Chain first row (if exists) back with remaining rows for zero-copy streaming
            info!("bulkcopy: Creating streaming PythonRowAdapter iterator");
            let metadata_arc = Arc::new(destination_metadata);

            // Build iterator: first row (if exists) + remaining rows from Python iterator
            let all_rows_iter = first_row_result
                .into_iter()
                .chain(py_iter)
                .map(|result| match result {
                    Ok(bound) => Ok(bound.unbind()),
                    Err(e) => Err(e),
                })
                .filter_map(|result: PyResult<Py<PyAny>>| match result {
                    Ok(tuple) => Some(tuple),
                    Err(e) => {
                        // TODO: This only logs the error, should we propagate it instead?
                        error!("bulkcopy: Error reading row from Python iterator: {:?}", e);
                        None
                    }
                });

            // Convert to PythonRowAdapter for each row
            let row_adapters = all_rows_iter.map(move |row| {
                PythonRowAdapter::with_metadata(
                    row,
                    Arc::clone(&metadata_arc),
                    Some(Arc::clone(&resolved_mappings_arc)),
                    first_row_col_count,
                )
            });

            // Execute bulk copy with zero-copy streaming
            info!("bulkcopy: Calling write_to_server_zerocopy");
            let bulk_result = bulk_copy
                .write_to_server_zerocopy(row_adapters)
                .await
                .map_err(|e| {
                    error!("bulkcopy: write_to_server_zerocopy failed: {}", e);
                    convert_tds_error(e)
                })?;
            info!(
                "bulkcopy: write_to_server_zerocopy completed successfully, rows_affected={}",
                bulk_result.rows_affected
            );

            Ok::<_, PyErr>(bulk_result)
        })?;

        // Convert result to Python dict
        let py_result = PyDict::new(py);
        py_result.set_item("rows_copied", result.rows_affected)?;

        // Calculate batch count
        let batch_count = if options.batch_size > 0 {
            result.rows_affected.div_ceil(options.batch_size as u64)
        } else {
            1
        };
        py_result.set_item("batch_count", batch_count)?;

        py_result.set_item("elapsed_time", result.elapsed.as_secs_f64())?;

        let rows_per_second = if result.elapsed.as_secs_f64() > 0.0 {
            result.rows_affected as f64 / result.elapsed.as_secs_f64()
        } else {
            0.0
        };
        py_result.set_item("rows_per_second", rows_per_second)?;

        Ok(py_result.into())
    }
}

impl PyCoreCursor {
    /// Convert a TDS ColumnValue to a Python object
    pub(crate) fn column_value_to_python<'py>(
        py: Python<'py>,
        col_val: &ColumnValues,
    ) -> Bound<'py, PyAny> {
        use pyo3::types::{PyBytes, PyList, PyModule};

        match col_val {
            ColumnValues::Null => py.None().into_bound(py),
            ColumnValues::Bit(b) => (*b).into_pyobject(py).unwrap().to_owned().into_any(),
            ColumnValues::TinyInt(i) => (*i).into_pyobject(py).unwrap().to_owned().into_any(),
            ColumnValues::SmallInt(i) => (*i).into_pyobject(py).unwrap().to_owned().into_any(),
            ColumnValues::Int(i) => (*i).into_pyobject(py).unwrap().to_owned().into_any(),
            ColumnValues::BigInt(i) => (*i).into_pyobject(py).unwrap().to_owned().into_any(),
            ColumnValues::Float(f) => (*f).into_pyobject(py).unwrap().to_owned().into_any(),
            ColumnValues::Real(f) => (*f as f64).into_pyobject(py).unwrap().to_owned().into_any(),
            ColumnValues::String(s) => s
                .to_utf8_string()
                .into_pyobject(py)
                .unwrap()
                .to_owned()
                .into_any(),
            ColumnValues::Uuid(u) => {
                // Convert Rust UUID to Python UUID object
                // Import uuid module and create UUID from bytes
                if let Ok(uuid_module) = PyModule::import(py, "uuid")
                    && let Ok(uuid_class) = uuid_module.getattr("UUID")
                {
                    // Convert UUID to bytes (16 bytes in RFC 4122 format)
                    let uuid_bytes = u.as_bytes();
                    let py_bytes = PyBytes::new(py, uuid_bytes);
                    // Call UUID(bytes=...) - use keyword argument
                    let kwargs = pyo3::types::PyDict::new(py);
                    if kwargs.set_item("bytes", py_bytes).is_ok()
                        && let Ok(uuid_obj) = uuid_class.call((), Some(&kwargs))
                    {
                        return uuid_obj.into_any();
                    }
                }
                // Fallback to string if UUID conversion fails
                u.to_string()
                    .into_pyobject(py)
                    .unwrap()
                    .to_owned()
                    .into_any()
            }
            ColumnValues::Bytes(b) => PyBytes::new(py, b).into_any(),
            ColumnValues::Vector(v) => {
                // Return Python list of floats for vectors
                match v.base_type() {
                    VectorBaseType::Float32 => {
                        if let Some(vals) = v.as_f32() {
                            let list = PyList::new(py, vals.iter().map(|f| *f as f64)).unwrap();
                            return list.into_any();
                        }
                    }
                }
                // Fallback to string if conversion fails
                format!("{:?}", v)
                    .into_pyobject(py)
                    .unwrap()
                    .to_owned()
                    .into_any()
            }
            ColumnValues::Numeric(n) | ColumnValues::Decimal(n) => {
                // Convert DecimalParts to Python Decimal object
                let decimal_str = n.to_string();
                if let Ok(decimal_module) = PyModule::import(py, "decimal")
                    && let Ok(decimal_class) = decimal_module.getattr("Decimal")
                    && let Ok(decimal_obj) = decimal_class.call1((decimal_str.as_str(),))
                {
                    return decimal_obj.into_any();
                }
                // Fallback to string if Decimal conversion fails
                decimal_str.into_pyobject(py).unwrap().to_owned().into_any()
            }
            ColumnValues::DateTime(dt) => {
                // Convert SqlDateTime to Python datetime.datetime object
                // SqlDateTime: days since 1900-01-01 (signed i32), time in 1/300th seconds (u32)
                if let Ok(datetime_module) = PyModule::import(py, "datetime")
                    && let Ok(date_class) = datetime_module.getattr("date")
                    && let Ok(datetime_class) = datetime_module.getattr("datetime")
                {
                    // Calculate the date by adding days to 1900-01-01
                    // 1900-01-01 has ordinal 693596 (days since 0001-01-01)
                    let base_ordinal: i32 = 693596;
                    let target_ordinal = base_ordinal + dt.days;

                    // Get the date from ordinal
                    if let Ok(date_obj) = date_class.call_method1("fromordinal", (target_ordinal,))
                    {
                        // Extract year, month, day from the date
                        if let (Ok(year), Ok(month), Ok(day)) = (
                            date_obj.getattr("year").and_then(|v| v.extract::<i32>()),
                            date_obj.getattr("month").and_then(|v| v.extract::<u8>()),
                            date_obj.getattr("day").and_then(|v| v.extract::<u8>()),
                        ) {
                            // Convert time ticks (1/300th seconds) to time components
                            let (hour, minute, second, microsecond) =
                                crate::types::ticks_to_time_components(dt.time);

                            if let Ok(datetime_obj) = datetime_class.call1((
                                year,
                                month,
                                day,
                                hour,
                                minute,
                                second,
                                microsecond,
                            )) {
                                return datetime_obj.into_any();
                            }
                        }
                    }
                }
                // Fallback to string if datetime conversion fails
                format!("{:?}", dt)
                    .into_pyobject(py)
                    .unwrap()
                    .to_owned()
                    .into_any()
            }
            ColumnValues::SmallDateTime(dt) => {
                // Convert SqlSmallDateTime to Python datetime.datetime object
                // SqlSmallDateTime: days since 1900-01-01 (unsigned u16), time in minutes (u16)
                if let Ok(datetime_module) = PyModule::import(py, "datetime")
                    && let Ok(date_class) = datetime_module.getattr("date")
                    && let Ok(datetime_class) = datetime_module.getattr("datetime")
                {
                    // Calculate the date by adding days to 1900-01-01
                    // 1900-01-01 has ordinal 693596 (days since 0001-01-01)
                    let base_ordinal: i32 = 693596;
                    let target_ordinal = base_ordinal + (dt.days as i32);

                    // Get the date from ordinal
                    if let Ok(date_obj) = date_class.call_method1("fromordinal", (target_ordinal,))
                    {
                        // Extract year, month, day from the date
                        if let (Ok(year), Ok(month), Ok(day)) = (
                            date_obj.getattr("year").and_then(|v| v.extract::<i32>()),
                            date_obj.getattr("month").and_then(|v| v.extract::<u8>()),
                            date_obj.getattr("day").and_then(|v| v.extract::<u8>()),
                        ) {
                            // Convert time in minutes to time components
                            let total_minutes = dt.time as u32;

                            let hour = (total_minutes / 60) as u8;
                            let minute = (total_minutes % 60) as u8;

                            if let Ok(datetime_obj) =
                                datetime_class.call1((year, month, day, hour, minute, 0, 0))
                            {
                                return datetime_obj.into_any();
                            }
                        }
                    }
                }
                // Fallback to string if datetime conversion fails
                format!("{:?}", dt)
                    .into_pyobject(py)
                    .unwrap()
                    .to_owned()
                    .into_any()
            }
            ColumnValues::Money(m) => {
                // Convert SqlMoney to Python Decimal object
                // Money values are stored as 8-byte integers representing units of 1/10000
                let lsb_in_i64 = (m.lsb_part as i64) & 0x00000000FFFFFFFF;
                let money_val = lsb_in_i64 | ((m.msb_part as i64) << 32);

                // Format as decimal string with 4 decimal places
                let integer_part = money_val / 10000;
                let fractional_part = (money_val % 10000).abs();
                let decimal_str = format!("{}.{:04}", integer_part, fractional_part);

                if let Ok(decimal_module) = PyModule::import(py, "decimal")
                    && let Ok(decimal_class) = decimal_module.getattr("Decimal")
                    && let Ok(decimal_obj) = decimal_class.call1((decimal_str.as_str(),))
                {
                    return decimal_obj.into_any();
                }
                // Fallback to string if Decimal conversion fails
                decimal_str.into_pyobject(py).unwrap().to_owned().into_any()
            }
            ColumnValues::SmallMoney(m) => {
                // Convert SqlSmallMoney to Python Decimal object
                // SmallMoney values are stored as 4-byte integers representing units of 1/10000
                let money_val = m.int_val as i64;

                // Format as decimal string with 4 decimal places
                let integer_part = money_val / 10000;
                let fractional_part = (money_val % 10000).abs();
                let decimal_str = format!("{}.{:04}", integer_part, fractional_part);

                if let Ok(decimal_module) = PyModule::import(py, "decimal")
                    && let Ok(decimal_class) = decimal_module.getattr("Decimal")
                    && let Ok(decimal_obj) = decimal_class.call1((decimal_str.as_str(),))
                {
                    return decimal_obj.into_any();
                }
                // Fallback to string if Decimal conversion fails
                decimal_str.into_pyobject(py).unwrap().to_owned().into_any()
            }
            ColumnValues::Date(sql_date) => {
                // Convert SqlDate to Python datetime.date object
                // SqlDate stores 0-based days since 0001-01-01 (date(1,1,1) = day 0)
                // Python's fromordinal() expects 1-based ordinals (date(1,1,1) = ordinal 1)
                // So we need to add 1 to convert back
                if let Ok(datetime_module) = PyModule::import(py, "datetime")
                    && let Ok(date_class) = datetime_module.getattr("date")
                {
                    // Add 1 to convert from 0-based days to 1-based ordinal
                    let ordinal = sql_date.get_days() + 1;
                    if let Ok(date_obj) = date_class.call_method1("fromordinal", (ordinal,)) {
                        return date_obj.into_any();
                    }
                }
                // Fallback to string if date conversion fails
                format!("{:?}", col_val)
                    .into_pyobject(py)
                    .unwrap()
                    .to_owned()
                    .into_any()
            }
            ColumnValues::Time(sql_time) => {
                // Convert SqlTime to Python datetime.time object
                // SqlTime stores time as 100-nanosecond units since midnight
                // We need to extract hours, minutes, seconds, and microseconds
                if let Ok(datetime_module) = PyModule::import(py, "datetime")
                    && let Ok(time_class) = datetime_module.getattr("time")
                {
                    let time_100ns = sql_time.time_nanoseconds;

                    // Convert 100-nanosecond units to components
                    // 1 hour = 36,000,000,000 units (100ns)
                    // 1 minute = 600,000,000 units (100ns)
                    // 1 second = 10,000,000 units (100ns)
                    // 1 microsecond = 10 units (100ns)

                    let hour = (time_100ns / 36_000_000_000) as u8;
                    let remainder = time_100ns % 36_000_000_000;

                    let minute = (remainder / 600_000_000) as u8;
                    let remainder = remainder % 600_000_000;

                    let second = (remainder / 10_000_000) as u8;
                    let remainder = remainder % 10_000_000;

                    let microsecond = (remainder / 10) as u32;

                    if let Ok(time_obj) = time_class.call1((hour, minute, second, microsecond)) {
                        return time_obj.into_any();
                    }
                }
                // Fallback to string if time conversion fails
                format!("{:?}", col_val)
                    .into_pyobject(py)
                    .unwrap()
                    .to_owned()
                    .into_any()
            }
            ColumnValues::DateTime2(dt2) => {
                // Convert SqlDateTime2 to Python datetime.datetime object
                // SqlDateTime2: days since 0001-01-01 (0-based, u32), time as SqlTime (100ns units)
                if let Ok(datetime_module) = PyModule::import(py, "datetime")
                    && let Ok(date_class) = datetime_module.getattr("date")
                    && let Ok(datetime_class) = datetime_module.getattr("datetime")
                {
                    // Calculate ordinal: SqlDateTime2.days is 0-based from 0001-01-01
                    // Python's fromordinal is 1-based (date(1,1,1) = ordinal 1)
                    // So we need to add 1
                    let ordinal = dt2.days + 1;

                    // Get the date from ordinal
                    if let Ok(date_obj) = date_class.call_method1("fromordinal", (ordinal,)) {
                        // Extract year, month, day from the date
                        if let (Ok(year), Ok(month), Ok(day)) = (
                            date_obj.getattr("year").and_then(|v| v.extract::<i32>()),
                            date_obj.getattr("month").and_then(|v| v.extract::<u8>()),
                            date_obj.getattr("day").and_then(|v| v.extract::<u8>()),
                        ) {
                            // Convert time from 100-nanosecond units to time components
                            // SqlTime.time_nanoseconds is already in 100ns units
                            let time_100ns = dt2.time.time_nanoseconds;

                            let hour = (time_100ns / 36_000_000_000) as u8;
                            let remainder = time_100ns % 36_000_000_000;

                            let minute = (remainder / 600_000_000) as u8;
                            let remainder = remainder % 600_000_000;

                            let second = (remainder / 10_000_000) as u8;
                            let remainder = remainder % 10_000_000;

                            let microsecond = (remainder / 10) as u32;

                            if let Ok(datetime_obj) = datetime_class.call1((
                                year,
                                month,
                                day,
                                hour,
                                minute,
                                second,
                                microsecond,
                            )) {
                                return datetime_obj.into_any();
                            }
                        }
                    }
                }
                // Fallback to string if datetime2 conversion fails
                format!("{:?}", dt2)
                    .into_pyobject(py)
                    .unwrap()
                    .to_owned()
                    .into_any()
            }
            ColumnValues::DateTimeOffset(dto) => {
                // Convert SqlDateTimeOffset to Python datetime.datetime object with timezone
                // SqlDateTimeOffset: datetime2 (SqlDateTime2) + offset (i16, minutes from UTC)
                if let Ok(datetime_module) = PyModule::import(py, "datetime")
                    && let Ok(date_class) = datetime_module.getattr("date")
                    && let Ok(datetime_class) = datetime_module.getattr("datetime")
                    && let Ok(timezone_class) = datetime_module.getattr("timezone")
                    && let Ok(timedelta_class) = datetime_module.getattr("timedelta")
                {
                    // Calculate ordinal: SqlDateTime2.days is 0-based from 0001-01-01
                    // Python's fromordinal is 1-based (date(1,1,1) = ordinal 1)
                    // So we need to add 1
                    let ordinal = dto.datetime2.days + 1;

                    // Get the date from ordinal
                    if let Ok(date_obj) = date_class.call_method1("fromordinal", (ordinal,)) {
                        // Extract year, month, day from the date
                        if let (Ok(year), Ok(month), Ok(day)) = (
                            date_obj.getattr("year").and_then(|v| v.extract::<i32>()),
                            date_obj.getattr("month").and_then(|v| v.extract::<u8>()),
                            date_obj.getattr("day").and_then(|v| v.extract::<u8>()),
                        ) {
                            // Convert time from 100-nanosecond units to time components
                            let time_100ns = dto.datetime2.time.time_nanoseconds;

                            let hour = (time_100ns / 36_000_000_000) as u8;
                            let remainder = time_100ns % 36_000_000_000;

                            let minute = (remainder / 600_000_000) as u8;
                            let remainder = remainder % 600_000_000;

                            let second = (remainder / 10_000_000) as u8;
                            let remainder = remainder % 10_000_000;

                            let microsecond = (remainder / 10) as u32;

                            // Create timezone from offset (minutes from UTC)
                            // Python's timezone expects a timedelta object
                            // timedelta(days, seconds, microseconds)
                            if let Ok(td_obj) =
                                timedelta_class.call1((0, dto.offset as i32 * 60, 0))
                                && let Ok(tz_obj) = timezone_class.call1((td_obj,))
                                && let Ok(datetime_obj) = datetime_class.call1((
                                    year,
                                    month,
                                    day,
                                    hour,
                                    minute,
                                    second,
                                    microsecond,
                                    tz_obj,
                                ))
                            {
                                return datetime_obj.into_any();
                            }
                        }
                    }
                }
                // Fallback to string if datetimeoffset conversion fails
                format!("{:?}", dto)
                    .into_pyobject(py)
                    .unwrap()
                    .to_owned()
                    .into_any()
            }
            ColumnValues::Json(json) => {
                // Return JSON as a plain string (not wrapped with debug format)
                json.as_string()
                    .into_pyobject(py)
                    .unwrap()
                    .to_owned()
                    .into_any()
            }
            ColumnValues::Xml(xml) => {
                // Return XML as a plain string (convert from UTF-16LE bytes to UTF-8 string)
                xml.as_string()
                    .into_pyobject(py)
                    .unwrap()
                    .to_owned()
                    .into_any()
            }
        }
    }

    /// Parse column mappings from Python list.
    ///
    /// Supports two formats:
    /// - `List[str]`: `['col1', 'col2']` — list index = source ordinal
    /// - `List[Tuple[int|str, str]]`: `[(0, 'col1'), ('src', 'dst')]` — explicit mapping
    fn parse_column_mappings(
        mappings_obj: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Vec<ColumnMapping>> {
        use pyo3::exceptions::PyTypeError;

        let Some(mappings) = mappings_obj else {
            return Ok(Vec::new());
        };
        if !mappings.is_instance_of::<PyList>() {
            return Err(PyTypeError::new_err("column_mappings must be a list"));
        }

        let list: &Bound<'_, PyList> = mappings.cast()?;
        if list.is_empty() {
            return Ok(Vec::new());
        }

        let first = list.get_item(0)?;
        if first.is_instance_of::<PyTuple>() {
            Self::parse_tuple_mappings(list)
        } else if first.extract::<String>().is_ok() {
            Self::parse_string_mappings(list)
        } else {
            Err(PyTypeError::new_err(
                "column_mappings elements must be str or (int|str, str) tuples",
            ))
        }
    }

    /// `['colA', 'colB']` → `[(0, colA), (1, colB)]`
    fn parse_string_mappings(list: &Bound<'_, PyList>) -> PyResult<Vec<ColumnMapping>> {
        use pyo3::exceptions::PyTypeError;

        list.iter()
            .enumerate()
            .map(|(i, item)| {
                let destination: String = item.extract().map_err(|_| {
                    PyTypeError::new_err(format!(
                        "Not all items in column mapping are the same type. column_mappings[{i}]: expected str, got {}",
                        item.get_type()
                    ))
                })?;
                Ok(ColumnMapping::ByOrdinal {
                    source: i,
                    destination,
                })
            })
            .collect()
    }

    /// `[(0, 'colA'), ('src', 'dst')]`
    fn parse_tuple_mappings(list: &Bound<'_, PyList>) -> PyResult<Vec<ColumnMapping>> {
        list.iter()
            .enumerate()
            .map(|(i, item)| Self::parse_one_tuple(i, &item))
            .collect()
    }

    fn parse_one_tuple(i: usize, item: &Bound<'_, PyAny>) -> PyResult<ColumnMapping> {
        use pyo3::exceptions::PyTypeError;

        if !item.is_instance_of::<PyTuple>() {
            return Err(PyTypeError::new_err(format!(
                "Not all items in column mapping are the same type. column_mappings[{i}]: expected tuple, got {}",
                item.get_type()
            )));
        }
        if item.len()? != 2 {
            return Err(PyTypeError::new_err(
                "Each mapping tuple must have exactly 2 elements: (source, destination)",
            ));
        }

        let source = item.get_item(0)?;
        let destination = item.get_item(1)?.extract::<String>()?;

        if let Ok(name) = source.extract::<String>() {
            Ok(ColumnMapping::ByName {
                source: name,
                destination,
            })
        } else if let Ok(ordinal) = source.extract::<usize>() {
            Ok(ColumnMapping::ByOrdinal {
                source: ordinal,
                destination,
            })
        } else {
            Err(PyTypeError::new_err(
                "Tuple source must be str (name) or int (ordinal)",
            ))
        }
    }

    fn __repr__(&self) -> String {
        "PyCoreCursor()".to_string()
    }
}

impl PyCoreCursor {
    pub fn new(tds_client: Arc<Mutex<TdsClient>>, runtime_handle: Handle) -> Self {
        Self {
            tds_client,
            runtime_handle,
            has_resultset: false,
        }
    }
}

/// Bulk copy options parsed from Python kwargs
#[derive(Debug, Clone)]
#[allow(dead_code)] // Will be used when implementing actual bulk copy
struct BulkCopyOptions {
    // Performance settings
    batch_size: usize,
    timeout: Duration,

    // Column mappings
    column_mappings: Vec<ColumnMapping>,

    // Bulk copy options
    keep_identity: bool,
    check_constraints: bool,
    table_lock: bool,
    keep_nulls: bool,
    fire_triggers: bool,
    use_internal_transaction: bool,
}

impl Default for BulkCopyOptions {
    fn default() -> Self {
        Self {
            batch_size: 0,
            timeout: Duration::from_secs(30),
            column_mappings: Vec::new(),
            keep_identity: false,
            check_constraints: false,
            table_lock: false,
            keep_nulls: false,
            fire_triggers: false,
            use_internal_transaction: false,
        }
    }
}

/// Column mapping from source to destination
#[derive(Debug, Clone)]
enum ColumnMapping {
    /// Map by column name
    ByName { source: String, destination: String },
    /// Map by column ordinal (0-based)
    ByOrdinal { source: usize, destination: String },
}
