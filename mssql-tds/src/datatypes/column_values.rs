// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::core::TdsResult;
use crate::datatypes::decoder::DecimalParts;
use crate::datatypes::sql_json::SqlJson;
use crate::datatypes::sql_string::SqlString;
use crate::datatypes::sql_vector::SqlVector;
use crate::error::Error;
use core::fmt;
use std::fmt::Debug;
use uuid::Uuid;

/// SQL Server `xml` column value stored as UTF-16LE bytes.
#[derive(Debug, PartialOrd, PartialEq, Clone)]
pub struct SqlXml {
    /// UTF-16LE encoded XML bytes.
    pub bytes: Vec<u8>,
}

impl SqlXml {
    /// Decodes the UTF-16LE bytes into a Rust `String`.
    pub fn as_string(&self) -> String {
        let mut u16_buffer = Vec::with_capacity(self.bytes.len() / 2);
        self.bytes
            .chunks(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .for_each(|item| u16_buffer.push(item));

        String::from_utf16(&u16_buffer).unwrap()
    }

    /// Returns `true` if a UTF-16LE BOM (0xFFFE) is present.
    pub fn has_bom(&self) -> bool {
        self.bytes.len() >= 2 && (self.bytes[0] == 0xFF && self.bytes[1] == 0xFE)
    }
}

impl From<String> for SqlXml {
    fn from(input: String) -> SqlXml {
        let mut bytes = Vec::with_capacity(input.len() * 2);
        input
            .encode_utf16()
            .for_each(|item| bytes.extend_from_slice(&item.to_le_bytes()));
        SqlXml { bytes }
    }
}

/// Decoded column value from a TDS result row.
#[derive(Debug, PartialEq, Clone)]
pub enum ColumnValues {
    /// `tinyint` (0–255).
    TinyInt(u8),
    /// `smallint`.
    SmallInt(i16),
    /// `int`.
    Int(i32),
    /// `bigint`.
    BigInt(i64),
    /// `real` (single-precision float).
    Real(f32),
    /// `float` (double-precision float).
    Float(f64),
    /// `decimal`.
    Decimal(DecimalParts),
    /// `numeric`.
    Numeric(DecimalParts),
    /// `bit`.
    Bit(bool),
    /// Character string (`varchar`, `nvarchar`, `char`, `nchar`, `text`, `ntext`).
    String(SqlString),
    /// `datetime`.
    DateTime(SqlDateTime),
    /// `date`.
    Date(SqlDate),
    /// `time`.
    Time(SqlTime),
    /// `datetime2`.
    DateTime2(SqlDateTime2),
    /// `datetimeoffset`.
    DateTimeOffset(SqlDateTimeOffset),
    /// `smalldatetime`.
    SmallDateTime(SqlSmallDateTime),
    /// `smallmoney`.
    SmallMoney(SqlSmallMoney),
    /// `money`.
    Money(SqlMoney),
    /// Binary data (`binary`, `varbinary`, `image`).
    Bytes(Vec<u8>),
    /// `xml`.
    Xml(SqlXml),
    /// SQL `NULL`.
    Null,
    /// `uniqueidentifier`.
    Uuid(Uuid),
    /// `json`.
    Json(SqlJson),
    /// `vector`.
    Vector(SqlVector),
}

/// Default fractional-seconds scale (100 ns) for `time`, `datetime2`, and `datetimeoffset`.
pub const DEFAULT_VARTIME_SCALE: u8 = 7;

/// TDS `time` value with configurable fractional-seconds precision.
#[derive(Debug, PartialEq, Clone)]
pub struct SqlTime {
    /// 100-nanosecond ticks since midnight (independent of `scale`).
    pub time_nanoseconds: u64,
    /// Fractional-seconds scale (0–7).
    pub scale: u8,
}

impl SqlTime {
    pub(crate) fn get_scale(&self) -> u8 {
        self.scale
    }
}

/// TDS `datetime2` value combining a day count with a [`SqlTime`].
#[derive(Debug, PartialEq, Clone)]
pub struct SqlDateTime2 {
    /// Days since 0001-01-01.
    pub days: u32,
    /// Time component.
    pub time: SqlTime,
}

/// TDS `datetimeoffset` value: a [`SqlDateTime2`] plus a UTC-offset in minutes.
#[derive(Debug, PartialEq, Clone)]
pub struct SqlDateTimeOffset {
    /// Date-time component.
    pub datetime2: SqlDateTime2,
    /// UTC offset in minutes.
    pub offset: i16,
}

/// TDS `smallmoney` value stored as a scaled 32-bit integer (×10⁴).
#[derive(PartialEq, Clone, Debug)]
pub struct SqlSmallMoney {
    /// Scaled integer value (÷10⁴ for actual amount).
    pub int_val: i32,
}
impl From<i32> for SqlSmallMoney {
    fn from(value: i32) -> Self {
        SqlSmallMoney { int_val: value }
    }
}
/// TDS `money` value stored as two 32-bit integers in mixed-endian layout.
#[derive(PartialEq, Clone)]
pub struct SqlMoney {
    /// Lower 32 bits in mixed-endian layout.
    pub lsb_part: i32, // LSB
    /// Upper 32 bits (0 for `smallmoney`).
    pub msb_part: i32, // MSB - Only populated for Money, 0 for SmallMoney
}
impl Debug for SqlMoney {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Money value: {:?}, int_part_1: {:?}, int_part_2: {:?}",
            TdsResult::<f64>::from(self).unwrap(),
            self.lsb_part,
            self.msb_part
        )
    }
}
impl From<(i32, i32)> for SqlMoney {
    fn from(value: (i32, i32)) -> Self {
        SqlMoney {
            lsb_part: value.0,
            msb_part: value.1,
        }
    }
}
impl From<i32> for SqlMoney {
    fn from(value: i32) -> Self {
        SqlMoney {
            lsb_part: value,
            msb_part: 0,
        }
    }
}
// This function reassembles the two 4-byte integers (in mixed endian format) into a single 8-byte signed integer.
// The resulting value is the TDS money value, which can be divided by 10^4 to get the actual money value.
// (See comments in MoneyParts definition for more details)
impl From<&SqlMoney> for TdsResult<f64> {
    fn from(value: &SqlMoney) -> Self {
        let lsb = value.lsb_part;
        let msb = value.msb_part;
        // -----Example:------
        // While this logic works on both little and big endian machines, this example assumes
        // a little endian machine. Coz big endian case is trivial.
        // 1) Hex representation of an 8-byte int value (MSB to LSB):
        //       - 11 22 33 44 55 66 77 88
        // 2) 8-byte int value stored in LE machine (Low Mem address First (LMF)):
        //       - 88 77 66 55 44 33 22 11
        // 3) This int value stored in TDS wire-format as two 4-byte integers (mixed endian, LMF):
        //       - 44 33 22 11, 88 77 66 55 (MSB = 44 33 22 11, LSB = 88 77 66 55)
        // *** We have (3) in variables msb and lsb. We need to reassemble it into (2) ***
        // - lsb as i64 =
        //       - +ve LSB: 88 77 66 55 00 00 00 00 (LMF)
        //       - -ve LSB: 88 77 66 55 ff ff ff ff (LMF)
        // - (lsb as i64) & 0x00000000FFFFFFFF = lsb_in_i64 = 88 77 66 55 00 00 00 00 (LMF)
        //       - This step is to handle -ve LSB case. We need to convert the ff ff ff ff MSB bytes
        //         to 00 00 00 00. This is done by masking the LSB with 0x00000000FFFFFFFF.
        // - (msb as i64) << 32 = 00 00 00 00 44 33 22 11 (LMF)
        // - (lsb_in_i64) | ((msb as i64) << 32) = 88 77 66 55 44 33 22 11 (LMF)
        let lsb_in_i64 = (lsb as i64) & 0x00000000FFFFFFFF;
        let money_val = lsb_in_i64 | ((msb as i64) << 32);
        // TDS value of money is the value multiplied by 10^4, hence we need to divide while decoding.
        // TODO: (value as f64) can cause precision loss
        Ok((money_val as f64) / 10000.0000)
    }
}

impl From<&SqlMoney> for TdsResult<f32> {
    fn from(value: &SqlMoney) -> Self {
        // TDS value of money is the value multiplied by 10^4, hence we need to divide while decoding.
        let scaled_value = (value.lsb_part as f64) / 10000.0000; // f64 so that we don't lose precision
        Ok(scaled_value as f32) // Post division, money value  must fit in f32
        // TODO: For max (& min) value of smallmoney (214748.3647), the f32 value is 214748.36, which is not accurate. Debug & fix this.
        //       See test test_money_no_panic. Trying to query these max values from SSMS or ODBC gives correct value.
    }
}

/// TDS `smalldatetime` value: days since 1900-01-01 and minutes since midnight.
#[derive(Debug, PartialEq, Clone)]
pub struct SqlSmallDateTime {
    /// Days since 1900-01-01.
    // One 2-byte unsigned integer that represents the number of days since January 1, 1900.
    pub days: u16,

    /// Minutes since midnight.
    // One 2-byte unsigned integer that represents the number of minutes elapsed since 12 AM that day.
    pub time: u16,
}

/// TDS `datetime` value: days since 1900-01-01 and 1/300-second ticks since midnight.
#[derive(Debug, PartialEq, Clone)]
pub struct SqlDateTime {
    /// Signed days since 1900-01-01 (negative for dates back to 1753).
    // One 4-byte signed integer that represents the number of days since January 1, 1900.
    // Negative numbers are allowed to represent dates since January 1, 1753.
    pub days: i32,

    /// 1/300-second ticks since midnight.
    // One 4-byte unsigned integer that represents the number of one
    // three-hundredths of a second (300 counts per second) elapsed since 12 AM that day.
    pub time: u32,
}

/// Represents a date in SQL Server, which is stored as the number of days since January 1, year 1.
/// This is a 3-byte unsigned integer, allowing for a range of dates from January 1, 0001 to December 31, 9999.
/// The reason for this struct is to evolve it to provide validation for
/// creation of Date type. We dont want a 4 byte u32 assigned to days,
/// which is invalid.
#[derive(Debug, PartialEq, Clone)]
pub struct SqlDate {
    // date is represented as one 3-byte unsigned integer that represents the number of days since January 1, year 1.
    days_since_01_01_0001: u32,
}

impl SqlDate {
    // SQL Server DATE range: 0001-01-01 to 9999-12-31
    // Days are counted from 0 (0001-01-01 = day 0, 0001-01-02 = day 1, etc.)
    const MIN_DAYS: u32 = 0; // 0001-01-01
    const MAX_DAYS: u32 = 3_652_058; // 9999-12-31

    /// Creates a validated `SqlDate` from the number of days since 0001-01-01.
    pub fn create(days: u32) -> TdsResult<SqlDate> {
        if days <= Self::MAX_DAYS {
            Ok(SqlDate {
                days_since_01_01_0001: days,
            })
        } else {
            Err(Error::UsageError(format!(
                "Date value {} is out of range for DATE column. Valid range: {} to {}",
                days,
                Self::MIN_DAYS,
                Self::MAX_DAYS
            )))
        }
    }

    // This function is unsafe because it allows the creation of a SqlDate without checking the range.
    // It should only be used when you are sure that the value is within the valid range
    // (0 to 0xFFFFFF). This is meant for scenarios where SQL server is sending
    // us the date value and we need to create a SqlDate from it without validation.
    pub(crate) fn unchecked_create(days: u32) -> SqlDate {
        SqlDate {
            days_since_01_01_0001: days,
        }
    }

    /// Returns the number of days since 0001-01-01.
    pub fn get_days(&self) -> u32 {
        self.days_since_01_01_0001
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sql_xml_from_string() {
        let xml_str = "<root><item>test</item></root>".to_string();
        let xml = SqlXml::from(xml_str.clone());
        assert_eq!(xml.as_string(), xml_str);
    }

    #[test]
    fn test_sql_xml_has_bom() {
        let xml_with_bom = SqlXml {
            bytes: vec![0xFF, 0xFE, 0x3C, 0x00],
        };
        assert!(xml_with_bom.has_bom());

        let xml_without_bom = SqlXml {
            bytes: vec![0x3C, 0x00],
        };
        assert!(!xml_without_bom.has_bom());
    }

    #[test]
    fn test_sql_xml_empty() {
        let xml = SqlXml { bytes: Vec::new() };
        assert!(!xml.has_bom());
    }

    #[test]
    fn test_sql_time_get_scale() {
        let time = SqlTime {
            time_nanoseconds: 123456789,
            scale: 5,
        };
        assert_eq!(time.get_scale(), 5);
    }

    #[test]
    fn test_sql_datetime2_creation() {
        let time = SqlTime {
            time_nanoseconds: 1000000,
            scale: 7,
        };
        let datetime2 = SqlDateTime2 {
            days: 18000,
            time: time.clone(),
        };
        assert_eq!(datetime2.days, 18000);
        assert_eq!(datetime2.time.scale, 7);
    }

    #[test]
    fn test_sql_datetimeoffset_creation() {
        let time = SqlTime {
            time_nanoseconds: 5000000,
            scale: 7,
        };
        let datetime2 = SqlDateTime2 { days: 20000, time };
        let dto = SqlDateTimeOffset {
            datetime2,
            offset: -300,
        };
        assert_eq!(dto.offset, -300);
        assert_eq!(dto.datetime2.days, 20000);
    }

    #[test]
    fn test_sql_small_money_from_i32() {
        let money = SqlSmallMoney::from(100000);
        assert_eq!(money.int_val, 100000);
    }

    #[test]
    fn test_sql_money_from_i32() {
        let money = SqlMoney::from(100000);
        assert_eq!(money.lsb_part, 100000);
        assert_eq!(money.msb_part, 0);
    }

    #[test]
    fn test_sql_money_from_tuple() {
        let money = SqlMoney::from((100000, 50000));
        assert_eq!(money.lsb_part, 100000);
        assert_eq!(money.msb_part, 50000);
    }

    #[test]
    fn test_sql_money_to_f64() {
        let money = SqlMoney::from(100000);
        let value: TdsResult<f64> = (&money).into();
        assert!(value.is_ok());
        assert_eq!(value.unwrap(), 10.0);
    }

    #[test]
    fn test_sql_money_to_f32() {
        let money = SqlMoney::from(50000);
        let value: TdsResult<f32> = (&money).into();
        assert!(value.is_ok());
        assert_eq!(value.unwrap(), 5.0);
    }

    #[test]
    fn test_sql_small_datetime_creation() {
        let sdt = SqlSmallDateTime {
            days: 365,
            time: 720,
        };
        assert_eq!(sdt.days, 365);
        assert_eq!(sdt.time, 720);
    }

    #[test]
    fn test_sql_datetime_creation() {
        let dt = SqlDateTime {
            days: 365,
            time: 12345,
        };
        assert_eq!(dt.days, 365);
        assert_eq!(dt.time, 12345);
    }

    #[test]
    fn test_sql_date_create_valid() {
        let result = SqlDate::create(100000);
        assert!(result.is_ok());
        let date = result.unwrap();
        assert_eq!(date.get_days(), 100000);
    }

    #[test]
    fn test_sql_date_create_max_valid() {
        // The SQL Server DATE type max is 9999-12-31 = 3,652,058 days since 0001-01-01
        let result = SqlDate::create(3_652_058);
        assert!(result.is_ok());
    }

    #[test]
    fn test_sql_date_create_invalid() {
        // Any value above 3,652,058 should fail
        let result = SqlDate::create(3_652_059);
        assert!(result.is_err());

        // 0xFFFFFF is far beyond the valid range
        let result_hex = SqlDate::create(0x1000000);
        assert!(result_hex.is_err());
    }

    #[test]
    fn test_sql_date_unchecked_create() {
        let date = SqlDate::unchecked_create(200000);
        assert_eq!(date.get_days(), 200000);
    }

    #[test]
    fn test_column_values_tinyint() {
        let val = ColumnValues::TinyInt(255);
        assert!(matches!(val, ColumnValues::TinyInt(255)));
    }

    #[test]
    fn test_column_values_smallint() {
        let val = ColumnValues::SmallInt(-1000);
        assert!(matches!(val, ColumnValues::SmallInt(-1000)));
    }

    #[test]
    fn test_column_values_int() {
        let val = ColumnValues::Int(123456);
        assert!(matches!(val, ColumnValues::Int(123456)));
    }

    #[test]
    fn test_column_values_bigint() {
        let val = ColumnValues::BigInt(9223372036854775807);
        assert!(matches!(val, ColumnValues::BigInt(_)));
    }

    #[test]
    fn test_column_values_real() {
        let val = ColumnValues::Real(2.5);
        assert!(matches!(val, ColumnValues::Real(_)));
    }

    #[test]
    fn test_column_values_float() {
        let val = ColumnValues::Float(2.5);
        assert!(matches!(val, ColumnValues::Float(_)));
    }

    #[test]
    fn test_column_values_bit() {
        let val = ColumnValues::Bit(true);
        assert!(matches!(val, ColumnValues::Bit(true)));
    }

    #[test]
    fn test_column_values_null() {
        let val = ColumnValues::Null;
        assert!(matches!(val, ColumnValues::Null));
    }

    #[test]
    fn test_column_values_bytes() {
        let val = ColumnValues::Bytes(vec![1, 2, 3, 4]);
        assert!(matches!(val, ColumnValues::Bytes(_)));
    }

    #[test]
    fn test_column_values_uuid() {
        let uuid = Uuid::nil();
        let val = ColumnValues::Uuid(uuid);
        assert!(matches!(val, ColumnValues::Uuid(_)));
    }

    #[test]
    fn test_column_values_clone() {
        let val = ColumnValues::Int(42);
        let cloned = val.clone();
        assert_eq!(val, cloned);
    }

    #[test]
    fn test_sql_xml_clone() {
        let xml = SqlXml {
            bytes: vec![0x3C, 0x00, 0x3E, 0x00],
        };
        let cloned = xml.clone();
        assert_eq!(xml, cloned);
    }

    #[test]
    fn test_sql_date_min_boundary() {
        // 0001-01-01 = day 0 (minimum valid date)
        let result = SqlDate::create(0);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().get_days(), 0);
    }

    #[test]
    fn test_sql_date_max_boundary() {
        // 9999-12-31 = 3,652,058 days since 0001-01-01 (maximum valid date)
        // but SQL Server DATE uses 0-based counting, so the correct value is 3,652,058
        let result = SqlDate::create(3_652_058);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().get_days(), 3_652_058);
    }

    #[test]
    fn test_sql_date_above_max() {
        // Day 3,652,059 is above maximum valid date (9999-12-31)
        // The correct maximum is 3,652,058
        let result = SqlDate::create(3_652_059);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("out of range"));
    }

    #[test]
    fn test_sql_date_far_future() {
        // Day 16,777,215 (0xFFFFFF) is far beyond SQL Server DATE range
        let result = SqlDate::create(16_777_215);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("out of range"));
    }

    #[test]
    fn test_sql_date_mid_range() {
        // 2000-01-01 = ordinal 730,485 (a typical date)
        let result = SqlDate::create(730_485);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().get_days(), 730_485);
    }
}
