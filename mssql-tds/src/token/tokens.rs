// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt::{self, Debug};

use super::{
    fed_auth_info::{FedAuthInfoToken, SspiToken},
    login_ack::LoginAckToken,
    tokenitems::ReturnValueStatus,
};
use crate::datatypes::column_values::ColumnValues;
use crate::{
    error::Error,
    message::login::{FeatureExtension, RoutingInfo},
    query::metadata::ColumnMetadata,
};

/// TDS token type identifiers as defined by the protocol specification.
#[derive(Eq, PartialEq, Hash, Debug)]
#[repr(u8)]
#[allow(clippy::upper_case_acronyms)]
pub(crate) enum TokenType {
    AltMetadata = 0x88,
    AltRow = 0xD3,
    ColMetadata = 0x81,
    ColInfo = 0xA5,
    Done = 0xFD,
    DoneProc = 0xFE,
    DoneInProc = 0xFF,
    EnvChange = 0xE3,
    Error = 0xAA,
    FeatureExtAck = 0xAE,
    FedAuthInfo = 0xEE,
    Info = 0xAB,
    LoginAck = 0xAD,
    NbcRow = 0xD2,
    Offset = 0x78,
    Order = 0xA9,
    ReturnStatus = 0x79,
    ReturnValue = 0xAC,
    Row = 0xD1,
    SSPI = 0xED,
    TabName = 0xA4,
}

impl TryFrom<u8> for TokenType {
    type Error = crate::error::Error;

    fn try_from(value: u8) -> Result<Self, <Self as TryFrom<u8>>::Error> {
        match value {
            0x88 => Ok(TokenType::AltMetadata),
            0xD3 => Ok(TokenType::AltRow),
            0x81 => Ok(TokenType::ColMetadata),
            0xA5 => Ok(TokenType::ColInfo),
            0xFD => Ok(TokenType::Done),
            0xFE => Ok(TokenType::DoneProc),
            0xFF => Ok(TokenType::DoneInProc),
            0xE3 => Ok(TokenType::EnvChange),
            0xAA => Ok(TokenType::Error),
            0xAE => Ok(TokenType::FeatureExtAck),
            0xEE => Ok(TokenType::FedAuthInfo),
            0xAB => Ok(TokenType::Info),
            0xAD => Ok(TokenType::LoginAck),
            0xD2 => Ok(TokenType::NbcRow),
            0x78 => Ok(TokenType::Offset),
            0xA9 => Ok(TokenType::Order),
            0x79 => Ok(TokenType::ReturnStatus),
            0xAC => Ok(TokenType::ReturnValue),
            0xD1 => Ok(TokenType::Row),
            0xED => Ok(TokenType::SSPI),
            0xA4 => Ok(TokenType::TabName),
            _ => Err(crate::error::Error::ProtocolError(format!(
                "Unknown token type: {value:#X}"
            ))),
        }
    }
}

/// A parsed TDS token.
pub(crate) trait Token {
    fn token_type(&self) -> TokenType;
}

#[derive(Debug)]
#[cfg(not(fuzzing))]
pub(crate) enum Tokens {
    Done(DoneToken),
    DoneInProc(DoneToken),
    DoneProc(DoneToken),
    EnvChange(EnvChangeToken),
    Error(ErrorToken),
    Info(InfoToken),
    LoginAck(LoginAckToken),
    FeatureExtAck(FeatureExtAckToken),
    FedAuthInfo(FedAuthInfoToken),
    Sspi(SspiToken),
    Row(RowToken),
    ColMetadata(ColMetadataToken),
    Order(OrderToken),
    ReturnStatus(ReturnStatusToken),
    ReturnValue(ReturnValueToken),
}

/// Union of all parsed TDS tokens (public under `fuzzing` cfg).
#[derive(Debug)]
#[cfg(fuzzing)]
#[allow(private_interfaces)]
pub enum Tokens {
    Done(DoneToken),
    DoneInProc(DoneToken),
    DoneProc(DoneToken),
    EnvChange(EnvChangeToken),
    Error(ErrorToken),
    Info(InfoToken),
    LoginAck(LoginAckToken),
    FeatureExtAck(FeatureExtAckToken),
    FedAuthInfo(FedAuthInfoToken),
    Sspi(SspiToken),
    Row(RowToken),
    ColMetadata(ColMetadataToken),
    Order(OrderToken),
    ReturnStatus(ReturnStatusToken),
    ReturnValue(ReturnValueToken),
}

macro_rules! impl_from_token {
    ($token_type:ty, $variant:ident) => {
        impl From<$token_type> for Tokens {
            fn from(token: $token_type) -> Self {
                Tokens::$variant(token)
            }
        }
    };
}

impl_from_token!(EnvChangeToken, EnvChange);
impl_from_token!(ErrorToken, Error);
impl_from_token!(InfoToken, Info);
impl_from_token!(LoginAckToken, LoginAck);
impl_from_token!(FeatureExtAckToken, FeatureExtAck);
impl_from_token!(FedAuthInfoToken, FedAuthInfo);
impl_from_token!(SspiToken, Sspi);
impl_from_token!(RowToken, Row);
impl_from_token!(ColMetadataToken, ColMetadata);
impl_from_token!(OrderToken, Order);
impl_from_token!(ReturnStatusToken, ReturnStatus);
impl_from_token!(ReturnValueToken, ReturnValue);

impl Token for Tokens {
    fn token_type(&self) -> TokenType {
        match self {
            Tokens::Done(token) => token.token_type(),
            Tokens::DoneInProc(token) => token.token_type(),
            Tokens::DoneProc(token) => token.token_type(),
            Tokens::EnvChange(token) => token.token_type(),
            Tokens::Error(token) => token.token_type(),
            Tokens::Info(token) => token.token_type(),
            Tokens::LoginAck(token) => token.token_type(),
            Tokens::FeatureExtAck(token) => token.token_type(),
            Tokens::FedAuthInfo(token) => token.token_type(),
            Tokens::Sspi(token) => token.token_type(),
            Tokens::Row(token) => token.token_type(),
            Tokens::ColMetadata(token) => token.token_type(),
            Tokens::Order(token) => token.token_type(),
            Tokens::ReturnStatus(token) => token.token_type(),
            Tokens::ReturnValue(token) => token.token_type(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum EnvChangeContainer {
    String(EnvChangeTokenValuePairs<String>),
    SqlCollation(EnvChangeTokenValuePairs<Option<SqlCollation>>),
    UInt32(EnvChangeTokenValuePairs<u32>),
    RoutingType(EnvChangeTokenValuePairs<Option<RoutingInfo>>),
    BytesType(EnvChangeTokenValuePairs<Vec<u8>>),
    UInt64(EnvChangeTokenValuePairs<u64>),
}

impl From<(String, String)> for EnvChangeContainer {
    fn from(value: (String, String)) -> Self {
        EnvChangeContainer::String(EnvChangeTokenValuePairs::<String>::new(value.0, value.1))
    }
}

impl From<(Option<SqlCollation>, Option<SqlCollation>)> for EnvChangeContainer {
    fn from(value: (Option<SqlCollation>, Option<SqlCollation>)) -> Self {
        EnvChangeContainer::SqlCollation(EnvChangeTokenValuePairs::<Option<SqlCollation>>::new(
            value.0, value.1,
        ))
    }
}

impl From<(u32, u32)> for EnvChangeContainer {
    fn from(value: (u32, u32)) -> Self {
        EnvChangeContainer::UInt32(EnvChangeTokenValuePairs::<u32>::new(value.0, value.1))
    }
}

impl From<(Option<RoutingInfo>, Option<RoutingInfo>)> for EnvChangeContainer {
    fn from(value: (Option<RoutingInfo>, Option<RoutingInfo>)) -> Self {
        EnvChangeContainer::RoutingType(EnvChangeTokenValuePairs::<Option<RoutingInfo>>::new(
            value.0, value.1,
        ))
    }
}

impl From<(Vec<u8>, Vec<u8>)> for EnvChangeContainer {
    fn from(value: (Vec<u8>, Vec<u8>)) -> Self {
        EnvChangeContainer::BytesType(EnvChangeTokenValuePairs::<Vec<u8>>::new(value.0, value.1))
    }
}

impl From<(u64, u64)> for EnvChangeContainer {
    fn from(value: (u64, u64)) -> Self {
        EnvChangeContainer::UInt64(EnvChangeTokenValuePairs::<u64>::new(value.0, value.1))
    }
}

impl fmt::Debug for EnvChangeContainer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvChangeContainer::String(value) => write!(f, "String: {value:?}"),
            EnvChangeContainer::SqlCollation(value) => write!(f, "SqlCollation: {value:?}"),
            EnvChangeContainer::UInt32(value) => write!(f, "UInt32: {value:?}"),
            EnvChangeContainer::RoutingType(value) => write!(f, "RoutingType: {value:?}"),
            EnvChangeContainer::BytesType(value) => write!(f, "ByteType: {value:?}"),
            EnvChangeContainer::UInt64(value) => write!(f, "UInt64 {value:?}"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct EnvChangeToken {
    pub sub_type: EnvChangeTokenSubType,
    pub change_type: EnvChangeContainer,
}

#[derive(Debug)]
pub(crate) struct FeatureExtAckToken {
    features: Vec<(FeatureExtension, Vec<u8>)>,
}

impl FeatureExtAckToken {
    pub(crate) fn new(features: Vec<(FeatureExtension, Vec<u8>)>) -> Self {
        Self { features }
    }

    pub(crate) fn acknowledged_features(&self) -> &[(FeatureExtension, Vec<u8>)] {
        &self.features
    }
}

impl Token for EnvChangeToken {
    fn token_type(&self) -> TokenType {
        TokenType::EnvChange
    }
}

impl Token for FeatureExtAckToken {
    fn token_type(&self) -> TokenType {
        TokenType::FeatureExtAck
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ColMetadataToken {
    pub column_count: u16,
    pub columns: Vec<ColumnMetadata>,
}

impl Token for ColMetadataToken {
    fn token_type(&self) -> TokenType {
        TokenType::ColMetadata
    }
}

#[derive(Debug, Default)]
pub(crate) struct OrderToken {
    pub _order_columns: Vec<u16>,
}

impl Token for OrderToken {
    fn token_type(&self) -> TokenType {
        TokenType::Order
    }
}

/// SQL Server collation metadata (LCID, flags, and sort ID).
#[derive(Clone, Default, PartialEq, Eq, Copy)]
pub struct SqlCollation {
    /// Raw 32-bit collation info value.
    pub info: u32,
    /// LCID language identifier (lower 20 bits of `info`).
    pub lcid_language_id: i32,
    /// Collation flags (bits 20–27 of `info`).
    pub col_flags: u8,
    /// Sort ID from the fifth collation byte.
    pub sort_id: u8,
}

impl TryFrom<&[u8]> for SqlCollation {
    type Error = Error;

    fn try_from(collation_bytes: &[u8]) -> Result<Self, Self::Error> {
        if collation_bytes.len() != 5 {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "Invalid collation length: {} (expected 5)",
                    collation_bytes.len()
                ),
            )));
        }

        let info = u32::from_ne_bytes([
            collation_bytes[0],
            collation_bytes[1],
            collation_bytes[2],
            collation_bytes[3],
        ]);

        // 20 BITS are lcid. The language id is the lower 16 bits and the lcid sort flags are in the next 4 bits.
        let lcid_language_id = (info & 0x000FFFFF) as i32; // Lower 16 bits.
        let col_flags = ((info >> 20) & 0xFF) as u8; // Next 8 bits
        let sort_id = collation_bytes[4];

        Ok(SqlCollation {
            info,
            lcid_language_id,
            col_flags,
            sort_id,
        })
    }
}

impl SqlCollation {
    /// Returns the LCID from the collation.
    pub fn lcid_language_id(&self) -> i32 {
        (self.info & 0x000FFFFF) as i32
    }

    /// Returns the comparison style from the collation.
    pub fn comparison_style(&self) -> u8 {
        ((self.info >> 20) & 0xFF) as u8 // Next 8 bits
    }

    /// Returns the sort ID from the collation.
    pub fn sort_id(&self) -> u8 {
        self.sort_id
    }

    /// Returns the collation version nibble (bits 28–31).
    pub fn version(&self) -> u8 {
        (self.info >> 28) as u8
    }

    /// Returns `true` if the `fIgnoreCase` flag is set.
    pub fn ignore_case(&self) -> bool {
        (self.col_flags & 0x1) != 0
    }

    /// Returns `true` if the `fIgnoreAccent` flag is set.
    pub fn ignore_accent(&self) -> bool {
        (self.col_flags & 0x2) != 0
    }

    /// Returns `true` if the `fIgnoreKana` flag is set.
    pub fn ignore_kana(&self) -> bool {
        (self.col_flags & 0x4) != 0
    }

    /// Returns `true` if the `fIgnoreWidth` flag is set.
    pub fn ignore_width(&self) -> bool {
        (self.col_flags & 0x8) != 0
    }

    /// Returns `true` if the `fBinary` flag is set.
    pub fn binary(&self) -> bool {
        (self.col_flags & 0x10) != 0
    }

    /// Returns `true` if the `fBinary2` flag is set.
    pub fn binary2(&self) -> bool {
        (self.col_flags & 0x20) != 0
    }

    /// Returns `true` if the `fUTF8` flag is set.
    pub fn utf8(&self) -> bool {
        (self.col_flags & 0x40) != 0
    }
}

impl Debug for SqlCollation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "INFO: {} LCID: {}, ComparisonStyle: {}, SortID: {}, IsUtf8: {}, IgnoreCase: {}",
            self.info,
            self.lcid_language_id,
            self.col_flags,
            self.sort_id,
            self.utf8(),
            self.ignore_case()
        )
    }
}

impl fmt::Display for SqlCollation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "INFO: {} LCID: {}, ComparisonStyle: {}, SortID: {}, IsUtf8: {}",
            self.info,
            self.lcid_language_id,
            self.col_flags,
            self.sort_id,
            self.utf8()
        )
    }
}

#[cfg(test)]
mod sql_collation_tests {
    use super::*;

    #[test]
    fn test_try_from_valid() {
        // Valid 5-byte collation
        let collation_bytes = [0x09, 0x04, 0xd0, 0x00, 0x34];
        let collation: SqlCollation = collation_bytes.as_slice().try_into().unwrap();
        assert_eq!(collation.sort_id, 0x34);
    }

    #[test]
    fn test_try_from_invalid_length() {
        // Invalid length: 4 bytes
        let collation_bytes = [0x09, 0x04, 0xd0, 0x00];
        let result: Result<SqlCollation, _> = collation_bytes.as_slice().try_into();
        assert!(result.is_err());

        // Invalid length: 6 bytes
        let collation_bytes = [0x09, 0x04, 0xd0, 0x00, 0x34, 0xff];
        let result: Result<SqlCollation, _> = collation_bytes.as_slice().try_into();
        assert!(result.is_err());

        // Invalid length: empty
        let collation_bytes: &[u8] = &[];
        let result: Result<SqlCollation, _> = collation_bytes.try_into();
        assert!(result.is_err());
    }

    #[test]
    fn test_collation_flags() {
        // Create collation with UTF-8 flag set (flag is in col_flags, which comes from bits 20-27 of info)
        // UTF-8 flag is 0x40 in col_flags, so we need to set bit 26 of info
        // info = (0x40 << 20) = 0x04000000
        let collation_bytes = [0x00, 0x00, 0x00, 0x04, 0x00];
        let collation: SqlCollation = collation_bytes.as_slice().try_into().unwrap();
        assert!(collation.utf8());
    }
}

/// Static lookup table for code pages by SortID
#[allow(dead_code)]
pub(crate) static CODE_PAGE_FROM_SORT_ID: [Option<u16>; 256] = [
    None,       // 0
    None,       // 1
    None,       // 2
    None,       // 3
    None,       // 4
    None,       // 5
    None,       // 6
    None,       // 7
    None,       // 8
    None,       // 9
    None,       // 10
    None,       // 11
    None,       // 12
    None,       // 13
    None,       // 14
    None,       // 15
    None,       // 16
    None,       // 17
    None,       // 18
    None,       // 19
    None,       // 20
    None,       // 21
    None,       // 22
    None,       // 23
    None,       // 24
    None,       // 25
    None,       // 26
    None,       // 27
    None,       // 28
    None,       // 29
    Some(437),  // 30
    Some(437),  // 31
    Some(437),  // 32
    Some(437),  // 33
    Some(437),  // 34
    None,       // 35
    None,       // 36
    None,       // 37
    None,       // 38
    None,       // 39
    Some(850),  // 40
    Some(850),  // 41
    Some(850),  // 42
    Some(850),  // 43
    Some(850),  // 44
    None,       // 45
    None,       // 46
    None,       // 47
    None,       // 48
    Some(850),  // 49
    Some(1252), // 50
    Some(1252), // 51
    Some(1252), // 52
    Some(1252), // 53
    Some(1252), // 54
    Some(850),  // 55
    Some(850),  // 56
    Some(850),  // 57
    Some(850),  // 58
    Some(850),  // 59
    Some(850),  // 60
    Some(850),  // 61
    None,       // 62
    None,       // 63
    None,       // 64
    None,       // 65
    None,       // 66
    None,       // 67
    None,       // 68
    None,       // 69
    None,       // 70
    Some(1252), // 71
    Some(1252), // 72
    Some(1252), // 73
    Some(1252), // 74
    Some(1252), // 75
    None,       // 76
    None,       // 77
    None,       // 78
    None,       // 79
    None,       // 80
    None,       // 81
    None,       // 82
    None,       // 83
    None,       // 84
    Some(1250), // 85
    Some(1250), // 86
    Some(1250), // 87
    Some(1250), // 88
    Some(1250), // 89
    Some(1250), // 90
    Some(1250), // 91
    Some(1250), // 92
    Some(1250), // 93
    Some(1250), // 94
    Some(1250), // 95
    Some(1250), // 96
    Some(1250), // 97
    Some(1250), // 98
    Some(1250), // 99
    Some(1250), // 100
    Some(1250), // 101
    Some(1250), // 102
    Some(1250), // 103
    None,       // 104
    None,       // 105
    None,       // 106
    None,       // 107
    None,       // 108
    Some(1251), // 109
    Some(1251), // 110
    Some(1251), // 111
    Some(1251), // 112
    Some(1251), // 113
    None,       // 114
    None,       // 115
    None,       // 116
    Some(1253), // 117
    Some(1253), // 118
    Some(1253), // 119
    None,       // 120
    None,       // 121
    None,       // 122
    None,       // 123
    None,       // 124
    None,       // 125
    Some(1253), // 126
    Some(1253), // 127
    Some(1253), // 128
    None,       // 129
    Some(1253), // 130
    None,       // 131
    None,       // 132
    None,       // 133
    None,       // 134
    Some(1254), // 135
    Some(1254), // 136
    Some(1254), // 137
    None,       // 138
    None,       // 139
    None,       // 140
    None,       // 141
    None,       // 142
    None,       // 143
    Some(1255), // 144
    Some(1255), // 145
    Some(1255), // 146
    None,       // 147
    None,       // 148
    None,       // 149
    None,       // 150
    None,       // 151
    None,       // 152
    Some(1256), // 153
    Some(1256), // 154
    Some(1256), // 155
    None,       // 156
    None,       // 157
    None,       // 158
    None,       // 159
    None,       // 160
    None,       // 161
    Some(1257), // 162
    Some(1257), // 163
    Some(1257), // 164
    Some(1257), // 165
    Some(1257), // 166
    Some(1257), // 167
    Some(1257), // 168
    Some(1257), // 169
    Some(1257), // 170
    None,       // 171
    None,       // 172
    None,       // 173
    None,       // 174
    None,       // 175
    None,       // 176
    None,       // 177
    None,       // 178
    None,       // 179
    None,       // 180
    None,       // 181
    None,       // 182
    None,       // 183
    None,       // 184
    None,       // 185
    None,       // 186
    None,       // 187
    None,       // 188
    None,       // 189
    None,       // 190
    None,       // 191
    None,       // 192
    None,       // 193
    Some(1252), // 194
    Some(1252), // 195
    Some(1252), // 196
    Some(1252), // 197
    None,       // 198
    None,       // 199
    None,       // 200
    None,       // 201
    None,       // 202
    Some(932),  // 203
    Some(932),  // 204
    Some(949),  // 205
    Some(949),  // 206
    Some(950),  // 207
    Some(950),  // 208
    Some(936),  // 209
    Some(936),  // 210
    Some(932),  // 211
    Some(949),  // 212
    Some(950),  // 213
    Some(936),  // 214
    Some(874),  // 215
    Some(874),  // 216
    Some(874),  // 217
    None,       // 218
    None,       // 219
    None,       // 220
    Some(1252), // 221
    Some(1252), // 222
    Some(1252), // 223
    Some(1252), // 224
    Some(1252), // 225
    Some(1252), // 226
    Some(1252), // 227
    None,       // 228
    None,       // 229
    None,       // 230
    None,       // 231
    None,       // 232
    None,       // 233
    None,       // 234
    None,       // 235
    None,       // 236
    None,       // 237
    None,       // 238
    None,       // 239
    None,       // 240
    None,       // 241
    None,       // 242
    None,       // 243
    None,       // 244
    None,       // 245
    None,       // 246
    None,       // 247
    None,       // 248
    None,       // 249
    None,       // 250
    None,       // 251
    None,       // 252
    None,       // 253
    None,       // 254
    None,       // 255
];

/// ERROR Token - SQL Server error message
///
/// Reports errors that occur during statement execution. These errors
/// typically have severity >= 11 and may cause statement failure.
///
/// ## Structure
/// ```text
/// ┌──────────────────────────────────────────────────────────┐
/// │ Number (4) | State (1) | Severity (1) | Message (var)   │
/// │ ServerName (var) | ProcName (var) | LineNumber (4)     │
/// └──────────────────────────────────────────────────────────┘
/// ```
///
/// ## Severity Levels
/// - 0-9: Informational (shouldn't appear in ERROR tokens)
/// - 10: Status information
/// - 11-16: User errors (correctable by user)
/// - 17-19: Software/hardware errors
/// - 20-25: Fatal errors (connection terminated)
///
/// ## Common Error Numbers
/// - 208: Invalid object name
/// - 515: Cannot insert NULL
/// - 547: Foreign key violation
/// - 2601: Duplicate key
#[derive(Debug)]
pub(crate) struct ErrorToken {
    /// SQL Server error number (e.g., 208 for "invalid object name")
    pub number: u32,

    /// Internal state code (indicates position in SQL Server's state machine)
    pub state: u8,

    /// Error severity (11-25 for errors, typically 16 for user errors)
    pub severity: u8,

    /// Human-readable error message
    pub message: String,

    /// Name of the SQL Server instance that generated the error
    pub server_name: String,

    /// Name of stored procedure where error occurred (empty if not in proc)
    pub proc_name: String,

    /// Line number in batch or procedure where error occurred
    pub line_number: u32,
}

impl Token for ErrorToken {
    fn token_type(&self) -> TokenType {
        TokenType::Error
    }
}

/// INFO Token - SQL Server informational message
///
/// Reports informational messages, warnings, and PRINT output.
/// Identical structure to ERROR token but with lower severity (< 11).
///
/// ## Structure (same as ERROR token)
/// ```text
/// ┌──────────────────────────────────────────────────────────┐
/// │ Number (4) | State (1) | Severity (1) | Message (var)   │
/// │ ServerName (var) | ProcName (var) | LineNumber (4)     │
/// └──────────────────────────────────────────────────────────┘
/// ```
///
/// ## Common Uses
/// - PRINT statements (severity 0)
/// - Database context changes (severity 10, number 5701)
/// - Language setting changes (severity 10, number 5703)
/// - Warnings and informational messages
///
/// ## Difference from ERROR
/// - Token type is 0xAB (INFO) vs 0xAA (ERROR)
/// - Severity typically < 11
/// - Don't cause statement failure
/// - Execution continues normally
#[derive(Debug)]
#[allow(dead_code)] // Not exposed publicly via any API yet.
pub(crate) struct InfoToken {
    /// Message number (informational code, e.g., 5701 for database change)
    pub number: u32,

    /// Internal state code
    pub state: u8,

    /// Message severity (typically 0-10 for INFO tokens)
    pub severity: u8,

    /// Human-readable message text
    pub message: String,

    /// Name of the SQL Server instance
    pub server_name: String,

    /// Name of stored procedure if applicable
    pub proc_name: String,

    /// Line number where message originated
    pub line_number: u32,
}

impl Token for InfoToken {
    fn token_type(&self) -> TokenType {
        TokenType::Info
    }
}

/// DONE Token - Indicates completion of a SQL statement
///
/// Sent when a SQL statement completes execution. Contains status flags,
/// the command type that completed, and the number of rows affected.
///
/// ## Structure
/// ```text
/// ┌─────────────────────────────────────────┐
/// │ Status (2 bytes) | CurCmd (2 bytes)     │
/// │ RowCount (8 bytes)                      │
/// └─────────────────────────────────────────┘
/// ```
///
/// ## Example
/// After `DELETE FROM Users WHERE Age > 100`:
/// - status: DONE_COUNT (0x10) - row count is valid
/// - cur_cmd: DELETE (0xC3)
/// - row_count: 5 (deleted 5 rows)
#[derive(Debug)]
pub(crate) struct DoneToken {
    /// Status flags indicating completion state (bitmask)
    /// - DONE_MORE (0x01): More results coming
    /// - DONE_ERROR (0x02): Error occurred
    /// - DONE_COUNT (0x10): Row count is valid
    /// - DONE_ATTN (0x20): Attention acknowledgment
    pub status: DoneStatus,

    /// The type of SQL command that completed
    /// (SELECT, INSERT, UPDATE, DELETE, etc.)
    pub cur_cmd: CurrentCommand,

    /// Number of rows affected by the statement
    /// Only valid if DONE_COUNT flag is set in status
    pub row_count: u64,
}

impl Token for DoneToken {
    fn token_type(&self) -> TokenType {
        TokenType::Done
    }
}

impl DoneToken {
    pub fn has_more(&self) -> bool {
        self.status.contains(DoneStatus::MORE)
    }

    pub fn has_error(&self) -> bool {
        self.status.contains(DoneStatus::ERROR)
    }
}

/// RETURNSTATUS Token - Return value from a stored procedure
///
/// Contains the integer value returned by a stored procedure's RETURN statement.
/// This token appears after all result sets and output parameters, but before DONEPROC.
///
/// ## Structure
/// ```text
/// ┌──────────────────────┐
/// │ Value (4 bytes)      │
/// │ INT32                │
/// └──────────────────────┘
/// ```
///
/// ## Conventions
/// - 0: Success (by convention)
/// - -1: General failure
/// - Other: Application-specific codes
///
/// ## Example
/// ```sql
/// CREATE PROCEDURE spCheckUser @userId INT
/// AS
/// BEGIN
///     IF EXISTS (SELECT 1 FROM Users WHERE Id = @userId)
///         RETURN 0;  -- Success
///     ELSE
///         RETURN -1; -- Not found
/// END
/// ```
/// Executing this proc sends a RETURNSTATUS token with value 0 or -1.
#[derive(Debug)]
pub(crate) struct ReturnStatusToken {
    /// Return value from the stored procedure's RETURN statement
    /// Convention: 0 = success, negative = error, positive = application-specific
    pub _value: i32,
}

impl Token for ReturnStatusToken {
    fn token_type(&self) -> TokenType {
        TokenType::ReturnValue
    }
}

/// RETURNVALUE Token - Output parameter value from stored procedure
///
/// Contains the value of an OUTPUT parameter returned from a stored procedure.
/// Multiple RETURNVALUE tokens may appear for procedures with multiple OUTPUT parameters.
///
/// ## Structure
/// ```text
/// ┌─────────────────────────────────────────────────────────────┐
/// │ ParamOrdinal (2) | ParamName (var) | Status (1) | Metadata  │
/// │ Value (variable based on data type)                         │
/// └─────────────────────────────────────────────────────────────┘
/// ```
///
/// ## Token Flow Example
/// ```sql
/// CREATE PROCEDURE spGetUserCount
///     @count INT OUTPUT
/// AS
/// BEGIN
///     SELECT @count = COUNT(*) FROM Users;
/// END
///
/// -- Execution:
/// DECLARE @c INT;
/// EXEC spGetUserCount @count = @c OUTPUT;
/// ```
///
/// Server sends (in order):
/// 1. RETURNVALUE token (for @count parameter)
/// 2. RETURNSTATUS token (procedure return value)
/// 3. DONEPROC token (procedure completion)
#[derive(Debug)]
pub(crate) struct ReturnValueToken {
    /// Ordinal position of the parameter (0-based)
    pub param_ordinal: u16,

    /// Name of the OUTPUT parameter (e.g., "@count")
    pub param_name: String,

    /// The actual value being returned
    pub value: ColumnValues,

    /// Metadata describing the parameter's data type
    pub column_metadata: Box<ColumnMetadata>,

    /// Status of the return value
    /// (indicates if value is default, NULL, etc.)
    pub status: ReturnValueStatus,
}

impl Token for ReturnValueToken {
    fn token_type(&self) -> TokenType {
        TokenType::ReturnValue
    }
}

#[derive(Debug)]
pub(crate) struct RowToken {
    #[allow(dead_code)]
    pub all_values: Vec<ColumnValues>,
}

impl RowToken {
    pub fn new(all_values: Vec<ColumnValues>) -> Self {
        Self { all_values }
    }
}

impl Token for RowToken {
    fn token_type(&self) -> TokenType {
        TokenType::Row
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct DoneStatus: u16 {
        /// Final.
        const FINAL = 0x0000;

        /// More.
        const MORE = 0x0001;

        /// Error.
        const ERROR = 0x0002;

        /// In Transaction.
        const IN_XACT = 0x0004;

        /// Count.
        const COUNT = 0x0010;

        /// Attention.
        const ATTN = 0x0020;

        /// Server Error.
        const SERVER_ERROR = 0x0100;
    }
}

impl From<u16> for DoneStatus {
    fn from(value: u16) -> Self {
        DoneStatus::from_bits_truncate(value)
    }
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CurrentCommand {
    None = 0x00,
    Select = 0xc1,
    Insert = 0xc3,
    Delete = 0xc4,
    Update = 0xc5,
    Abort = 0xd2,
    BeginXact = 0xd4,
    EndXact = 0xd5,
    BulkInsert = 0xf0,
    OpenCursor = 0x20,
    Merge = 0x117,
}

impl TryFrom<u16> for CurrentCommand {
    type Error = &'static str;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0xc1 => Ok(CurrentCommand::Select),
            0xc3 => Ok(CurrentCommand::Insert),
            0xc4 => Ok(CurrentCommand::Delete),
            0xc5 => Ok(CurrentCommand::Update),
            0xd2 => Ok(CurrentCommand::Abort),
            0xd4 => Ok(CurrentCommand::BeginXact),
            0xd5 => Ok(CurrentCommand::EndXact),
            0xf0 => Ok(CurrentCommand::BulkInsert),
            0x20 => Ok(CurrentCommand::OpenCursor),
            0x117 => Ok(CurrentCommand::Merge),
            // All unknown values are treated as None, and considered valid.
            _ => Ok(CurrentCommand::None),
        }
    }
}

/// Represents the different sub-types of environment change tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvChangeTokenSubType {
    Database,
    Language,
    CharacterSet,
    PacketSize,
    UnicodeDataSortingLocalId,
    UnicodeDataSortingComparisonFlags,
    SqlCollation,
    BeginTransaction,
    CommitTransaction,
    RollbackTransaction,
    EnlistDtcTransaction,
    DefectTransaction,
    DatabaseMirroringPartner,
    PromoteTransaction,
    TransactionManagerAddress,
    TransactionEnded,
    ResetConnection,
    UserInstanceName,
    Routing,
    Unknown(u8),
}

impl TryFrom<u8> for EnvChangeTokenSubType {
    type Error = crate::error::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => EnvChangeTokenSubType::Database,
            2 => EnvChangeTokenSubType::Language,
            3 => EnvChangeTokenSubType::CharacterSet,
            4 => EnvChangeTokenSubType::PacketSize,
            5 => EnvChangeTokenSubType::UnicodeDataSortingLocalId,
            6 => EnvChangeTokenSubType::UnicodeDataSortingComparisonFlags,
            7 => EnvChangeTokenSubType::SqlCollation,
            8 => EnvChangeTokenSubType::BeginTransaction,
            9 => EnvChangeTokenSubType::CommitTransaction,
            10 => EnvChangeTokenSubType::RollbackTransaction,
            11 => EnvChangeTokenSubType::EnlistDtcTransaction,
            12 => EnvChangeTokenSubType::DefectTransaction,
            13 => EnvChangeTokenSubType::DatabaseMirroringPartner,
            15 => EnvChangeTokenSubType::PromoteTransaction,
            16 => EnvChangeTokenSubType::TransactionManagerAddress,
            17 => EnvChangeTokenSubType::TransactionEnded,
            18 => EnvChangeTokenSubType::ResetConnection,
            19 => EnvChangeTokenSubType::UserInstanceName,
            20 => EnvChangeTokenSubType::Routing,
            unknown => EnvChangeTokenSubType::Unknown(unknown),
        })
    }
}

impl EnvChangeTokenSubType {
    #[allow(dead_code)]
    pub fn as_u8(&self) -> u8 {
        match self {
            EnvChangeTokenSubType::Database => 1,
            EnvChangeTokenSubType::Language => 2,
            EnvChangeTokenSubType::CharacterSet => 3,
            EnvChangeTokenSubType::PacketSize => 4,
            EnvChangeTokenSubType::UnicodeDataSortingLocalId => 5,
            EnvChangeTokenSubType::UnicodeDataSortingComparisonFlags => 6,
            EnvChangeTokenSubType::SqlCollation => 7,
            EnvChangeTokenSubType::BeginTransaction => 8,
            EnvChangeTokenSubType::CommitTransaction => 9,
            EnvChangeTokenSubType::RollbackTransaction => 10,
            EnvChangeTokenSubType::EnlistDtcTransaction => 11,
            EnvChangeTokenSubType::DefectTransaction => 12,
            EnvChangeTokenSubType::DatabaseMirroringPartner => 13,
            EnvChangeTokenSubType::PromoteTransaction => 15,
            EnvChangeTokenSubType::TransactionManagerAddress => 16,
            EnvChangeTokenSubType::TransactionEnded => 17,
            EnvChangeTokenSubType::ResetConnection => 18,
            EnvChangeTokenSubType::UserInstanceName => 19,
            EnvChangeTokenSubType::Routing => 20,
            EnvChangeTokenSubType::Unknown(val) => *val,
        }
    }
}

#[cfg(test)]
mod env_change_tests {
    use super::*;

    #[test]
    fn test_env_change_token_subtype_try_from() {
        // Test valid values
        assert!(matches!(
            EnvChangeTokenSubType::try_from(1).unwrap(),
            EnvChangeTokenSubType::Database
        ));
        assert!(matches!(
            EnvChangeTokenSubType::try_from(20).unwrap(),
            EnvChangeTokenSubType::Routing
        ));

        // Test invalid values (should not panic, should return Unknown)
        assert!(matches!(
            EnvChangeTokenSubType::try_from(30).unwrap(),
            EnvChangeTokenSubType::Unknown(30)
        ));
        assert!(matches!(
            EnvChangeTokenSubType::try_from(255).unwrap(),
            EnvChangeTokenSubType::Unknown(255)
        ));
    }

    #[test]
    fn test_env_change_token_subtype_as_u8() {
        assert_eq!(EnvChangeTokenSubType::Database.as_u8(), 1);
        assert_eq!(EnvChangeTokenSubType::Routing.as_u8(), 20);
        assert_eq!(EnvChangeTokenSubType::Unknown(30).as_u8(), 30);
    }
}

/// A generic struct that stores the old/new values of an environment change.
#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub(crate) struct EnvChangeTokenValuePairs<T> {
    old_value: T,
    new_value: T,
}

impl<T> EnvChangeTokenValuePairs<T> {
    /// Creates a new instance of EnvChangeTokenValue.
    pub fn new(old_value: T, new_value: T) -> Self {
        Self {
            old_value,
            new_value,
        }
    }

    /// Gets a reference to the old value.
    #[allow(dead_code)]
    pub fn old_value(&self) -> &T {
        &self.old_value
    }

    /// Gets a reference to the new value.
    pub fn new_value(&self) -> &T {
        &self.new_value
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    // ── TokenType::try_from ──

    #[test]
    fn token_type_try_from_all_valid() {
        let cases: &[(u8, TokenType)] = &[
            (0x88, TokenType::AltMetadata),
            (0xD3, TokenType::AltRow),
            (0x81, TokenType::ColMetadata),
            (0xA5, TokenType::ColInfo),
            (0xFD, TokenType::Done),
            (0xFE, TokenType::DoneProc),
            (0xFF, TokenType::DoneInProc),
            (0xE3, TokenType::EnvChange),
            (0xAA, TokenType::Error),
            (0xAE, TokenType::FeatureExtAck),
            (0xEE, TokenType::FedAuthInfo),
            (0xAB, TokenType::Info),
            (0xAD, TokenType::LoginAck),
            (0xD2, TokenType::NbcRow),
            (0x78, TokenType::Offset),
            (0xA9, TokenType::Order),
            (0x79, TokenType::ReturnStatus),
            (0xAC, TokenType::ReturnValue),
            (0xD1, TokenType::Row),
            (0xED, TokenType::SSPI),
            (0xA4, TokenType::TabName),
        ];
        for &(byte, ref expected) in cases {
            assert_eq!(&TokenType::try_from(byte).unwrap(), expected);
        }
    }

    #[test]
    fn token_type_try_from_invalid() {
        assert!(TokenType::try_from(0x00).is_err());
        assert!(TokenType::try_from(0x42).is_err());
    }

    // ── SqlCollation accessors ──

    #[test]
    fn sql_collation_accessors() {
        // info layout: bits 0-19 = lcid, bits 20-27 = comparison_style, bits 28-31 = version
        // lcid=0xABCDE, comparison_style=0x37, version=0x2
        // info = 0x2_37_ABCDE
        let col = SqlCollation {
            info: 0x237A_BCDE,
            lcid_language_id: 0xABCDE_i32,
            col_flags: 0x37,
            sort_id: 0x42,
        };
        assert_eq!(col.lcid_language_id(), 0xABCDE_i32);
        assert_eq!(col.comparison_style(), 0x37);
        assert_eq!(col.sort_id(), 0x42);
        assert_eq!(col.version(), 0x2);
    }

    #[test]
    fn sql_collation_flag_bits() {
        let make = |flags: u8| SqlCollation {
            info: 0,
            lcid_language_id: 0,
            col_flags: flags,
            sort_id: 0,
        };

        assert!(make(0x01).ignore_case());
        assert!(!make(0x00).ignore_case());

        assert!(make(0x02).ignore_accent());
        assert!(!make(0x00).ignore_accent());

        assert!(make(0x04).ignore_kana());
        assert!(!make(0x00).ignore_kana());

        assert!(make(0x08).ignore_width());
        assert!(!make(0x00).ignore_width());

        assert!(make(0x10).binary());
        assert!(!make(0x00).binary());

        assert!(make(0x20).binary2());
        assert!(!make(0x00).binary2());

        assert!(make(0x40).utf8());
        assert!(!make(0x00).utf8());
    }

    #[test]
    fn sql_collation_display() {
        let col = SqlCollation {
            info: 0x0400_0409,
            lcid_language_id: 0x0409,
            col_flags: 0x40,
            sort_id: 52,
        };
        let s = format!("{col}");
        assert!(s.contains("INFO:"));
        assert!(s.contains("IsUtf8: true"));
    }

    #[test]
    fn sql_collation_debug() {
        let col = SqlCollation {
            info: 0x0400_0409,
            lcid_language_id: 0x0409,
            col_flags: 0x01,
            sort_id: 0,
        };
        let s = format!("{col:?}");
        assert!(s.contains("IgnoreCase: true"));
    }

    // ── EnvChangeContainer Debug (uncovered variants) ──

    #[test]
    fn env_change_container_debug_routing() {
        let routing = RoutingInfo {
            protocol: 0,
            port: 1433,
            server: "host.example.com".into(),
        };
        let container: EnvChangeContainer = (Some(routing), None).into();
        let dbg = format!("{container:?}");
        assert!(dbg.starts_with("RoutingType:"));
    }

    #[test]
    fn env_change_container_debug_bytes() {
        let container: EnvChangeContainer = (vec![1u8, 2, 3], vec![4u8, 5]).into();
        let dbg = format!("{container:?}");
        assert!(dbg.starts_with("ByteType:"));
    }

    #[test]
    fn env_change_container_debug_u64() {
        let container: EnvChangeContainer = (100u64, 200u64).into();
        let dbg = format!("{container:?}");
        assert!(dbg.starts_with("UInt64"));
    }

    #[test]
    fn env_change_container_debug_string() {
        let container: EnvChangeContainer = ("old".to_string(), "new".to_string()).into();
        let dbg = format!("{container:?}");
        assert!(dbg.starts_with("String:"));
    }

    #[test]
    fn env_change_container_debug_u32() {
        let container: EnvChangeContainer = (4096u32, 8192u32).into();
        let dbg = format!("{container:?}");
        assert!(dbg.starts_with("UInt32:"));
    }

    #[test]
    fn env_change_container_debug_sql_collation() {
        let col = SqlCollation {
            info: 0,
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        };
        let container: EnvChangeContainer = (Some(col), None).into();
        let dbg = format!("{container:?}");
        assert!(dbg.starts_with("SqlCollation:"));
    }

    // ── DoneStatus bitflags ──

    #[test]
    fn done_status_from_u16_and_flags() {
        let status = DoneStatus::from(0x0001);
        assert!(status.contains(DoneStatus::MORE));

        let status = DoneStatus::from(0x0002);
        assert!(status.contains(DoneStatus::ERROR));

        let status = DoneStatus::from(0x0004);
        assert!(status.contains(DoneStatus::IN_XACT));

        let status = DoneStatus::from(0x0010);
        assert!(status.contains(DoneStatus::COUNT));

        let status = DoneStatus::from(0x0020);
        assert!(status.contains(DoneStatus::ATTN));

        let status = DoneStatus::from(0x0100);
        assert!(status.contains(DoneStatus::SERVER_ERROR));

        // Combined flags
        let status = DoneStatus::from(0x0013);
        assert!(status.contains(DoneStatus::MORE));
        assert!(status.contains(DoneStatus::ERROR));
        assert!(status.contains(DoneStatus::COUNT));

        // Truncation of unknown bits
        let status = DoneStatus::from(0xFFFF);
        assert!(status.contains(DoneStatus::MORE | DoneStatus::ERROR | DoneStatus::SERVER_ERROR));
    }

    #[test]
    fn done_token_helpers() {
        let token = DoneToken {
            status: DoneStatus::MORE | DoneStatus::ERROR,
            cur_cmd: CurrentCommand::Select,
            row_count: 0,
        };
        assert!(token.has_more());
        assert!(token.has_error());

        let token2 = DoneToken {
            status: DoneStatus::FINAL,
            cur_cmd: CurrentCommand::None,
            row_count: 0,
        };
        assert!(!token2.has_more());
        assert!(!token2.has_error());
    }

    // ── CurrentCommand::try_from ──

    #[test]
    fn current_command_try_from_all_known() {
        let cases: &[(u16, CurrentCommand)] = &[
            (0xc1, CurrentCommand::Select),
            (0xc3, CurrentCommand::Insert),
            (0xc4, CurrentCommand::Delete),
            (0xc5, CurrentCommand::Update),
            (0xd2, CurrentCommand::Abort),
            (0xd4, CurrentCommand::BeginXact),
            (0xd5, CurrentCommand::EndXact),
            (0xf0, CurrentCommand::BulkInsert),
            (0x20, CurrentCommand::OpenCursor),
            (0x117, CurrentCommand::Merge),
        ];
        for &(val, expected) in cases {
            assert_eq!(CurrentCommand::try_from(val).unwrap(), expected);
        }
    }

    #[test]
    fn current_command_try_from_unknown_maps_to_none() {
        assert_eq!(
            CurrentCommand::try_from(0x00).unwrap(),
            CurrentCommand::None
        );
        assert_eq!(
            CurrentCommand::try_from(0x9999).unwrap(),
            CurrentCommand::None
        );
    }

    // ── EnvChangeContainer equality ──

    #[test]
    fn env_change_container_eq() {
        let a: EnvChangeContainer = (1u64, 2u64).into();
        let b: EnvChangeContainer = (1u64, 2u64).into();
        let c: EnvChangeContainer = (1u64, 3u64).into();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // ── EnvChangeTokenValuePairs accessors ──

    #[test]
    fn env_change_value_pairs_accessors() {
        let pair = EnvChangeTokenValuePairs::new("old".to_string(), "new".to_string());
        assert_eq!(pair.old_value(), "old");
        assert_eq!(pair.new_value(), "new");
    }
}
