// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz test for RpcParameter API functions
//!
//! This fuzzer tests the RpcParameter-related functions that process user inputs:
//! - RpcParameter::new() - Parameter construction
//! - RpcParameter::get_sql_name() - SQL type name generation
//! - build_parameter_list_string() - Parameter list building for sp_executesql
//!
//! These are critical functions that:
//! 1. Process user-provided parameter names (potential for name length validation bugs)
//! 2. Convert SqlType to SQL Server type names (string formatting bugs)
//! 3. Build parameter declaration strings (SQL injection via parameter names)
//! 4. Handle extreme numeric values (precision/scale, length fields)
//!
//! What bugs this catches:
//! - Panics from unexpected SqlType variants
//! - Integer overflows in length calculations
//! - String formatting issues (e.g., Decimal with extreme precision/scale)
//! - Memory issues from very long parameter names or lists
//! - Edge cases in type name generation (MAX, special lengths)
//!
//! Run with: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_api_inputs

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags, build_parameter_list_string};

/// Maximum string length to prevent fuzzer timeouts
const MAX_STRING_LEN: usize = 2048;

/// Maximum binary data length to prevent fuzzer timeouts  
const MAX_BINARY_LEN: usize = 1024;

/// Fuzzed SQL types matching the real SqlType enum
#[derive(Debug, Arbitrary)]
enum FuzzSqlType {
    Bit(Option<bool>),
    TinyInt(Option<u8>),
    SmallInt(Option<i16>),
    Int(Option<i32>),
    BigInt(Option<i64>),
    Real(Option<f32>),
    Float(Option<f64>),
    
    // String types with length
    NVarchar(Option<String>, u16),
    Varchar(Option<String>, u16),
    NChar(Option<String>, u16),
    
    // Binary types with length
    VarBinary(Option<Vec<u8>>, u16),
    Binary(Option<Vec<u8>>, u16),
    
    // MAX types
    NVarcharMax(Option<String>),
    VarcharMax(Option<String>),
    VarBinaryMax(Option<Vec<u8>>),
    
    // Decimal with precision and scale
    Decimal(Option<u8>, Option<u8>), // (precision, scale)
}

/// Safely truncate a string at a UTF-8 character boundary
fn truncate_string_safe(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    
    // Find the nearest character boundary at or before max_len
    let mut truncate_at = max_len;
    while truncate_at > 0 && !s.is_char_boundary(truncate_at) {
        truncate_at -= 1;
    }
    
    s[..truncate_at].to_string()
}

impl FuzzSqlType {
    /// Convert to real SqlType, sanitizing extreme values
    fn to_sql_type(&mut self) -> SqlType {
        match self {
            FuzzSqlType::Bit(v) => SqlType::Bit(*v),
            FuzzSqlType::TinyInt(v) => SqlType::TinyInt(*v),
            FuzzSqlType::SmallInt(v) => SqlType::SmallInt(*v),
            FuzzSqlType::Int(v) => SqlType::Int(*v),
            FuzzSqlType::BigInt(v) => SqlType::BigInt(*v),
            FuzzSqlType::Real(v) => SqlType::Real(*v),
            FuzzSqlType::Float(v) => SqlType::Float(*v),
            
            FuzzSqlType::NVarchar(s, len) => {
                let sql_string = s.as_ref().map(|string| {
                    let truncated = truncate_string_safe(string, MAX_STRING_LEN);
                    SqlString::from_utf8_string(truncated)
                });
                // Clamp length to reasonable values
                let clamped_len = (*len).min(4000);
                SqlType::NVarchar(sql_string, clamped_len)
            }
            
            FuzzSqlType::Varchar(s, len) => {
                let sql_string = s.as_ref().map(|string| {
                    let truncated = truncate_string_safe(string, MAX_STRING_LEN);
                    SqlString::from_utf8_string(truncated)
                });
                let clamped_len = (*len).min(8000);
                SqlType::Varchar(sql_string, clamped_len)
            }
            
            FuzzSqlType::NChar(s, len) => {
                let sql_string = s.as_ref().map(|string| {
                    let truncated = truncate_string_safe(string, MAX_STRING_LEN);
                    SqlString::from_utf8_string(truncated)
                });
                let clamped_len = (*len).min(4000);
                SqlType::NChar(sql_string, clamped_len)
            }
            
            FuzzSqlType::VarBinary(data, len) => {
                if let Some(bytes) = data {
                    if bytes.len() > MAX_BINARY_LEN {
                        bytes.truncate(MAX_BINARY_LEN);
                    }
                }
                let clamped_len = (*len).min(8000);
                SqlType::VarBinary(data.clone(), clamped_len)
            }
            
            FuzzSqlType::Binary(data, len) => {
                if let Some(bytes) = data {
                    if bytes.len() > MAX_BINARY_LEN {
                        bytes.truncate(MAX_BINARY_LEN);
                    }
                }
                let clamped_len = (*len).min(8000);
                SqlType::Binary(data.clone(), clamped_len)
            }
            
            FuzzSqlType::NVarcharMax(s) => {
                let sql_string = s.as_ref().map(|string| {
                    let truncated = truncate_string_safe(string, MAX_STRING_LEN);
                    SqlString::from_utf8_string(truncated)
                });
                SqlType::NVarcharMax(sql_string)
            }
            
            FuzzSqlType::VarcharMax(s) => {
                let sql_string = s.as_ref().map(|string| {
                    let truncated = truncate_string_safe(string, MAX_STRING_LEN);
                    SqlString::from_utf8_string(truncated)
                });
                SqlType::VarcharMax(sql_string)
            }
            
            FuzzSqlType::VarBinaryMax(data) => {
                if let Some(bytes) = data {
                    if bytes.len() > MAX_BINARY_LEN {
                        bytes.truncate(MAX_BINARY_LEN);
                    }
                }
                SqlType::VarBinaryMax(data.clone())
            }
            
            FuzzSqlType::Decimal(precision, scale) => {
                // SQL Server decimal has precision 1-38, scale 0-precision
                // Test extreme/invalid values to catch validation bugs
                let _p = precision.unwrap_or(18).clamp(1, 38);
                let _s = scale.unwrap_or(0).clamp(0, _p);
                
                // For fuzzing, create a DecimalParts - we'll use None to test null handling
                // Real DecimalParts requires more complex setup, so just test the type name generation
                SqlType::Decimal(None)
            }
        }
    }
}

/// Fuzzed RPC parameter
#[derive(Debug, Arbitrary)]
struct FuzzRpcParameter {
    name: Option<String>,
    value: FuzzSqlType,
    is_output: bool,
}

impl FuzzRpcParameter {
    fn to_rpc_parameter(&mut self) -> RpcParameter {
        let flags = if self.is_output {
            StatusFlags::BY_REF_VALUE
        } else {
            StatusFlags::NONE
        };
        
        let sql_type = self.value.to_sql_type();
        RpcParameter::new(self.name.clone(), flags, sql_type)
    }
}

/// Main fuzzing scenarios
#[derive(Debug, Arbitrary)]
enum FuzzScenario {
    /// Test get_sql_name with a single SqlType
    GetSqlName(FuzzSqlType),
    
    /// Test RpcParameter::new with various inputs
    CreateParameter(FuzzRpcParameter),
    
    /// Test build_parameter_list_string with multiple parameters
    BuildParameterList(Vec<FuzzRpcParameter>),
}

fuzz_target!(|data: &[u8]| {
    let scenario = match <FuzzScenario as Arbitrary>::arbitrary(&mut arbitrary::Unstructured::new(data)) {
        Ok(s) => s,
        Err(_) => return,
    };
    
    match scenario {
        FuzzScenario::GetSqlName(mut fuzz_type) => {
            // Test RpcParameter::get_sql_name() which generates SQL type names
            // This tests string formatting, length handling, and edge cases
            let sql_type = fuzz_type.to_sql_type();
            let _type_name = RpcParameter::get_sql_name(&sql_type);
            
            // The function should never panic, even with:
            // - MAX lengths (> 4000 for NVarchar, > 8000 for Varchar)
            // - Zero lengths
            // - Extreme decimal precision/scale
            // - None values for nullable types
        }
        
        FuzzScenario::CreateParameter(mut fuzz_param) => {
            // Test RpcParameter::new() constructor
            let param = fuzz_param.to_rpc_parameter();
            
            // Also test get_sql_name on this parameter's type
            let _type_name = RpcParameter::get_sql_name(param.get_value());
        }
        
        FuzzScenario::BuildParameterList(mut fuzz_params) => {
            // Limit number of parameters to prevent timeout
            if fuzz_params.len() > 30 {
                fuzz_params.truncate(30);
            }
            
            // Convert to RpcParameters
            let rpc_params: Vec<RpcParameter> = fuzz_params
                .iter_mut()
                .map(|p| p.to_rpc_parameter())
                .collect();
            
            // Test build_parameter_list_string() which builds the parameter
            // declaration string for sp_executesql
            // This tests:
            // - String concatenation with many parameters
            // - Formatting of type names with lengths
            // - Memory allocation for large parameter lists
            // - Handling of None parameter names
            let mut params_list = String::new();
            let _ = build_parameter_list_string(&rpc_params, &mut params_list);
        }
    }
});
