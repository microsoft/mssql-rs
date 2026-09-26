// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CLR user-defined type (UDT) parameter support.

use crate::core::TdsResult;
use crate::datatypes::sql_tvp::write_b_varchar;
use crate::error::Error;
use crate::io::packet_writer::PacketWriter;

/// The server-side identity of a CLR UDT (`db.schema.type`).
///
/// Only `type_name` is required. SQL Server resolves an unqualified name
/// against the current database and the caller's default schema, which is why
/// the driver sends the other parts as empty rather than inventing values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UdtTypeName {
    /// Catalog/database name.
    pub db_name: Option<String>,
    /// Schema name (e.g. `dbo`).
    pub schema_name: Option<String>,
    /// UDT name (e.g. `hierarchyid`).
    pub type_name: String,
}

impl UdtTypeName {
    /// Creates a UDT name. Leave `db_name` and `schema_name` `None` to let the
    /// server resolve the type against the current database and default schema.
    pub fn new(db_name: Option<String>, schema_name: Option<String>, type_name: String) -> Self {
        Self {
            db_name,
            schema_name,
            type_name,
        }
    }

    /// Validates the whole name before any of it reaches the wire.
    ///
    /// The type name is mandatory: the server cannot resolve the UDT without
    /// it, and msodbcsql refuses such a binding before it reaches the wire.
    ///
    /// Each part is also bounded to what a B_VARCHAR's `u8` count can express.
    /// That limit is re-checked in `write_b_varchar`, but checking it here too
    /// is what keeps a rejection local: the parts are written in sequence, and
    /// a long earlier part can fill a packet and flush it (`PacketWriter`
    /// sends on overflow) before a later part is even inspected. Without this
    /// preflight, invalid local input becomes a half-sent RPC that has to be
    /// cancelled and drained instead of failing before any network I/O.
    pub(crate) fn validate(&self) -> TdsResult<()> {
        if self.type_name.is_empty() {
            return Err(Error::UsageError(
                "UDT type name must not be empty".to_string(),
            ));
        }
        for part in [
            self.db_name.as_deref(),
            self.schema_name.as_deref(),
            Some(self.type_name.as_str()),
        ]
        .into_iter()
        .flatten()
        {
            let units = part.encode_utf16().count();
            if units > u8::MAX as usize {
                return Err(Error::UsageError(format!(
                    "type name part is too long: {units} UTF-16 code units (max 255)"
                )));
            }
        }
        Ok(())
    }
}

/// Writes a UDT parameter's `TYPE_INFO` name block: catalog, schema, and type
/// name as B_VARCHARs.
///
/// This is the whole block - unlike the `UDT_INFO` carried in `COLMETADATA`,
/// the parameter form has no `MAX_BYTE_SIZE` and no assembly-qualified name.
/// See msodbcsql's `CRPCPolicy::WriteUDTHeader`
/// (`Sql/Ntdbms/sqlncli/tds/tdsrpc.cpp`), which emits exactly these three parts
/// before the PLP body length.
pub(crate) async fn write_udt_type_name(
    packet_writer: &mut PacketWriter<'_>,
    type_name: &UdtTypeName,
) -> TdsResult<()> {
    type_name.validate()?;
    write_b_varchar(packet_writer, type_name.db_name.as_deref()).await?;
    write_b_varchar(packet_writer, type_name.schema_name.as_deref()).await?;
    write_b_varchar(packet_writer, Some(type_name.type_name.as_str())).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::packet_writer::TdsPacketWriter;
    use crate::io::packet_writer::tests::MockNetworkWriter;
    use crate::message::messages::PacketType;

    async fn type_name_bytes(name: &UdtTypeName) -> TdsResult<Vec<u8>> {
        let mut mock = MockNetworkWriter::new(4096);
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        write_udt_type_name(&mut writer, name).await?;
        writer.finalize().await?;
        Ok(mock.data[PacketWriter::PACKET_HEADER_SIZE..].to_vec())
    }

    /// The exact block `CRPCPolicy::WriteUDTHeader` emits
    /// (`Sql/Ntdbms/sqlncli/tds/tdsrpc.cpp:1099`): three B_VARCHARs, each a
    /// `u8` UTF-16 unit count followed by UTF-16LE characters, and nothing
    /// else - no `MAX_BYTE_SIZE` and no assembly-qualified name.
    #[tokio::test]
    async fn test_write_udt_type_name_bytes() {
        let name = UdtTypeName::new(
            Some("mydb".to_string()),
            Some("dbo".to_string()),
            "Point".to_string(),
        );
        let bytes = type_name_bytes(&name).await.unwrap();

        let expected: Vec<u8> = vec![
            0x04, b'm', 0x00, b'y', 0x00, b'd', 0x00, b'b', 0x00, // db "mydb"
            0x03, b'd', 0x00, b'b', 0x00, b'o', 0x00, // schema "dbo"
            0x05, b'P', 0x00, b'o', 0x00, b'i', 0x00, b'n', 0x00, b't', 0x00, // type "Point"
        ];
        assert_eq!(bytes, expected.as_slice());
    }

    /// An unqualified name sends both optional parts as zero-length, which is
    /// how the server is asked to resolve against the current database and
    /// default schema - matching `WriteUDTHeader`'s null-pointer branches.
    #[tokio::test]
    async fn test_write_udt_type_name_omits_absent_parts() {
        let name = UdtTypeName::new(None, None, "hierarchyid".to_string());
        let bytes = type_name_bytes(&name).await.unwrap();

        assert_eq!(bytes[0], 0x00, "absent catalog is a zero-length B_VARCHAR");
        assert_eq!(bytes[1], 0x00, "absent schema is a zero-length B_VARCHAR");
        assert_eq!(bytes[2], 11, "type name is 11 UTF-16 units");
        assert_eq!(bytes.len(), 3 + 11 * 2);
    }

    /// The length prefix counts UTF-16 code units, not bytes or `char`s, so a
    /// name outside the BMP must not be measured by its Rust length.
    #[tokio::test]
    async fn test_write_udt_type_name_counts_utf16_units() {
        // U+1D400 is one `char` but two UTF-16 code units.
        let name = UdtTypeName::new(None, None, "\u{1D400}".to_string());
        let bytes = type_name_bytes(&name).await.unwrap();

        assert_eq!(bytes[2], 2, "surrogate pair counts as two units");
        assert_eq!(bytes.len(), 3 + 4);
    }

    #[test]
    fn test_validate_empty_type_name_rejected() {
        let name = UdtTypeName::new(None, None, String::new());
        assert!(matches!(name.validate(), Err(Error::UsageError(_))));
    }

    #[test]
    fn test_validate_type_name_ok() {
        let name = UdtTypeName::new(None, None, "hierarchyid".to_string());
        assert!(name.validate().is_ok());
    }

    /// A name part past the B_VARCHAR `u8` count limit is refused rather than
    /// truncated: msodbcsql narrows the same count with a `(BYTE)` cast
    /// (`sqlcdesc.cpp:4944`), which would silently mis-frame the block.
    #[tokio::test]
    async fn test_write_udt_type_name_part_too_long_rejected() {
        let name = UdtTypeName::new(None, None, "a".repeat(256));
        let result = type_name_bytes(&name).await;
        assert!(matches!(result, Err(Error::UsageError(_))));
    }

    /// An oversized part is rejected before *any* byte is written, including
    /// when an earlier part is valid. The parts are written in sequence and
    /// `PacketWriter` sends on overflow, so a 255-unit catalog can fill and
    /// flush a small packet; without the preflight in `validate`, invalid
    /// local input would become a half-sent RPC needing cancel-and-drain
    /// rather than a clean local failure.
    #[tokio::test]
    async fn test_a_late_oversized_part_writes_nothing() {
        for name in [
            // Oversized schema behind a maximal catalog.
            UdtTypeName::new(
                Some("a".repeat(255)),
                Some("b".repeat(256)),
                "Point".to_string(),
            ),
            // Oversized type name behind two valid parts.
            UdtTypeName::new(
                Some("a".repeat(255)),
                Some("dbo".to_string()),
                "c".repeat(256),
            ),
        ] {
            assert!(matches!(name.validate(), Err(Error::UsageError(_))));

            // Packet size comes from the mock, not from `PacketWriter::new`
            // (whose third argument is a timeout). 512 bytes is small enough
            // that a 255-unit catalog part must overflow and flush.
            let mut mock = MockNetworkWriter::new(512);
            let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
            let result = write_udt_type_name(&mut writer, &name).await;

            assert!(matches!(result, Err(Error::UsageError(_))));
            assert!(
                mock.data.is_empty(),
                "no packet may reach the network before the name is validated"
            );
        }
    }
}
