// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Wire encoding of a PLP column; used to select and transcode the delivered
/// SQL C type. UTF-16 text can be delivered as SQL_C_WCHAR or transcoded to
/// SQL_C_CHAR; single-byte text as SQL_C_CHAR. Binary delivery and
/// varchar->SQL_C_WCHAR widening are not yet supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlpEncoding {
    /// nvarchar(max), nchar(max), xml — UTF-16LE on the wire.
    Utf16Text,
    /// varchar(max), text, json — single-byte / UTF-8 on the wire.
    SingleByteText,
    /// varbinary(max), image, UDT — opaque bytes.
    Binary,
}
