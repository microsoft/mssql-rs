// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use adbc_core::error::Result;
use adbc_core::options::{InfoCode, ObjectDepth};
use adbc_core::schemas::{GET_INFO_SCHEMA, GET_OBJECTS_SCHEMA, GET_TABLE_TYPES_SCHEMA};
use arrow_array::{
    ArrayRef, BooleanArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray, UInt32Array,
    UnionArray, new_empty_array,
};
use arrow_buffer::ScalarBuffer;
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use mssql_tds::connection::tds_client::{ExecuteOptions, ResultSet, StatementResult};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::escape_identifier;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
use serde_json::{Value, json};

use crate::driver::{Reader, Shared};
use crate::error::{data, tds, unsupported};
use crate::parameters;
use crate::types::datetime_fields;

pub(crate) fn batch_reader(batch: RecordBatch) -> Reader {
    let schema = batch.schema();
    Box::new(RecordBatchIterator::new([Ok(batch)], schema))
}

pub(crate) fn text_param(index: usize, value: Option<&str>) -> RpcParameter {
    RpcParameter::new(
        Some(format!("@p{index}")),
        StatusFlags::NONE,
        SqlType::NVarcharMax(value.map(|value| SqlString::from_utf8_string(value.to_owned()))),
    )
}

pub(crate) fn text(value: &ColumnValues) -> Result<Option<String>> {
    match value {
        ColumnValues::Null => Ok(None),
        ColumnValues::String(value) => {
            let encoding = value
                .encoding_type()
                .resolved_encoding()
                .ok_or_else(|| data("unresolved metadata encoding"))?;
            let (decoded, malformed) = encoding.decode_without_bom_handling(&value.bytes);
            if malformed {
                return Err(data("malformed metadata text"));
            }
            Ok(Some(decoded.into_owned()))
        }
        _ => Err(data("expected SQL metadata text")),
    }
}

pub(crate) fn number(value: &ColumnValues) -> Result<Option<i64>> {
    Ok(match value {
        ColumnValues::Null => None,
        ColumnValues::TinyInt(value) => Some(i64::from(*value)),
        ColumnValues::SmallInt(value) => Some(i64::from(*value)),
        ColumnValues::Int(value) => Some(i64::from(*value)),
        ColumnValues::BigInt(value) => Some(*value),
        ColumnValues::Bit(value) => Some(i64::from(*value)),
        _ => return Err(data("expected SQL metadata integer")),
    })
}

pub(crate) struct Rows {
    columns: Vec<String>,
    pub(crate) values: Vec<Vec<ColumnValues>>,
}

impl Rows {
    fn index(&self, name: &str) -> Result<usize> {
        self.columns
            .iter()
            .position(|column| column == name)
            .ok_or_else(|| data(format!("server omitted metadata column {name}")))
    }
}

pub(crate) fn query(shared: &Shared, sql: &str, params: Vec<RpcParameter>) -> Result<Rows> {
    shared.with(|executor, session| {
        session.idle()?;
        executor.run(async {
            let operation = async {
                let mut result = if params.is_empty() {
                    session
                        .client
                        .execute(
                            sql.to_owned(),
                            ExecuteOptions::new().timeout_secs(shared.config.timeout),
                        )
                        .await
                } else {
                    session
                        .client
                        .execute_sp_executesql(
                            sql.to_owned(),
                            params,
                            ExecuteOptions::new().timeout_secs(shared.config.timeout),
                        )
                        .await
                }
                .map_err(tds)?;
                let mut columns = Vec::new();
                let mut values = Vec::new();
                loop {
                    match result {
                        StatementResult::Rows => {
                            if !columns.is_empty() {
                                return Err(unsupported("multiple metadata result sets"));
                            }
                            columns = session
                                .client
                                .get_metadata()
                                .iter()
                                .map(|column| column.column_name.clone())
                                .collect();
                            while let Some(row) = session.client.next_row().await.map_err(tds)? {
                                values.push(row);
                            }
                        }
                        StatementResult::End => return Ok(Rows { columns, values }),
                        _ => {}
                    }
                    result = session.client.advance().await.map_err(tds)?;
                }
            }
            .await;
            if operation.is_err()
                && session.client.has_open_batch()
                && let Err(error) = session.abandon().await
            {
                tracing::error!(%error, "ADBC metadata cleanup failed");
            }
            operation
        })
    })
}

pub(crate) fn sql_field(name: &str, sql_type: &str, nullable: bool) -> Result<Field> {
    let lower = sql_type.to_ascii_lowercase();
    let base = lower.split('(').next().unwrap_or(&lower).trim();
    let ty = match base {
        "bit" => DataType::Boolean,
        "tinyint" => DataType::UInt8,
        "smallint" => DataType::Int16,
        "int" => DataType::Int32,
        "bigint" => DataType::Int64,
        "real" => DataType::Float32,
        "float" => DataType::Float64,
        "decimal" | "numeric" => {
            let args = lower
                .split_once('(')
                .and_then(|(_, args)| args.strip_suffix(')'))
                .ok_or_else(|| data("decimal description missing precision"))?;
            let (precision, scale) = args
                .split_once(',')
                .ok_or_else(|| data("decimal description missing scale"))?;
            DataType::Decimal128(
                precision
                    .trim()
                    .parse()
                    .map_err(|_| data("invalid decimal precision"))?,
                scale
                    .trim()
                    .parse()
                    .map_err(|_| data("invalid decimal scale"))?,
            )
        }
        "money" => DataType::Decimal128(19, 4),
        "smallmoney" => DataType::Decimal128(10, 4),
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" | "xml" | "json" => {
            DataType::Utf8
        }
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion" => DataType::Binary,
        "uniqueidentifier" => DataType::FixedSizeBinary(16),
        "date" => DataType::Date32,
        "time" => DataType::Time64(TimeUnit::Nanosecond),
        "datetime" | "smalldatetime" => DataType::Timestamp(TimeUnit::Microsecond, None),
        "datetime2" => DataType::Struct(datetime_fields(false)),
        "datetimeoffset" => DataType::Struct(datetime_fields(true)),
        _ => return Err(unsupported(format!("SQL type {sql_type}"))),
    };
    Ok(Field::new(name, ty, nullable).with_metadata(
        [("mssql.sql_type".into(), sql_type.to_owned())]
            .into_iter()
            .collect::<std::collections::HashMap<String, String>>(),
    ))
}

pub(crate) fn describe(
    shared: &Shared,
    sql: &str,
    bound: Option<&Schema>,
    parameters_only: bool,
) -> Result<Schema> {
    let declaration = bound.map(parameters::declarations).transpose()?;
    let rows = query(
        shared,
        if parameters_only {
            "EXEC sys.sp_describe_undeclared_parameters @tsql=@p1"
        } else {
            "EXEC sys.sp_describe_first_result_set @tsql=@p1, @params=@p2"
        },
        if parameters_only {
            vec![text_param(1, Some(sql))]
        } else {
            vec![
                text_param(1, Some(sql)),
                text_param(2, declaration.as_deref()),
            ]
        },
    )?;
    let name = rows.index("name")?;
    let sql_type = rows.index(if parameters_only {
        "suggested_system_type_name"
    } else {
        "system_type_name"
    })?;
    let hidden = if parameters_only {
        None
    } else {
        Some(rows.index("is_hidden")?)
    };
    let nullable = if parameters_only {
        None
    } else {
        Some(rows.index("is_nullable")?)
    };
    let mut fields = Vec::new();
    for row in &rows.values {
        if hidden.is_some_and(|index| number(&row[index]).ok().flatten() == Some(1)) {
            continue;
        }
        let name = text(&row[name])?.unwrap_or_default();
        let nullable = nullable
            .map(|index| number(&row[index]))
            .transpose()?
            .flatten()
            != Some(0);
        fields.push(match text(&row[sql_type])? {
            Some(sql_type) => sql_field(&name, &sql_type, nullable)?,
            None if parameters_only => Field::new(name, DataType::Null, true),
            None => return Err(data("server could not describe result type")),
        });
    }
    Ok(Schema::new(fields))
}

pub(crate) fn table_schema(
    shared: &Shared,
    catalog: Option<&str>,
    schema: Option<&str>,
    table: &str,
) -> Result<Schema> {
    let parts: Vec<_> = catalog
        .into_iter()
        .chain(schema.or(Some("dbo")))
        .chain([table])
        .map(escape_identifier)
        .collect();
    describe(
        shared,
        &format!("SELECT * FROM {}", parts.join(".")),
        None,
        false,
    )
}

pub(crate) fn table_types() -> Result<Reader> {
    Ok(batch_reader(RecordBatch::try_new(
        GET_TABLE_TYPES_SCHEMA.clone(),
        vec![Arc::new(StringArray::from(vec!["BASE TABLE", "VIEW"]))],
    )?))
}

pub(crate) fn info(shared: &Shared, requested: Option<HashSet<InfoCode>>) -> Result<Reader> {
    let version = shared.with(|_, session| {
        Ok(session
            .client
            .server_version()
            .map(|version| format!("{version:?}")))
    })?;
    let codes = [
        InfoCode::VendorName,
        InfoCode::VendorVersion,
        InfoCode::VendorSql,
        InfoCode::VendorSubstrait,
        InfoCode::DriverName,
        InfoCode::DriverVersion,
        InfoCode::DriverAdbcVersion,
    ];
    let mut names = Vec::new();
    let mut ids = Vec::new();
    let mut offsets = Vec::new();
    let mut strings = Vec::new();
    let mut booleans = Vec::new();
    let mut integers = Vec::new();
    for code in codes {
        if requested
            .as_ref()
            .is_some_and(|requested| !requested.contains(&code))
        {
            continue;
        }
        names.push(u32::from(&code));
        match code {
            InfoCode::VendorSql | InfoCode::VendorSubstrait => {
                ids.push(1);
                offsets.push(booleans.len() as i32);
                booleans.push(code == InfoCode::VendorSql);
            }
            InfoCode::DriverAdbcVersion => {
                ids.push(2);
                offsets.push(integers.len() as i32);
                integers.push(1_001_000_i64);
            }
            _ => {
                ids.push(0);
                offsets.push(strings.len() as i32);
                strings.push(match code {
                    InfoCode::VendorName => Some("Microsoft SQL Server".to_owned()),
                    InfoCode::VendorVersion => version.clone(),
                    InfoCode::DriverName => Some("mssql-adbc".to_owned()),
                    InfoCode::DriverVersion => Some(env!("CARGO_PKG_VERSION").to_owned()),
                    _ => return Err(data("unexpected info code")),
                });
            }
        }
    }
    let DataType::Union(fields, _) = GET_INFO_SCHEMA.field(1).data_type() else {
        return Err(data("invalid upstream info schema"));
    };
    let mut arrays: Vec<ArrayRef> = fields
        .iter()
        .map(|(_, field)| new_empty_array(field.data_type()))
        .collect();
    arrays[0] = Arc::new(StringArray::from(strings));
    arrays[1] = Arc::new(BooleanArray::from(booleans));
    arrays[2] = Arc::new(Int64Array::from(integers));
    let union = UnionArray::try_new(
        fields.clone(),
        ScalarBuffer::from(ids),
        Some(ScalarBuffer::from(offsets)),
        arrays,
    )?;
    Ok(batch_reader(RecordBatch::try_new(
        GET_INFO_SCHEMA.clone(),
        vec![Arc::new(UInt32Array::from(names)), Arc::new(union)],
    )?))
}

fn json_reader(schema: SchemaRef, value: Value) -> Result<Reader> {
    let bytes = serde_json::to_vec(&value).map_err(|error| data(error.to_string()))?;
    Ok(Box::new(
        arrow_json::ReaderBuilder::new(schema).build(std::io::Cursor::new(bytes))?,
    ))
}

pub(crate) fn objects(
    shared: &Shared,
    depth: ObjectDepth,
    catalog: Option<&str>,
    db_schema: Option<&str>,
    table: Option<&str>,
    types: Option<Vec<&str>>,
    column: Option<&str>,
) -> Result<Reader> {
    let catalog_rows = query(
        shared,
        "SELECT DB_NAME() AS catalog_name WHERE @p1 IS NULL OR DB_NAME() LIKE @p1 ESCAPE '\\'",
        vec![text_param(1, catalog)],
    )?;
    let Some(catalog_row) = catalog_rows.values.first() else {
        return Ok(Box::new(RecordBatchIterator::new(
            std::iter::empty(),
            GET_OBJECTS_SCHEMA.clone(),
        )));
    };
    let catalog = text(&catalog_row[0])?.ok_or_else(|| data("current catalog missing"))?;
    if depth == ObjectDepth::Catalogs {
        return json_reader(
            GET_OBJECTS_SCHEMA.clone(),
            json!({"catalog_name":catalog,"catalog_db_schemas":null}),
        );
    }
    let rows = query(
        shared,
        "SELECT name FROM sys.schemas WHERE @p1 IS NULL OR name LIKE @p1 ESCAPE '\\' ORDER BY name",
        vec![text_param(1, db_schema)],
    )?;
    let mut schemas: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for row in rows.values {
        schemas.insert(
            text(&row[0])?.ok_or_else(|| data("schema name missing"))?,
            BTreeMap::new(),
        );
    }
    if depth != ObjectDepth::Schemas {
        let rows = query(
            shared,
            "SELECT SCHEMA_NAME(o.schema_id),o.name,CASE o.type WHEN 'U' THEN 'BASE TABLE' ELSE 'VIEW' END FROM sys.objects o WHERE o.type IN ('U','V') AND o.is_ms_shipped=0 AND (@p1 IS NULL OR o.name LIKE @p1 ESCAPE '\\')",
            vec![text_param(1, table)],
        )?;
        for row in rows.values {
            let schema = text(&row[0])?.ok_or_else(|| data("schema name missing"))?;
            let name = text(&row[1])?.ok_or_else(|| data("table name missing"))?;
            let ty = text(&row[2])?.ok_or_else(|| data("table type missing"))?;
            if types
                .as_ref()
                .is_some_and(|types| !types.contains(&ty.as_str()))
            {
                continue;
            }
            if let Some(tables) = schemas.get_mut(&schema) {
                tables.insert(name.clone(), json!({"table_name":name,"table_type":ty,
                    "table_columns": if depth == ObjectDepth::Tables { Value::Null } else { json!([]) },
                    "table_constraints": if depth == ObjectDepth::Tables { Value::Null } else { json!([]) }}));
            }
        }
    }
    if matches!(depth, ObjectDepth::All | ObjectDepth::Columns) {
        let rows = query(
            shared,
            "SELECT SCHEMA_NAME(o.schema_id),o.name,c.name,c.column_id,TYPE_NAME(c.system_type_id),c.max_length,c.precision,c.scale,c.is_nullable,c.is_identity,c.is_computed,dc.definition FROM sys.objects o JOIN sys.columns c ON c.object_id=o.object_id LEFT JOIN sys.default_constraints dc ON dc.object_id=c.default_object_id WHERE o.type IN ('U','V') AND (@p1 IS NULL OR c.name LIKE @p1 ESCAPE '\\') ORDER BY o.schema_id,o.object_id,c.column_id",
            vec![text_param(1, column)],
        )?;
        for row in rows.values {
            let schema = text(&row[0])?.ok_or_else(|| data("schema name missing"))?;
            let table = text(&row[1])?.ok_or_else(|| data("table name missing"))?;
            if let Some(table) = schemas
                .get_mut(&schema)
                .and_then(|tables| tables.get_mut(&table))
            {
                let columns = table["table_columns"]
                    .as_array_mut()
                    .ok_or_else(|| data("invalid catalog column list"))?;
                let nullable = number(&row[8])? == Some(1);
                columns.push(json!({"column_name":text(&row[2])?,"ordinal_position":number(&row[3])?,
                    "xdbc_type_name":text(&row[4])?,"xdbc_char_octet_length":number(&row[5])?,
                    "xdbc_decimal_digits":number(&row[7])?,"xdbc_nullable":if nullable {1} else {0},
                    "xdbc_is_nullable":if nullable {"YES"} else {"NO"},"xdbc_is_autoincrement":number(&row[9])?==Some(1),
                    "xdbc_is_generatedcolumn":number(&row[10])?==Some(1),"xdbc_column_def":text(&row[11])?}));
            }
        }
        add_constraints(shared, &mut schemas)?;
    }
    let schemas: Vec<_> = schemas.into_iter().map(|(name, tables)| json!({"db_schema_name":name,
        "db_schema_tables":if depth == ObjectDepth::Schemas {Value::Null} else {json!(tables.into_values().collect::<Vec<_>>())}})).collect();
    json_reader(
        GET_OBJECTS_SCHEMA.clone(),
        json!({"catalog_name":catalog,"catalog_db_schemas":schemas}),
    )
}

fn add_constraints(
    shared: &Shared,
    schemas: &mut BTreeMap<String, BTreeMap<String, Value>>,
) -> Result<()> {
    let rows = query(
        shared,
        "SELECT SCHEMA_NAME(t.schema_id),t.name,k.name,CASE k.type WHEN 'PK' THEN 'PRIMARY KEY' ELSE 'UNIQUE' END,c.name,CAST(NULL AS nvarchar(128)),CAST(NULL AS nvarchar(128)),CAST(NULL AS nvarchar(128)),CAST(NULL AS nvarchar(128)),ic.key_ordinal FROM sys.key_constraints k JOIN sys.tables t ON t.object_id=k.parent_object_id JOIN sys.index_columns ic ON ic.object_id=t.object_id AND ic.index_id=k.unique_index_id AND ic.key_ordinal>0 JOIN sys.columns c ON c.object_id=t.object_id AND c.column_id=ic.column_id UNION ALL SELECT SCHEMA_NAME(t.schema_id),t.name,f.name,'FOREIGN KEY',c.name,DB_NAME(),SCHEMA_NAME(rt.schema_id),rt.name,rc.name,fc.constraint_column_id FROM sys.foreign_keys f JOIN sys.tables t ON t.object_id=f.parent_object_id JOIN sys.foreign_key_columns fc ON fc.constraint_object_id=f.object_id JOIN sys.columns c ON c.object_id=t.object_id AND c.column_id=fc.parent_column_id JOIN sys.tables rt ON rt.object_id=f.referenced_object_id JOIN sys.columns rc ON rc.object_id=rt.object_id AND rc.column_id=fc.referenced_column_id UNION ALL SELECT SCHEMA_NAME(t.schema_id),t.name,k.name,'CHECK',c.name,NULL,NULL,NULL,NULL,0 FROM sys.check_constraints k JOIN sys.tables t ON t.object_id=k.parent_object_id LEFT JOIN sys.columns c ON c.object_id=t.object_id AND c.column_id=k.parent_column_id ORDER BY 1,2,3,10",
        vec![],
    )?;
    let mut grouped: BTreeMap<(String, String, String), Value> = BTreeMap::new();
    for row in rows.values {
        let schema = text(&row[0])?.ok_or_else(|| data("constraint schema missing"))?;
        let table = text(&row[1])?.ok_or_else(|| data("constraint table missing"))?;
        let name = text(&row[2])?.ok_or_else(|| data("constraint name missing"))?;
        let ty = text(&row[3])?.ok_or_else(|| data("constraint type missing"))?;
        let constraint = grouped
            .entry((schema, table, name.clone()))
            .or_insert_with(|| {
                json!({
            "constraint_name":name,"constraint_type":ty,"constraint_column_names":[],
            "constraint_column_usage": if ty=="FOREIGN KEY" {json!([])} else {Value::Null}})
            });
        if let Some(column) = text(&row[4])? {
            constraint["constraint_column_names"]
                .as_array_mut()
                .ok_or_else(|| data("invalid constraint columns"))?
                .push(json!(column));
        }
        if ty == "FOREIGN KEY" {
            constraint["constraint_column_usage"].as_array_mut().ok_or_else(|| data("invalid FK usage"))?.push(json!({
                "fk_catalog":text(&row[5])?,"fk_db_schema":text(&row[6])?,"fk_table":text(&row[7])?,"fk_column_name":text(&row[8])?}));
        }
    }
    for ((schema, table, _), constraint) in grouped {
        if let Some(table) = schemas
            .get_mut(&schema)
            .and_then(|tables| tables.get_mut(&table))
        {
            table["table_constraints"]
                .as_array_mut()
                .ok_or_else(|| data("invalid constraint list"))?
                .push(constraint);
        }
    }
    Ok(())
}
