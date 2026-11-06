// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! `serde::Deserialize` support for [`Row`].
//!
//! Gated behind the `serde` cargo feature. Enables ergonomic
//! row → struct mapping:
//!
//! ```rust,no_run
//! # #[cfg(feature = "serde")]
//! # async fn example(client: &mut mssql_rs::Client) -> mssql_rs::Result<()> {
//! use serde::Deserialize;
//! use mssql_rs::Row;
//!
//! #[derive(Deserialize)]
//! struct User {
//!     id: i64,
//!     name: String,
//!     email: Option<String>,
//! }
//!
//! let users: Vec<User> = client
//!     .query("SELECT id, name, email FROM users", &[])
//!     .await?
//!     .into_first_result()
//!     .into_iter()
//!     .map(Row::deserialize)
//!     .collect::<mssql_rs::Result<Vec<_>>>()?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Field-name mapping
//!
//! Struct fields are matched to columns by **exact name** (case-sensitive,
//! same lookup as [`Row::get`]). Use `#[serde(rename = "Name")]` to map to a
//! column whose name doesn't match the field identifier.
//!
//! ## Type mapping
//!
//! | TDS column type                          | Visited as                          |
//! |------------------------------------------|-------------------------------------|
//! | `bit`                                    | `bool`                              |
//! | `tinyint` / `smallint` / `int` / `bigint`| widening integer (`u8` → `i64`)     |
//! | `real` / `float`                         | `f32` / `f64`                       |
//! | `nvarchar` / `varchar` / `text` / `xml` / `json` | borrowed `&str` (UTF-8) |
//! | `varbinary` / `binary` / `image`         | borrowed `&[u8]`                    |
//! | `uniqueidentifier`                       | `&str` (lowercase hyphenated UUID)  |
//! | `date` / `time` / `datetime` / `datetime2` / `datetimeoffset` / `smalldatetime` | ISO-8601 `String` |
//! | `decimal` / `numeric`                    | `String` (`Display` of `DecimalParts`) |
//! | `money` / `smallmoney`                   | `f64`                               |
//! | `NULL`                                   | `Option::None` (when typed as `Option<T>`) or unit |
//!
//! Numeric coercion is forgiving: a column declared as `tinyint` will
//! deserialize into a struct field of any integer width up to `i64`.
//!
//! ## Borrowed deserialization
//!
//! For borrowed `&str` / `&[u8]` fields, use [`Row::deserialize_borrowed`]
//! which keeps the row alive for the lifetime of the resulting value.

use mssql_tds::datatypes::column_values::ColumnValues;
use serde::de::{
    self, DeserializeSeed, Deserializer, IntoDeserializer, MapAccess, SeqAccess, Visitor,
};

use crate::error::{Error, Result};
use crate::row::Row;

// ---------------------------------------------------------------------------
// serde::de::Error glue for our Error type
// ---------------------------------------------------------------------------

impl de::Error for Error {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Error::Conversion(msg.to_string())
    }
}

// ---------------------------------------------------------------------------
// Row -> Deserializer
// ---------------------------------------------------------------------------

/// `serde` deserializer that views a [`Row`] as a struct or map (column name → value)
/// or as a sequence (positional values).
pub struct RowDeserializer<'a> {
    row: &'a Row,
}

impl<'a> RowDeserializer<'a> {
    /// Wrap a row reference for serde deserialization.
    pub fn new(row: &'a Row) -> Self {
        Self { row }
    }
}

impl<'de, 'a: 'de> Deserializer<'de> for RowDeserializer<'a> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        // Default is map-style (struct-shaped consumers).
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_map(RowMapAccess {
            row: self.row,
            idx: 0,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_map(RowMapAccess {
            row: self.row,
            idx: 0,
        })
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_seq(RowSeqAccess {
            row: self.row,
            idx: 0,
        })
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct enum identifier ignored_any
    }
}

struct RowMapAccess<'a> {
    row: &'a Row,
    idx: usize,
}

impl<'de, 'a: 'de> MapAccess<'de> for RowMapAccess<'a> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        if self.idx >= self.row.len() {
            return Ok(None);
        }
        let name = self
            .row
            .columns()
            .get(self.idx)
            .ok_or(Error::ColumnIndexOutOfBounds {
                index: self.idx,
                count: self.row.columns().len(),
            })?
            .name
            .as_str();
        // Use IntoDeserializer for &str → produces a borrowed-str deserializer.
        seed.deserialize(name.into_deserializer()).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        let val = self
            .row
            .raw_value(self.idx)
            .ok_or(Error::ColumnIndexOutOfBounds {
                index: self.idx,
                count: self.row.len(),
            })?;
        let decoded = self.row.decoded_str_at(self.idx);
        let result = seed.deserialize(ColumnValueDeserializer { val, decoded });
        self.idx += 1;
        result
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.row.len() - self.idx)
    }
}

struct RowSeqAccess<'a> {
    row: &'a Row,
    idx: usize,
}

impl<'de, 'a: 'de> SeqAccess<'de> for RowSeqAccess<'a> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        if self.idx >= self.row.len() {
            return Ok(None);
        }
        let val = self
            .row
            .raw_value(self.idx)
            .ok_or(Error::ColumnIndexOutOfBounds {
                index: self.idx,
                count: self.row.len(),
            })?;
        let decoded = self.row.decoded_str_at(self.idx);
        let v = seed.deserialize(ColumnValueDeserializer { val, decoded })?;
        self.idx += 1;
        Ok(Some(v))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.row.len() - self.idx)
    }
}

// ---------------------------------------------------------------------------
// ColumnValues -> Deserializer
// ---------------------------------------------------------------------------

struct ColumnValueDeserializer<'a> {
    val: &'a ColumnValues,
    decoded: Option<&'a str>,
}

impl<'a> ColumnValueDeserializer<'a> {
    fn unsupported<T>(&self, target: &str) -> Result<T> {
        Err(Error::Conversion(format!(
            "cannot deserialize {:?} as {target}",
            std::mem::discriminant(self.val)
        )))
    }

    fn temporal_string(&self) -> Option<String> {
        // Reuse the chrono FromSql impls already in row.rs to render a stable
        // ISO-8601 representation. This keeps the deserializer in sync with
        // the rest of the bridge's temporal semantics.
        use crate::row::FromSql;
        match self.val {
            ColumnValues::Date(_) => {
                chrono::NaiveDate::from_sql(self.val).map(|d| d.format("%Y-%m-%d").to_string())
            }
            ColumnValues::Time(_) => {
                chrono::NaiveTime::from_sql(self.val).map(|t| t.format("%H:%M:%S%.f").to_string())
            }
            ColumnValues::DateTime(_)
            | ColumnValues::DateTime2(_)
            | ColumnValues::SmallDateTime(_) => chrono::NaiveDateTime::from_sql(self.val)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
            ColumnValues::DateTimeOffset(_) => {
                chrono::DateTime::<chrono::FixedOffset>::from_sql(self.val)
                    .map(|dt| dt.to_rfc3339())
            }
            _ => None,
        }
    }
}

macro_rules! visit_int {
    ($name:ident, $visit:ident, $t:ty) => {
        fn $name<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            // Normalize to i128 first to avoid signed/unsigned ambiguity.
            let wide: i128 = match self.val {
                ColumnValues::TinyInt(n) => *n as i128,
                ColumnValues::SmallInt(n) => *n as i128,
                ColumnValues::Int(n) => *n as i128,
                ColumnValues::BigInt(n) => *n as i128,
                ColumnValues::Bit(b) => *b as i128,
                _ => return self.unsupported(stringify!($t)),
            };
            let v: $t = <$t as ::std::convert::TryFrom<i128>>::try_from(wide).map_err(|_| {
                <Error as de::Error>::custom(format!(
                    "value {} out of range for {}",
                    wide,
                    stringify!($t)
                ))
            })?;
            visitor.$visit(v)
        }
    };
}

macro_rules! visit_float {
    ($name:ident, $visit:ident, $t:ty) => {
        fn $name<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            let v: $t = match self.val {
                ColumnValues::Real(n) => *n as $t,
                ColumnValues::Float(n) => *n as $t,
                ColumnValues::TinyInt(n) => *n as $t,
                ColumnValues::SmallInt(n) => *n as $t,
                ColumnValues::Int(n) => *n as $t,
                ColumnValues::BigInt(n) => *n as $t,
                ColumnValues::Money(m) => {
                    let f: mssql_tds::core::TdsResult<f64> = m.into();
                    f.map_err(|e| {
                        <Error as de::Error>::custom(format!("money decode failed: {e:?}"))
                    })? as $t
                }
                ColumnValues::SmallMoney(sm) => (sm.int_val as f64 / 10_000.0) as $t,
                _ => return self.unsupported(stringify!($t)),
            };
            visitor.$visit(v)
        }
    };
}

impl<'de, 'a: 'de> Deserializer<'de> for ColumnValueDeserializer<'a> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.val {
            ColumnValues::Null => visitor.visit_unit(),
            ColumnValues::Bit(b) => visitor.visit_bool(*b),
            ColumnValues::TinyInt(v) => visitor.visit_u8(*v),
            ColumnValues::SmallInt(v) => visitor.visit_i16(*v),
            ColumnValues::Int(v) => visitor.visit_i32(*v),
            ColumnValues::BigInt(v) => visitor.visit_i64(*v),
            ColumnValues::Real(v) => visitor.visit_f32(*v),
            ColumnValues::Float(v) => visitor.visit_f64(*v),
            ColumnValues::Bytes(b) => visitor.visit_borrowed_bytes(b),
            ColumnValues::Uuid(u) => visitor.visit_string(u.to_string()),
            ColumnValues::String(_) | ColumnValues::Xml(_) | ColumnValues::Json(_) => {
                match self.decoded {
                    Some(s) => visitor.visit_borrowed_str(s),
                    None => visitor.visit_borrowed_str(""),
                }
            }
            ColumnValues::Date(_)
            | ColumnValues::Time(_)
            | ColumnValues::DateTime(_)
            | ColumnValues::DateTime2(_)
            | ColumnValues::SmallDateTime(_)
            | ColumnValues::DateTimeOffset(_) => {
                let s = self
                    .temporal_string()
                    .ok_or_else(|| Error::Conversion("invalid temporal value".into()))?;
                visitor.visit_string(s)
            }
            ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => {
                visitor.visit_string(d.to_string())
            }
            ColumnValues::Money(m) => {
                let f: mssql_tds::core::TdsResult<f64> = m.into();
                let n = f.map_err(|e| {
                    <Error as de::Error>::custom(format!("money decode failed: {e:?}"))
                })?;
                visitor.visit_f64(n)
            }
            ColumnValues::SmallMoney(sm) => visitor.visit_f64(sm.int_val as f64 / 10_000.0),
            ColumnValues::Vector(_) => Err(Error::Conversion(
                "ColumnValues::Vector is not supported by serde Deserialize".into(),
            )),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.val {
            ColumnValues::Null => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.val {
            ColumnValues::Bit(b) => visitor.visit_bool(*b),
            _ => self.unsupported("bool"),
        }
    }

    visit_int!(deserialize_i8, visit_i8, i8);
    visit_int!(deserialize_i16, visit_i16, i16);
    visit_int!(deserialize_i32, visit_i32, i32);
    visit_int!(deserialize_i64, visit_i64, i64);
    visit_int!(deserialize_i128, visit_i128, i128);
    visit_int!(deserialize_u8, visit_u8, u8);
    visit_int!(deserialize_u16, visit_u16, u16);
    visit_int!(deserialize_u32, visit_u32, u32);
    visit_int!(deserialize_u64, visit_u64, u64);
    visit_int!(deserialize_u128, visit_u128, u128);

    visit_float!(deserialize_f32, visit_f32, f32);
    visit_float!(deserialize_f64, visit_f64, f64);

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.decoded {
            Some(s) => {
                let mut iter = s.chars();
                match (iter.next(), iter.next()) {
                    (Some(c), None) => visitor.visit_char(c),
                    _ => Err(Error::Conversion(
                        "cannot deserialize multi-char string as char".into(),
                    )),
                }
            }
            None => self.unsupported("char"),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.val {
            ColumnValues::String(_) | ColumnValues::Xml(_) | ColumnValues::Json(_) => {
                match self.decoded {
                    Some(s) => visitor.visit_borrowed_str(s),
                    None => visitor.visit_borrowed_str(""),
                }
            }
            ColumnValues::Uuid(u) => visitor.visit_string(u.to_string()),
            ColumnValues::Date(_)
            | ColumnValues::Time(_)
            | ColumnValues::DateTime(_)
            | ColumnValues::DateTime2(_)
            | ColumnValues::SmallDateTime(_)
            | ColumnValues::DateTimeOffset(_) => {
                let s = self
                    .temporal_string()
                    .ok_or_else(|| Error::Conversion("invalid temporal value".into()))?;
                visitor.visit_string(s)
            }
            ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => {
                visitor.visit_string(d.to_string())
            }
            _ => self.unsupported("&str"),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.val {
            ColumnValues::Bytes(b) => visitor.visit_borrowed_bytes(b),
            ColumnValues::Uuid(u) => visitor.visit_borrowed_bytes(u.as_bytes()),
            _ => self.unsupported("bytes"),
        }
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.val {
            ColumnValues::Bytes(b) => visitor.visit_byte_buf(b.clone()),
            ColumnValues::Uuid(u) => visitor.visit_byte_buf(u.as_bytes().to_vec()),
            _ => self.unsupported("byte buf"),
        }
    }

    fn deserialize_seq<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(Error::Conversion(
            "cannot deserialize a column value as a sequence".into(),
        ))
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(Error::Conversion(
            "cannot deserialize a column value as a map".into(),
        ))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        // Enums backed by their variant name as a string in the column.
        visitor.visit_enum(EnumStrAccess { de: self })
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_unit()
    }
}

struct EnumStrAccess<'a> {
    de: ColumnValueDeserializer<'a>,
}

impl<'de, 'a: 'de> de::EnumAccess<'de> for EnumStrAccess<'a> {
    type Error = Error;
    type Variant = UnitVariant;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant)> {
        let v = seed.deserialize(self.de)?;
        Ok((v, UnitVariant))
    }
}

struct UnitVariant;

impl<'de> de::VariantAccess<'de> for UnitVariant {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, _seed: T) -> Result<T::Value> {
        Err(Error::Conversion(
            "newtype enum variants are not supported on column values".into(),
        ))
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, _visitor: V) -> Result<V::Value> {
        Err(Error::Conversion(
            "tuple enum variants are not supported on column values".into(),
        ))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value> {
        Err(Error::Conversion(
            "struct enum variants are not supported on column values".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::{Column, ColumnType};
    use crate::row::RowSchema;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::datatypes::sql_string::SqlString;
    use serde::Deserialize;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn make_row(pairs: Vec<(&str, ColumnType, ColumnValues)>) -> Row {
        let columns: Vec<Column> = pairs
            .iter()
            .map(|(n, t, _)| Column::test_column(n, *t, 0))
            .collect();
        let values: Vec<ColumnValues> = pairs.into_iter().map(|(_, _, v)| v).collect();
        let name_map: HashMap<String, usize> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name().to_string(), i))
            .collect();
        let schema = Arc::new(RowSchema { columns, name_map });
        Row::from_schema(schema, values)
    }

    fn s(v: &str) -> ColumnValues {
        ColumnValues::String(SqlString::from_utf8_string(v.into()))
    }

    fn cell<'de, T: Deserialize<'de>>(
        value: &'de ColumnValues,
        decoded: Option<&'de str>,
    ) -> Result<T> {
        T::deserialize(ColumnValueDeserializer {
            val: value,
            decoded,
        })
    }

    #[test]
    fn row_map_rejects_values_without_column_metadata() {
        let schema = Arc::new(RowSchema {
            columns: vec![],
            name_map: HashMap::new(),
        });
        let row = Row::from_schema(schema, vec![ColumnValues::Int(1)]);
        let error = row
            .deserialize::<HashMap<String, i32>>()
            .expect_err("map keys require column metadata");
        assert!(matches!(
            error,
            Error::ColumnIndexOutOfBounds { index: 0, count: 0 }
        ));
    }

    #[test]
    fn integer_coercion_checks_bounds_and_rejects_non_integers() {
        macro_rules! check {
            ($($ty:ty),+ $(,)?) => {$(
                for value in [
                    ColumnValues::TinyInt(1), ColumnValues::SmallInt(1),
                    ColumnValues::Int(1), ColumnValues::BigInt(1), ColumnValues::Bit(true),
                ] {
                    assert_eq!(cell::<$ty>(&value, None).expect("one fits every integer type"), 1);
                }
                let error = cell::<$ty>(&s("1"), Some("1")).expect_err("strings are not integer values");
                assert!(error.to_string().contains(concat!("as ", stringify!($ty))));
            )+};
        }
        check!(i8, i16, i32, i64, i128, u8, u16, u32, u64, u128);
        assert_eq!(
            cell::<i64>(&ColumnValues::BigInt(i64::MIN), None).expect("i64 minimum fits an i64"),
            i64::MIN
        );
        assert_eq!(
            cell::<u8>(&ColumnValues::TinyInt(255), None).expect("255 fits a u8"),
            255
        );
        assert!(
            cell::<i8>(&ColumnValues::TinyInt(128), None)
                .expect_err("128 exceeds the i8 range")
                .to_string()
                .contains("128 out of range for i8")
        );
        assert!(
            cell::<u64>(&ColumnValues::Int(-1), None)
                .expect_err("negative integers cannot deserialize as u64")
                .to_string()
                .contains("-1 out of range for u64")
        );
        cell::<i32>(&ColumnValues::BigInt(i64::MAX), None)
            .expect_err("i64 maximum exceeds the i32 range");
        assert!(
            cell::<bool>(&ColumnValues::Int(1), None)
                .expect_err("integer columns cannot deserialize as bool")
                .to_string()
                .contains("as bool")
        );
    }

    #[test]
    fn floating_coercion_includes_money_and_integer_sources() {
        use mssql_tds::datatypes::column_values::{SqlMoney, SqlSmallMoney};
        for (value, expected) in [
            (ColumnValues::Real(1.5), 1.5),
            (ColumnValues::Float(-2.5), -2.5),
            (ColumnValues::TinyInt(255), 255.0),
            (ColumnValues::SmallInt(-12), -12.0),
            (ColumnValues::Int(123), 123.0),
            (ColumnValues::BigInt(-456), -456.0),
            (
                ColumnValues::Money(SqlMoney {
                    msb_part: 0,
                    lsb_part: 12500,
                }),
                1.25,
            ),
            (
                ColumnValues::SmallMoney(SqlSmallMoney { int_val: -12500 }),
                -1.25,
            ),
        ] {
            let single = cell::<f32>(&value, None).expect("numeric column converts to f32");
            let double = cell::<f64>(&value, None).expect("numeric column converts to f64");
            let dynamic = cell::<serde_json::Value>(&value, None)
                .expect("numeric column converts to JSON")
                .as_f64()
                .expect("numeric JSON value converts to f64");
            assert!((single - expected as f32).abs() < f32::EPSILON);
            assert!((double - expected).abs() < f64::EPSILON);
            assert!((dynamic - expected).abs() < f64::EPSILON);
        }
        cell::<f32>(&ColumnValues::Null, None).expect_err("NULL is not a float");
        cell::<f64>(&ColumnValues::Bit(true), None).expect_err("bit columns are not floats");
    }

    #[test]
    fn dynamic_values_and_temporal_strings_have_stable_representations() {
        use mssql_tds::datatypes::column_values::{
            SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlSmallDateTime, SqlTime,
        };
        use mssql_tds::datatypes::decoder::DecimalParts;
        let time = || SqlTime {
            time_nanoseconds: 12_345_678,
            scale: 7,
        };
        let cases = [
            (
                ColumnValues::Date(SqlDate::create(0).expect("day zero is a valid SQL date")),
                "0001-01-01",
            ),
            (ColumnValues::Time(time()), "00:00:01.234567800"),
            (
                ColumnValues::DateTime(SqlDateTime { days: 0, time: 300 }),
                "1900-01-01T00:00:01",
            ),
            (
                ColumnValues::SmallDateTime(SqlSmallDateTime { days: 0, time: 61 }),
                "1900-01-01T01:01:00",
            ),
            (
                ColumnValues::DateTime2(SqlDateTime2 {
                    days: 0,
                    time: time(),
                }),
                "0001-01-01T00:00:01.234567800",
            ),
            (
                ColumnValues::DateTimeOffset(SqlDateTimeOffset {
                    datetime2: SqlDateTime2 {
                        days: 0,
                        time: time(),
                    },
                    offset: 60,
                }),
                "0001-01-01T01:00:01.234567800+01:00",
            ),
            (
                ColumnValues::Decimal(
                    DecimalParts::from_string("-12.50", 4, 2)
                        .expect("negative decimal fixture fits precision and scale"),
                ),
                "-12.50",
            ),
            (
                ColumnValues::Numeric(
                    DecimalParts::from_string("12.50", 4, 2)
                        .expect("positive decimal fixture fits precision and scale"),
                ),
                "12.50",
            ),
            (
                ColumnValues::Uuid(uuid::Uuid::nil()),
                "00000000-0000-0000-0000-000000000000",
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(
                cell::<String>(&value, None).expect("column has a string representation"),
                expected
            );
            assert_eq!(
                cell::<serde_json::Value>(&value, None).expect("column converts to JSON"),
                serde_json::json!(expected)
            );
        }
        assert_eq!(
            cell::<serde_json::Value>(&ColumnValues::Null, None)
                .expect("NULL converts to JSON null"),
            serde_json::Value::Null
        );
        assert_eq!(
            cell::<serde_json::Value>(&ColumnValues::Bit(true), None)
                .expect("bit converts to JSON boolean"),
            serde_json::json!(true)
        );
        assert_eq!(
            cell::<serde_json::Value>(&s("héllo"), Some("héllo"))
                .expect("Unicode string converts to JSON"),
            serde_json::json!("héllo")
        );
        let invalid = ColumnValues::Time(SqlTime {
            time_nanoseconds: 864_000_000_000,
            scale: 7,
        });
        for error in [
            cell::<String>(&invalid, None).expect_err("invalid time cannot render as a string"),
            cell::<serde_json::Value>(&invalid, None)
                .expect_err("invalid time cannot render as JSON"),
        ] {
            assert!(error.to_string().contains("invalid temporal value"));
        }
        assert!(
            cell::<String>(&ColumnValues::Int(1), None)
                .expect_err("integer columns cannot deserialize as strings")
                .to_string()
                .contains("as &str")
        );
    }

    #[test]
    fn characters_and_binary_values_validate_shape_and_borrow_storage() {
        assert_eq!(
            cell::<char>(&s("🦀"), Some("🦀")).expect("one Unicode scalar converts to char"),
            '🦀'
        );
        for text in ["", "ab"] {
            assert!(
                cell::<char>(&s(text), Some(text))
                    .expect_err("char requires exactly one Unicode scalar")
                    .to_string()
                    .contains("multi-char")
            );
        }
        assert!(
            cell::<char>(&ColumnValues::Int(1), None)
                .expect_err("integer columns cannot deserialize as char")
                .to_string()
                .contains("as char")
        );
        let original = vec![0, 255, 42];
        let original_ptr = original.as_ptr();
        let bytes = ColumnValues::Bytes(original);
        let borrowed: &[u8] = cell(&bytes, None).expect("binary column lends its bytes");
        assert_eq!(borrowed, &[0, 255, 42]);
        assert_eq!(borrowed.as_ptr(), original_ptr);
        let id = uuid::Uuid::from_bytes([42; 16]);
        let value = ColumnValues::Uuid(id);
        assert_eq!(
            cell::<&[u8]>(&value, None).expect("UUID column lends its bytes"),
            id.as_bytes()
        );
        assert!(
            cell::<&[u8]>(&ColumnValues::Int(1), None)
                .expect_err("integer columns cannot deserialize as bytes")
                .to_string()
                .contains("as bytes")
        );

        struct OwnedBytesVisitor;
        impl<'de> Visitor<'de> for OwnedBytesVisitor {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("owned binary data")
            }
            fn visit_byte_buf<E: de::Error>(
                self,
                v: Vec<u8>,
            ) -> std::result::Result<Self::Value, E> {
                Ok(v)
            }
        }
        for (value, expected) in [(&bytes, vec![0, 255, 42]), (&value, vec![42; 16])] {
            assert_eq!(
                ColumnValueDeserializer {
                    val: value,
                    decoded: None
                }
                .deserialize_byte_buf(OwnedBytesVisitor)
                .expect("binary and UUID columns deserialize to owned bytes"),
                expected
            );
        }
        assert!(
            ColumnValueDeserializer {
                val: &ColumnValues::Null,
                decoded: None
            }
            .deserialize_byte_buf(OwnedBytesVisitor)
            .expect_err("NULL cannot deserialize to owned bytes")
            .to_string()
            .contains("as byte buf")
        );
    }

    #[test]
    fn row_maps_sequences_and_newtypes_preserve_shape() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Pair(i32, String);
        #[derive(Debug, Deserialize, PartialEq)]
        struct Wrapped(Pair);
        let row = make_row(vec![
            ("n", ColumnType::Int4, ColumnValues::Int(7)),
            ("text", ColumnType::NVarchar, s("seven")),
        ]);
        assert_eq!(
            row.clone()
                .deserialize::<Wrapped>()
                .expect("row deserializes through a newtype wrapper"),
            Wrapped(Pair(7, "seven".into()))
        );
        let dynamic = row
            .deserialize::<serde_json::Value>()
            .expect("row deserializes as a JSON object");
        assert_eq!(dynamic, serde_json::json!({"n": 7, "text": "seven"}));
        let row = make_row(vec![
            ("a", ColumnType::Int4, ColumnValues::Int(1)),
            ("b", ColumnType::Int4, ColumnValues::Int(2)),
        ]);
        assert_eq!(
            row.clone()
                .deserialize::<Vec<i32>>()
                .expect("integer row deserializes as a sequence"),
            vec![1, 2]
        );
        assert_eq!(
            row.deserialize::<HashMap<String, i32>>()
                .expect("integer row deserializes as a map"),
            HashMap::from([("a".into(), 1), ("b".into(), 2)])
        );
        assert!(
            make_row(vec![])
                .deserialize::<Vec<i32>>()
                .expect("empty row deserializes as an empty sequence")
                .is_empty()
        );

        #[derive(Debug, Deserialize, PartialEq)]
        struct Number(i32);
        #[derive(Debug, Deserialize, PartialEq)]
        struct Unit;
        assert_eq!(
            cell::<Number>(&ColumnValues::Int(7), None)
                .expect("integer column deserializes through a newtype"),
            Number(7)
        );
        assert_eq!(
            cell::<Unit>(&ColumnValues::Null, None).expect("NULL deserializes as unit"),
            Unit
        );
    }

    #[test]
    fn column_containers_and_data_carrying_enum_variants_are_rejected() {
        #[derive(Debug, Deserialize)]
        #[expect(
            dead_code,
            reason = "payload variants exercise unsupported deserialization"
        )]
        enum State {
            Ready,
            Number(i32),
            Pair(i32, i32),
            Named { value: i32 },
        }
        assert!(matches!(
            cell::<State>(&s("Ready"), Some("Ready")).expect("unit enum variants are supported"),
            State::Ready
        ));
        for (variant, expected) in [
            ("Number", "newtype enum"),
            ("Pair", "tuple enum"),
            ("Named", "struct enum"),
        ] {
            assert!(
                cell::<State>(&s(variant), Some(variant))
                    .expect_err("data-carrying enum variants are unsupported")
                    .to_string()
                    .contains(expected)
            );
        }
        assert!(
            cell::<State>(&s("Unknown"), Some("Unknown"))
                .expect_err("unknown enum variants are rejected")
                .to_string()
                .contains("unknown variant")
        );
        let value = ColumnValues::Int(1);
        #[derive(Debug, Deserialize)]
        #[expect(
            dead_code,
            reason = "fields exercise unsupported tuple deserialization"
        )]
        struct Pair(i32, i32);
        #[derive(Debug, Deserialize)]
        #[expect(
            dead_code,
            reason = "field exercises unsupported struct deserialization"
        )]
        struct Record {
            value: i32,
        }
        for error in [
            cell::<Vec<i32>>(&value, None).expect_err("column cannot deserialize as a sequence"),
            cell::<(i32, i32)>(&value, None).expect_err("column cannot deserialize as a tuple"),
            cell::<Pair>(&value, None).expect_err("column cannot deserialize as a tuple struct"),
        ] {
            assert!(error.to_string().contains("as a sequence"));
        }
        for error in [
            cell::<HashMap<String, i32>>(&value, None)
                .expect_err("column cannot deserialize as a map"),
            cell::<Record>(&value, None).expect_err("column cannot deserialize as a struct"),
        ] {
            assert!(error.to_string().contains("as a map"));
        }
    }

    #[test]
    fn deserialize_basic_struct() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct User {
            id: i64,
            name: String,
            active: bool,
        }
        let row = make_row(vec![
            ("id", ColumnType::Int8, ColumnValues::BigInt(42)),
            ("name", ColumnType::NVarchar, s("alice")),
            ("active", ColumnType::Bit, ColumnValues::Bit(true)),
        ]);
        let u: User = row.deserialize().expect("row matches the User fields");
        assert_eq!(
            u,
            User {
                id: 42,
                name: "alice".to_string(),
                active: true,
            }
        );
    }

    #[test]
    fn deserialize_option_handles_null() {
        #[derive(Deserialize)]
        struct R {
            email: Option<String>,
            phone: Option<String>,
        }
        let row = make_row(vec![
            ("email", ColumnType::NVarchar, s("a@b")),
            ("phone", ColumnType::NVarchar, ColumnValues::Null),
        ]);
        let v: R = row
            .deserialize()
            .expect("nullable columns match Option fields");
        assert_eq!(v.email.as_deref(), Some("a@b"));
        assert_eq!(v.phone, None);
    }

    #[test]
    fn deserialize_borrowed_spatial_bytes() {
        #[derive(Deserialize)]
        struct Spatial<'a> {
            geography: &'a [u8],
            geometry: &'a [u8],
            #[serde(borrow)]
            missing: Option<&'a [u8]>,
        }
        let row = make_row(vec![
            (
                "geography",
                ColumnType::Geography,
                ColumnValues::Bytes(vec![1, 2]),
            ),
            (
                "geometry",
                ColumnType::Geometry,
                ColumnValues::Bytes(vec![3, 4]),
            ),
            ("missing", ColumnType::Geography, ColumnValues::Null),
        ]);
        let value: Spatial<'_> = row
            .deserialize_borrowed()
            .expect("spatial columns lend their binary storage");
        assert_eq!(value.geography, &[1, 2]);
        assert_eq!(value.geometry, &[3, 4]);
        assert_eq!(value.missing, None);
        assert_eq!(
            value.geography.as_ptr(),
            row.get::<&[u8], _>("geography")
                .expect("geography column contains bytes")
                .as_ptr()
        );
    }

    #[test]
    fn deserialize_widens_integers() {
        #[derive(Deserialize)]
        struct R {
            tiny: i64,
            small: i64,
            int_: i64,
            big: i64,
        }
        let row = make_row(vec![
            ("tiny", ColumnType::Int1, ColumnValues::TinyInt(7)),
            ("small", ColumnType::Int2, ColumnValues::SmallInt(-3)),
            ("int_", ColumnType::Int4, ColumnValues::Int(1_000_000)),
            ("big", ColumnType::Int8, ColumnValues::BigInt(i64::MAX)),
        ]);
        let r: R = row.deserialize().expect("integer columns widen to i64");
        assert_eq!(r.tiny, 7);
        assert_eq!(r.small, -3);
        assert_eq!(r.int_, 1_000_000);
        assert_eq!(r.big, i64::MAX);
    }

    #[test]
    fn deserialize_floats() {
        #[derive(Deserialize)]
        struct R {
            r: f32,
            f: f64,
        }
        let row = make_row(vec![
            ("r", ColumnType::Float4, ColumnValues::Real(1.5)),
            (
                "f",
                ColumnType::Float8,
                ColumnValues::Float(std::f64::consts::PI),
            ),
        ]);
        let r: R = row
            .deserialize()
            .expect("float columns match the float fields");
        assert!((r.r - 1.5).abs() < 1e-6);
        assert!((r.f - std::f64::consts::PI).abs() < 1e-12);
    }

    #[test]
    fn deserialize_uuid_as_string() {
        #[derive(Deserialize)]
        struct R {
            id: String,
        }
        let id = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000")
            .expect("UUID fixture is valid");
        let row = make_row(vec![("id", ColumnType::Guid, ColumnValues::Uuid(id))]);
        let r: R = row
            .deserialize()
            .expect("UUID column deserializes as a string");
        assert_eq!(r.id, "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn deserialize_borrowed_str() {
        #[derive(Deserialize)]
        struct R<'a> {
            name: &'a str,
        }
        let row = make_row(vec![("name", ColumnType::NVarchar, s("borrowed"))]);
        let r: R<'_> = row
            .deserialize_borrowed()
            .expect("string column lends its decoded storage");
        assert_eq!(r.name, "borrowed");
    }

    #[test]
    fn deserialize_temporal_as_string() {
        #[derive(Deserialize)]
        struct R {
            d: String,
        }
        // SqlDate::create takes days-since-0001-01-01. Pick a known date.
        let date_val = mssql_tds::datatypes::column_values::SqlDate::create(737_790)
            .expect("date fixture is within the SQL date range");
        let row = make_row(vec![("d", ColumnType::Date, ColumnValues::Date(date_val))]);
        let r: R = row
            .deserialize()
            .expect("date column renders as an ISO date");
        assert_eq!(r.d.len(), 10);
        assert_eq!(r.d.get(4..5), Some("-"));
        assert_eq!(r.d.get(7..8), Some("-"));
    }

    #[test]
    fn deserialize_missing_column_field_errors() {
        #[derive(Debug, Deserialize)]
        #[expect(
            dead_code,
            reason = "fields exercise missing-field deserialization errors"
        )]
        struct R {
            a: i32,
            b: i32,
        }
        let row = make_row(vec![("a", ColumnType::Int4, ColumnValues::Int(1))]);
        let res: Result<R> = row.deserialize();
        let error = res.expect_err("row lacks the required field b");
        assert!(error.to_string().contains("missing field `b`"));
    }

    #[test]
    fn deserialize_tuple_positional() {
        let row = make_row(vec![
            ("x", ColumnType::Int4, ColumnValues::Int(1)),
            ("y", ColumnType::NVarchar, s("two")),
            ("z", ColumnType::Bit, ColumnValues::Bit(false)),
        ]);
        let t: (i32, String, bool) = row
            .deserialize()
            .expect("row columns match the positional tuple");
        assert_eq!(t, (1, "two".to_string(), false));
    }

    #[test]
    fn deserialize_extra_columns_ignored() {
        // Struct asks for only `a`, row has `a` and `b`. Should succeed.
        #[derive(Deserialize)]
        struct R {
            a: i32,
        }
        let row = make_row(vec![
            ("a", ColumnType::Int4, ColumnValues::Int(11)),
            ("b", ColumnType::Int4, ColumnValues::Int(22)),
        ]);
        let r: R = row.deserialize().expect("extra columns are ignored");
        assert_eq!(r.a, 11);
    }

    #[test]
    fn deserialize_renamed_field() {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "FullName")]
            full_name: String,
        }
        let row = make_row(vec![("FullName", ColumnType::NVarchar, s("Ada Lovelace"))]);
        let r: R = row
            .deserialize()
            .expect("renamed field matches the column name");
        assert_eq!(r.full_name, "Ada Lovelace");
    }
}
