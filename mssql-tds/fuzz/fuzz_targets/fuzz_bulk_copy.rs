// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz test for bulk copy (BCP) metadata and command generation.
//!
//! Tests pure functions in the bulk copy path:
//! - `build_insert_bulk_command()` — SQL command generation from metadata+options
//! - `BulkCopyColumnMetadata::get_sql_type_definition()` — type definition strings
//! - `BulkCopyOptions::validate()` — options validation
//!
//! Run with: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_bulk_copy

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use mssql_tds::connection::bulk_copy::BulkCopyOptions;
use mssql_tds::datatypes::bulk_copy_metadata::{BulkCopyColumnMetadata, SqlDbType, TypeLength};
use mssql_tds::fuzz_support::build_insert_bulk_command;

const MAX_TABLE_NAME_LEN: usize = 256;
const MAX_COLUMN_NAME_LEN: usize = 128;
const MAX_COLLATION_NAME_LEN: usize = 64;
const MAX_COLUMNS: usize = 20;

#[derive(Debug, Arbitrary)]
enum FuzzSqlDbType {
    BigInt,
    Int,
    SmallInt,
    TinyInt,
    Bit,
    Float,
    Real,
    Decimal,
    Numeric,
    Money,
    SmallMoney,
    Date,
    DateTime,
    DateTime2,
    DateTimeOffset,
    SmallDateTime,
    Time,
    Char,
    VarChar,
    Text,
    NChar,
    NVarChar,
    NText,
    Binary,
    VarBinary,
    Image,
    UniqueIdentifier,
    Xml,
    Variant,
    Udt,
    Json,
    Vector,
}

impl FuzzSqlDbType {
    fn to_sql_db_type(&self) -> SqlDbType {
        match self {
            Self::BigInt => SqlDbType::BigInt,
            Self::Int => SqlDbType::Int,
            Self::SmallInt => SqlDbType::SmallInt,
            Self::TinyInt => SqlDbType::TinyInt,
            Self::Bit => SqlDbType::Bit,
            Self::Float => SqlDbType::Float,
            Self::Real => SqlDbType::Real,
            Self::Decimal => SqlDbType::Decimal,
            Self::Numeric => SqlDbType::Numeric,
            Self::Money => SqlDbType::Money,
            Self::SmallMoney => SqlDbType::SmallMoney,
            Self::Date => SqlDbType::Date,
            Self::DateTime => SqlDbType::DateTime,
            Self::DateTime2 => SqlDbType::DateTime2,
            Self::DateTimeOffset => SqlDbType::DateTimeOffset,
            Self::SmallDateTime => SqlDbType::SmallDateTime,
            Self::Time => SqlDbType::Time,
            Self::Char => SqlDbType::Char,
            Self::VarChar => SqlDbType::VarChar,
            Self::Text => SqlDbType::Text,
            Self::NChar => SqlDbType::NChar,
            Self::NVarChar => SqlDbType::NVarChar,
            Self::NText => SqlDbType::NText,
            Self::Binary => SqlDbType::Binary,
            Self::VarBinary => SqlDbType::VarBinary,
            Self::Image => SqlDbType::Image,
            Self::UniqueIdentifier => SqlDbType::UniqueIdentifier,
            Self::Xml => SqlDbType::Xml,
            Self::Variant => SqlDbType::Variant,
            Self::Udt => SqlDbType::Udt,
            Self::Json => SqlDbType::Json,
            Self::Vector => SqlDbType::Vector,
        }
    }
}

#[derive(Debug, Arbitrary)]
enum FuzzTypeLength {
    Fixed(i32),
    Variable(i32),
    Plp,
    Unknown,
}

impl FuzzTypeLength {
    fn to_type_length(&self) -> TypeLength {
        match self {
            Self::Fixed(n) => TypeLength::Fixed(*n),
            Self::Variable(n) => TypeLength::Variable(*n),
            Self::Plp => TypeLength::Plp,
            Self::Unknown => TypeLength::Unknown,
        }
    }
}

#[derive(Debug, Arbitrary)]
struct FuzzColumnMetadata {
    column_name: String,
    sql_type: FuzzSqlDbType,
    tds_type: u8,
    length: i32,
    length_type: FuzzTypeLength,
    precision: u8,
    scale: u8,
    collation_name: Option<String>,
    is_nullable: bool,
    is_identity: bool,
}

impl FuzzColumnMetadata {
    fn sanitize(&mut self) {
        truncate_utf8(&mut self.column_name, MAX_COLUMN_NAME_LEN);
        if let Some(ref mut cn) = self.collation_name {
            truncate_utf8(cn, MAX_COLLATION_NAME_LEN);
        }
    }

    fn to_column_metadata(&self) -> BulkCopyColumnMetadata {
        let sql_type = self.sql_type.to_sql_db_type();

        let mut meta = BulkCopyColumnMetadata::new(&self.column_name, sql_type, self.tds_type)
            .with_length(self.length, self.length_type.to_type_length())
            .with_precision_scale(self.precision, self.scale)
            .with_nullable(self.is_nullable)
            .with_identity(self.is_identity);

        if let Some(ref cn) = self.collation_name {
            meta = meta.with_collation_name(cn);
        }

        meta
    }
}

#[derive(Debug, Arbitrary)]
struct FuzzBulkCopyOptions {
    batch_size: usize,
    timeout_sec: u32,
    check_constraints: bool,
    fire_triggers: bool,
    keep_identity: bool,
    keep_nulls: bool,
    table_lock: bool,
    use_internal_transaction: bool,
}

impl FuzzBulkCopyOptions {
    fn to_options(&self) -> BulkCopyOptions {
        BulkCopyOptions {
            batch_size: self.batch_size,
            timeout_sec: self.timeout_sec,
            check_constraints: self.check_constraints,
            fire_triggers: self.fire_triggers,
            keep_identity: self.keep_identity,
            keep_nulls: self.keep_nulls,
            table_lock: self.table_lock,
            use_internal_transaction: self.use_internal_transaction,
            notification_interval: 0,
            allow_encrypted_value_modifications: false,
        }
    }
}

#[derive(Debug, Arbitrary)]
struct FuzzBulkCopyInput {
    table_name: String,
    columns: Vec<FuzzColumnMetadata>,
    options: FuzzBulkCopyOptions,
}

impl FuzzBulkCopyInput {
    fn sanitize(&mut self) {
        truncate_utf8(&mut self.table_name, MAX_TABLE_NAME_LEN);
        if self.columns.len() > MAX_COLUMNS {
            self.columns.truncate(MAX_COLUMNS);
        }
        for col in &mut self.columns {
            col.sanitize();
        }
    }
}

fn truncate_utf8(s: &mut String, max_len: usize) {
    if s.len() > max_len {
        let mut pos = max_len;
        while pos > 0 && !s.is_char_boundary(pos) {
            pos -= 1;
        }
        s.truncate(pos);
    }
}

fuzz_target!(|data: &[u8]| {
    let mut input = match <FuzzBulkCopyInput as Arbitrary>::arbitrary(
        &mut arbitrary::Unstructured::new(data),
    ) {
        Ok(v) => v,
        Err(_) => return,
    };

    input.sanitize();

    let column_metadata: Vec<BulkCopyColumnMetadata> =
        input.columns.iter().map(|c| c.to_column_metadata()).collect();
    let options = input.options.to_options();

    // Fuzz build_insert_bulk_command — must not panic (errors are OK)
    let _ = build_insert_bulk_command(&input.table_name, &column_metadata, &options);

    // Fuzz get_sql_type_definition on each column — must not panic
    for col in &column_metadata {
        let _ = col.get_sql_type_definition();
    }

    // Fuzz BulkCopyOptions::validate — must not panic
    let _ = options.validate();

    // Fuzz needs_collation / needs_precision_scale / is_plp / is_long
    for col in &column_metadata {
        let _nc = col.needs_collation();
        let _nps = col.needs_precision_scale();
        let _plp = col.is_plp();
        let _long = col.is_long();
    }

    // Fuzz vector_dimensions on Vector columns — catch unexpected panics
    for col in &column_metadata {
        let _ = col.vector_dimensions();
    }
});
