// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Additional unit tests for bulk load message module
//! These tests focus on command generation and options handling.

#[cfg(test)]
mod tests {

    use crate::datatypes::bulk_copy_metadata::{BulkCopyColumnMetadata, SqlDbType, TypeLength};
    use crate::token::tokens::SqlCollation;

    // Helper function to create a simple int column
    fn create_int_column(name: &str) -> BulkCopyColumnMetadata {
        BulkCopyColumnMetadata::new(name, SqlDbType::Int, 0x26)
            .with_nullable(false)
            .with_length(4, TypeLength::Fixed(4))
    }

    // Helper function to create a varchar column
    fn create_varchar_column(name: &str, length: i32) -> BulkCopyColumnMetadata {
        BulkCopyColumnMetadata::new(name, SqlDbType::VarChar, 0xA7)
            .with_nullable(true)
            .with_length(length, TypeLength::Variable(length))
            .with_collation(SqlCollation::default())
    }

    // Helper function to create an nvarchar column
    // Note: length parameter is in CHARACTERS, but internally stored as BYTES (length * 2)
    fn create_nvarchar_column(name: &str, char_length: i32) -> BulkCopyColumnMetadata {
        let byte_length = char_length * 2; // NVARCHAR uses 2 bytes per character
        BulkCopyColumnMetadata::new(name, SqlDbType::NVarChar, 0xE7)
            .with_nullable(true)
            .with_length(byte_length, TypeLength::Variable(byte_length))
            .with_collation(SqlCollation::default())
    }

    #[test]
    fn test_tds_type_mapping_comprehensive() {
        // Test all major TDS type mappings
        assert_eq!(SqlDbType::TinyInt.to_tds_type(), 0x26);
        assert_eq!(SqlDbType::SmallInt.to_tds_type(), 0x26);
        assert_eq!(SqlDbType::Int.to_tds_type(), 0x26);
        assert_eq!(SqlDbType::BigInt.to_tds_type(), 0x26);
        assert_eq!(SqlDbType::Bit.to_tds_type(), 0x68);
        assert_eq!(SqlDbType::Real.to_tds_type(), 0x6D);
        assert_eq!(SqlDbType::Float.to_tds_type(), 0x6D);
        assert_eq!(SqlDbType::Decimal.to_tds_type(), 0x6A);
        assert_eq!(SqlDbType::Numeric.to_tds_type(), 0x6C);
        assert_eq!(SqlDbType::Money.to_tds_type(), 0x6E);
        assert_eq!(SqlDbType::SmallMoney.to_tds_type(), 0x6E);
        assert_eq!(SqlDbType::Date.to_tds_type(), 0x28);
        assert_eq!(SqlDbType::Time.to_tds_type(), 0x29);
        assert_eq!(SqlDbType::DateTime.to_tds_type(), 0x6F);
        assert_eq!(SqlDbType::DateTime2.to_tds_type(), 0x2A);
        assert_eq!(SqlDbType::DateTimeOffset.to_tds_type(), 0x2B);
        assert_eq!(SqlDbType::SmallDateTime.to_tds_type(), 0x6F);
        assert_eq!(SqlDbType::Char.to_tds_type(), 0xAF);
        assert_eq!(SqlDbType::VarChar.to_tds_type(), 0xA7);
        assert_eq!(SqlDbType::Text.to_tds_type(), 0x23);
        assert_eq!(SqlDbType::NChar.to_tds_type(), 0xEF);
        assert_eq!(SqlDbType::NVarChar.to_tds_type(), 0xE7);
        assert_eq!(SqlDbType::NText.to_tds_type(), 0x63);
        assert_eq!(SqlDbType::Binary.to_tds_type(), 0xAD);
        assert_eq!(SqlDbType::VarBinary.to_tds_type(), 0xA5);
        assert_eq!(SqlDbType::Image.to_tds_type(), 0x22);
        assert_eq!(SqlDbType::UniqueIdentifier.to_tds_type(), 0x24);
        assert_eq!(SqlDbType::Xml.to_tds_type(), 0xF1);
        assert_eq!(SqlDbType::Json.to_tds_type(), 0xF4);
        assert_eq!(SqlDbType::Variant.to_tds_type(), 0x62);
        assert_eq!(SqlDbType::Udt.to_tds_type(), 0xF0);
        assert_eq!(SqlDbType::Vector.to_tds_type(), 0xF5);
    }

    #[test]
    fn test_tds_type_fixed_mapping() {
        // Test fixed-length type mappings
        assert_eq!(SqlDbType::TinyInt.to_tds_type_fixed(), 0x30);
        assert_eq!(SqlDbType::SmallInt.to_tds_type_fixed(), 0x34);
        assert_eq!(SqlDbType::Int.to_tds_type_fixed(), 0x38);
        assert_eq!(SqlDbType::BigInt.to_tds_type_fixed(), 0x7F);
        assert_eq!(SqlDbType::Bit.to_tds_type_fixed(), 0x32);
        assert_eq!(SqlDbType::Real.to_tds_type_fixed(), 0x3B);
        assert_eq!(SqlDbType::Float.to_tds_type_fixed(), 0x3E);

        // Types without fixed versions should return nullable variant
        assert_eq!(
            SqlDbType::Decimal.to_tds_type_fixed(),
            SqlDbType::Decimal.to_tds_type()
        );
        assert_eq!(
            SqlDbType::NVarChar.to_tds_type_fixed(),
            SqlDbType::NVarChar.to_tds_type()
        );
    }

    #[test]
    fn test_bulk_copy_tds_type_mapping() {
        // Test that most types return the same TDS type for bulk copy
        assert_eq!(
            SqlDbType::Int.to_bulk_copy_tds_type(),
            SqlDbType::Int.to_tds_type()
        );
        assert_eq!(
            SqlDbType::VarChar.to_bulk_copy_tds_type(),
            SqlDbType::VarChar.to_tds_type()
        );
        assert_eq!(
            SqlDbType::NVarChar.to_bulk_copy_tds_type(),
            SqlDbType::NVarChar.to_tds_type()
        );
        assert_eq!(
            SqlDbType::DateTime.to_bulk_copy_tds_type(),
            SqlDbType::DateTime.to_tds_type()
        );
        assert_eq!(
            SqlDbType::Vector.to_bulk_copy_tds_type(),
            SqlDbType::Vector.to_tds_type()
        );

        // Test that JSON is properly identified as 0xF4
        assert_eq!(
            SqlDbType::Json.to_tds_type(),
            0xF4,
            "JSON should return 0xF4 (TdsDataType::Json) from to_tds_type()"
        );

        // Test that JSON returns NVARCHAR for bulk copy
        assert_eq!(
            SqlDbType::Json.to_bulk_copy_tds_type(),
            0xE7,
            "JSON should return 0xE7 (TdsDataType::NVarChar) from to_bulk_copy_tds_type() for bulk copy operations"
        );

        // Verify JSON is treated differently than NVARCHAR
        assert_ne!(
            SqlDbType::Json.to_tds_type(),
            SqlDbType::NVarChar.to_tds_type(),
            "JSON type identifier (0xF4) should differ from NVARCHAR (0xE7)"
        );

        // Verify JSON uses NVARCHAR for bulk copy
        assert_eq!(
            SqlDbType::Json.to_bulk_copy_tds_type(),
            SqlDbType::NVarChar.to_tds_type(),
            "JSON should use NVARCHAR encoding (0xE7) for bulk copy"
        );

        // Test that XML is properly identified as 0xF1
        assert_eq!(
            SqlDbType::Xml.to_tds_type(),
            0xF1,
            "XML should return 0xF1 (TdsDataType::Xml) from to_tds_type()"
        );

        // Test that XML returns NVARCHAR for bulk copy
        assert_eq!(
            SqlDbType::Xml.to_bulk_copy_tds_type(),
            0xE7,
            "XML should return 0xE7 (TdsDataType::NVarChar) from to_bulk_copy_tds_type() for bulk copy operations"
        );

        // Verify XML is treated differently than NVARCHAR
        assert_ne!(
            SqlDbType::Xml.to_tds_type(),
            SqlDbType::NVarChar.to_tds_type(),
            "XML type identifier (0xF1) should differ from NVARCHAR (0xE7)"
        );

        // Verify XML uses NVARCHAR for bulk copy
        assert_eq!(
            SqlDbType::Xml.to_bulk_copy_tds_type(),
            SqlDbType::NVarChar.to_tds_type(),
            "XML should use NVARCHAR encoding (0xE7) for bulk copy"
        );
    }

    #[test]
    fn test_insert_bulk_with_collation() {
        use crate::connection::bulk_copy::BulkCopyOptions;
        use crate::message::bulk_load::build_insert_bulk_command;

        // Create metadata with collation names
        let col1 = create_int_column("Id");
        let mut col2 = create_nvarchar_column("Name", 100);
        col2.collation_name = Some("SQL_Latin1_General_CP1_CI_AS".to_string());
        let mut col3 = create_varchar_column("Description", 255);
        col3.collation_name = Some("Latin1_General_BIN".to_string());

        let metadata = vec![col1, col2, col3];
        let options = BulkCopyOptions::default();

        let command = build_insert_bulk_command("dbo.TestTable", &metadata, &options).unwrap();

        // Verify COLLATE is included for character types
        assert!(
            command.contains("COLLATE SQL_Latin1_General_CP1_CI_AS"),
            "Expected COLLATE clause for nvarchar column, got: {}",
            command
        );
        assert!(
            command.contains("COLLATE Latin1_General_BIN"),
            "Expected COLLATE clause for varchar column, got: {}",
            command
        );

        // Verify COLLATE is NOT included for int column
        let int_section = &command[..command.find("Name").unwrap()];
        assert!(
            !int_section.contains("COLLATE"),
            "Int column should not have COLLATE clause, got: {}",
            command
        );
    }

    #[test]
    fn test_insert_bulk_without_collation() {
        use crate::connection::bulk_copy::BulkCopyOptions;
        use crate::message::bulk_load::build_insert_bulk_command;

        // Create metadata without collation names
        let col1 = create_int_column("Id");
        let col2 = create_nvarchar_column("Name", 100);
        let col3 = create_varchar_column("Description", 255);

        let metadata = vec![col1, col2, col3];
        let options = BulkCopyOptions::default();

        let command = build_insert_bulk_command("dbo.TestTable", &metadata, &options).unwrap();

        // Verify no COLLATE clauses are present
        assert!(
            !command.contains("COLLATE"),
            "Should not have COLLATE clause when collation_name is None, got: {}",
            command
        );

        // Verify basic structure is correct
        assert!(command.starts_with("INSERT BULK [dbo].[TestTable] ("));
        assert!(command.contains("[Id] int"));
        assert!(command.contains("[Name] nvarchar(100)"));
        assert!(command.contains("[Description] varchar(255)"));
    }

    #[test]
    fn test_insert_bulk_mixed_collation() {
        use crate::connection::bulk_copy::BulkCopyOptions;
        use crate::message::bulk_load::build_insert_bulk_command;

        // Create metadata with some columns having collation and some not
        let mut col1 = create_nvarchar_column("Name", 50);
        col1.collation_name = Some("SQL_Latin1_General_CP1_CI_AS".to_string());

        let col2 = create_nvarchar_column("Description", 200); // No collation

        let mut col3 = create_varchar_column("Code", 10);
        col3.collation_name = Some("Latin1_General_BIN".to_string());

        let metadata = vec![col1, col2, col3];
        let options = BulkCopyOptions::default();

        let command = build_insert_bulk_command("dbo.MixedTable", &metadata, &options).unwrap();

        // Verify first column has COLLATE
        assert!(
            command.contains("[Name] nvarchar(50) COLLATE SQL_Latin1_General_CP1_CI_AS"),
            "First column should have COLLATE, got: {}",
            command
        );

        // Verify second column does NOT have COLLATE
        let desc_pos = command.find("Description").unwrap();
        let next_comma_pos = command[desc_pos..]
            .find(',')
            .unwrap_or(command.len() - desc_pos);
        let desc_section = &command[desc_pos..desc_pos + next_comma_pos];
        assert!(
            !desc_section.contains("COLLATE"),
            "Description column should not have COLLATE, got: {}",
            desc_section
        );

        // Verify third column has COLLATE
        assert!(
            command.contains("[Code] varchar(10) COLLATE Latin1_General_BIN"),
            "Third column should have COLLATE, got: {}",
            command
        );
    }

    #[test]
    fn test_insert_bulk_collation_with_options() {
        use crate::connection::bulk_copy::BulkCopyOptions;
        use crate::message::bulk_load::build_insert_bulk_command;

        let mut col1 = create_nvarchar_column("Name", 100);
        col1.collation_name = Some("SQL_Latin1_General_CP1_CI_AS".to_string());

        let metadata = vec![col1];
        let options = BulkCopyOptions {
            keep_nulls: true,
            table_lock: true,
            ..BulkCopyOptions::default()
        };

        let command = build_insert_bulk_command("dbo.TestTable", &metadata, &options).unwrap();

        // Verify COLLATE clause is present
        assert!(
            command.contains("COLLATE SQL_Latin1_General_CP1_CI_AS"),
            "Expected COLLATE clause, got: {}",
            command
        );

        // Verify WITH clause is present with options
        assert!(
            command.contains("WITH (KEEP_NULLS, TABLOCK)"),
            "Expected WITH clause with options, got: {}",
            command
        );
    }

    #[test]
    fn test_insert_bulk_quotes_destination_parts() {
        use crate::connection::bulk_copy::BulkCopyOptions;
        use crate::message::bulk_load::build_insert_bulk_command;

        let metadata = [create_int_column("id")];
        for (name, expected) in [
            ("Target", "[Target]"),
            (" dbo . Target ", "[dbo].[Target]"),
            ("db.dbo.Target", "[db].[dbo].[Target]"),
            ("server.db.dbo.Target", "[server].[db].[dbo].[Target]"),
            ("db..Target", "[db]..[Target]"),
            (".Target", ".[Target]"),
            ("..Target", "..[Target]"),
            ("server...Target", "[server]...[Target]"),
            ("#Target", "[#Target]"),
            ("##Target", "[##Target]"),
            ("tempdb..#Target", "[tempdb]..[#Target]"),
            (
                "[db.with.dot].[schema].[a]]b]",
                "[db.with.dot].[schema].[a]]b]",
            ),
            ("\"db\".\"a\"\"b\".\" Target \"", "[db].[a\"b].[ Target ]"),
            ("[O'Brien].[t'ab]]le]", "[O'Brien].[t'ab]]le]"),
            (
                "dbo.Target ([id] int); SELECT 4242 AS audit_probe;--",
                "[dbo].[Target ([id]] int); SELECT 4242 AS audit_probe;--]",
            ),
            ("[a]]; SELECT 4242;--]", "[a]]; SELECT 4242;--]"),
        ] {
            let command =
                build_insert_bulk_command(name, &metadata, &BulkCopyOptions::default()).unwrap();
            assert_eq!(
                command,
                format!("INSERT BULK {expected} ([id] int)"),
                "{name}"
            );
        }
    }

    #[test]
    fn test_insert_bulk_quotes_column_identifiers() {
        use crate::connection::bulk_copy::BulkCopyOptions;
        use crate::message::bulk_load::build_insert_bulk_command;

        for (name, expected) in [
            ("id", "[id]"),
            ("a]b", "[a]]b]"),
            ("a]]b", "[a]]]]b]"),
            ("O'Brien", "[O'Brien]"),
            ("a.b", "[a.b]"),
            (" column ", "[ column ]"),
            ("a\"b", "[a\"b]"),
            (
                "id] int); SELECT 4242 AS audit_probe;--",
                "[id]] int); SELECT 4242 AS audit_probe;--]",
            ),
        ] {
            let command = build_insert_bulk_command(
                "dbo.Target",
                &[create_int_column(name)],
                &BulkCopyOptions::default(),
            )
            .unwrap();
            assert_eq!(
                command,
                format!("INSERT BULK [dbo].[Target] ({expected} int)")
            );
        }
    }

    #[test]
    fn test_insert_bulk_rejects_missing_or_invalid_target() {
        use crate::connection::bulk_copy::BulkCopyOptions;
        use crate::error::Error;
        use crate::message::bulk_load::build_insert_bulk_command;

        for name in [
            "",
            " ",
            ".",
            "dbo.",
            "db..",
            "db.dbo. ",
            "[]",
            "db.dbo.[]",
            "\"\"",
            "[t",
            "[t]extra",
            "a.b.c.d.e",
            "a.b.c.d.",
        ] {
            assert!(
                matches!(
                    build_insert_bulk_command(
                        name,
                        &[create_int_column("id")],
                        &BulkCopyOptions::default()
                    ),
                    Err(Error::UsageError(_))
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn test_insert_bulk_rejects_invalid_collation_tokens() {
        use crate::connection::bulk_copy::BulkCopyOptions;
        use crate::error::Error;
        use crate::message::bulk_load::build_insert_bulk_command;

        let too_long = "A".repeat(129);
        for name in [
            "",
            " Latin1_General_CI_AS",
            "Latin1_General_CI_AS ",
            "1Latin1",
            "_Latin1",
            "Latin1-General",
            "Latin1.General",
            "[Latin1_General_CI_AS]",
            "'Latin1_General_CI_AS'",
            "Latin1_General_CI_AS); SELECT 4242 AS audit_probe;--",
            "Latin1_General_CI_AS--",
            "Latin1_General_CI_AS/**/",
            "Latin1\nGeneral",
            "Latin1\0General",
            "Lat\u{ed}n1_General",
            &too_long,
        ] {
            let metadata = [create_nvarchar_column("Name", 10).with_collation_name(name)];
            assert!(
                matches!(
                    build_insert_bulk_command("Target", &metadata, &BulkCopyOptions::default()),
                    Err(Error::UsageError(message)) if message.starts_with("Invalid collation name:")
                ),
                "{name:?}"
            );
        }
    }

    #[test]
    fn test_insert_bulk_accepts_collation_tokens() {
        use crate::connection::bulk_copy::BulkCopyOptions;
        use crate::message::bulk_load::build_insert_bulk_command;

        let max_length = "A".repeat(128);
        for name in [
            "SQL_Latin1_General_CP1_CI_AS",
            "Latin1_General_100_CI_AS_SC_UTF8",
            "Japanese_XJIS_140_CI_AS_KS_WS_VSS",
            "SQL_EBCDIC277_2_CP1_CS_AS",
            "Latin1_General_BIN2",
            "a",
            &max_length,
        ] {
            for column in [
                create_nvarchar_column("Name", 10),
                create_varchar_column("Name", 10),
            ] {
                let command = build_insert_bulk_command(
                    "Target",
                    &[column.with_collation_name(name)],
                    &BulkCopyOptions::default(),
                )
                .unwrap();
                assert!(command.ends_with(&format!(" COLLATE {name})")), "{command}");
            }
        }
    }
}
