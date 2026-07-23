// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod bulk_copy_xml_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use async_trait::async_trait;
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::connection::tds_client::ResultSet;
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::ColumnValues;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    #[derive(Debug, Clone)]
    struct XmlRow {
        id: i32,
        xml_col: Option<String>,
    }

    #[async_trait]
    impl BulkLoadRow for XmlRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            let xml_val = self
                .xml_col
                .as_ref()
                .map(|s| ColumnValues::Xml(s.clone().into()))
                .unwrap_or(ColumnValues::Null);
            writer.write_column_value(*column_index, &xml_val).await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[async_trait]
    impl BulkLoadRow for &XmlRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            let xml_val = self
                .xml_col
                .as_ref()
                .map(|s| ColumnValues::Xml(s.clone().into()))
                .unwrap_or(ColumnValues::Null);
            writer.write_column_value(*column_index, &xml_val).await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_xml_column() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table (automatically cleaned up)
        client
            .execute(
                "CREATE TABLE #BulkCopyXmlTest (id INT NOT NULL, xml_col XML NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // XML test constants
        const XML_SIMPLE_1: &str = "<root><child>Test1</child></root>";
        const XML_SIMPLE_2: &str = "<root><child>Test2</child></root>";
        const XML_WITH_NS_CDATA_INPUT: &str = r#"<root xmlns="http://example.com" attr="value"><child id="42">Test &amp; special &lt;chars&gt;</child><![CDATA[Raw <data> here]]></root>"#;
        // SQL Server processes CDATA and stores it differently
        const XML_WITH_NS_CDATA_STORED: &str = r#"<root xmlns="http://example.com" attr="value"><child id="42">Test &amp; special &lt;chars&gt;</child>Raw &lt;data&gt; here</root>"#;
        const XML_WITH_PI_INPUT: &str =
            r#"<?xml version="1.0"?><!-- Comment --><root><data>Test with PI</data></root>"#;
        // SQL Server strips XML declaration but keeps comments
        const XML_WITH_PI_STORED: &str =
            r#"<!-- Comment --><root><data>Test with PI</data></root>"#;
        const XML_WITH_ENTITIES_INPUT: &str = r#"<root><text>&lt;tag&gt; &amp; &quot;quoted&quot; &apos;apostrophe&apos;</text></root>"#;
        // SQL Server normalizes entities - &quot; becomes ", &apos; becomes '
        const XML_WITH_ENTITIES_STORED: &str =
            r#"<root><text>&lt;tag&gt; &amp; "quoted" 'apostrophe'</text></root>"#;
        const XML_EMPTY: &str = "";
        const XML_WITH_UNICODE: &str =
            r#"<root><emoji>🎉</emoji><chinese>中文</chinese><arabic>العربية</arabic></root>"#;

        // Prepare test data with various XML scenarios
        let test_data = vec![
            XmlRow {
                id: 1,
                xml_col: Some(XML_SIMPLE_1.to_string()),
            },
            XmlRow {
                id: 2,
                xml_col: Some(XML_SIMPLE_2.to_string()),
            },
            XmlRow {
                id: 3,
                xml_col: None,
            },
            XmlRow {
                id: 4,
                xml_col: Some(XML_WITH_NS_CDATA_INPUT.to_string()),
            },
            XmlRow {
                id: 5,
                xml_col: Some(XML_WITH_PI_INPUT.to_string()),
            },
            XmlRow {
                id: 6,
                xml_col: Some(XML_WITH_ENTITIES_INPUT.to_string()),
            },
            XmlRow {
                id: 7,
                xml_col: Some(XML_EMPTY.to_string()),
            },
            XmlRow {
                id: 8,
                xml_col: Some(XML_WITH_UNICODE.to_string()),
            },
        ];

        // Execute bulk copy
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyXmlTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 8, "Expected 8 rows to be inserted");

        // Verify the data was inserted
        client
            .execute(
                "SELECT id, xml_col FROM #BulkCopyXmlTest ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                match row_count {
                    1 => {
                        assert_eq!(row[0], ColumnValues::Int(1));
                        assert_eq!(row[1], ColumnValues::Xml(XML_SIMPLE_1.to_string().into()));
                    }
                    2 => {
                        assert_eq!(row[0], ColumnValues::Int(2));
                        assert_eq!(row[1], ColumnValues::Xml(XML_SIMPLE_2.to_string().into()));
                    }
                    3 => {
                        assert_eq!(row[0], ColumnValues::Int(3));
                        assert_eq!(row[1], ColumnValues::Null);
                    }
                    4 => {
                        assert_eq!(row[0], ColumnValues::Int(4));
                        // SQL Server processes CDATA sections when storing XML
                        assert_eq!(
                            row[1],
                            ColumnValues::Xml(XML_WITH_NS_CDATA_STORED.to_string().into())
                        );
                    }
                    5 => {
                        assert_eq!(row[0], ColumnValues::Int(5));
                        // SQL Server strips XML declaration
                        assert_eq!(
                            row[1],
                            ColumnValues::Xml(XML_WITH_PI_STORED.to_string().into())
                        );
                    }
                    6 => {
                        assert_eq!(row[0], ColumnValues::Int(6));
                        // SQL Server normalizes &quot; and &apos; entities
                        assert_eq!(
                            row[1],
                            ColumnValues::Xml(XML_WITH_ENTITIES_STORED.to_string().into())
                        );
                    }
                    7 => {
                        assert_eq!(row[0], ColumnValues::Int(7));
                        assert_eq!(row[1], ColumnValues::Xml(XML_EMPTY.to_string().into()));
                    }
                    8 => {
                        assert_eq!(row[0], ColumnValues::Int(8));
                        assert_eq!(
                            row[1],
                            ColumnValues::Xml(XML_WITH_UNICODE.to_string().into())
                        );
                    }
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 8, "Expected 8 rows in result");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_xml_with_bom() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyXmlBomTest (id INT NOT NULL, xml_col XML NULL)".to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        const XML_WITHOUT_BOM: &str = "<root><child>Without BOM</child></root>";

        let test_data = vec![
            // XML with UTF-8 BOM
            XmlRow {
                id: 1,
                xml_col: Some(format!("\u{FEFF}{}", XML_WITHOUT_BOM)),
            },
            // XML without BOM
            XmlRow {
                id: 2,
                xml_col: Some(XML_WITHOUT_BOM.to_string()),
            },
        ];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyXmlBomTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 2, "Expected 2 rows to be inserted");

        // Verify both inserted successfully
        client
            .execute(
                "SELECT id, xml_col FROM #BulkCopyXmlBomTest ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                match row_count {
                    1 => {
                        assert_eq!(row[0], ColumnValues::Int(1));
                        // SQL Server should strip BOM and store the XML correctly
                        assert_eq!(
                            row[1],
                            ColumnValues::Xml(XML_WITHOUT_BOM.to_string().into())
                        );
                    }
                    2 => {
                        assert_eq!(row[0], ColumnValues::Int(2));
                        assert_eq!(
                            row[1],
                            ColumnValues::Xml(XML_WITHOUT_BOM.to_string().into())
                        );
                    }
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 2, "Expected 2 rows in result");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_xml_malformed_error() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyXmlMalformedTest (id INT NOT NULL, xml_col XML NULL)"
                    .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Malformed XML - unclosed tag
        let test_data = vec![XmlRow {
            id: 1,
            xml_col: Some("<root><child>Unclosed tag".to_string()),
        }];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyXmlMalformedTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&test_data)
                .await
        };

        // Should fail with XML parsing error
        assert!(result.is_err(), "Expected malformed XML to fail");
        let err = result.unwrap_err();
        let err_msg = format!("{:?}", err);
        assert!(
            err_msg.contains("XML") || err_msg.contains("parsing"),
            "Expected XML parsing error, got: {}",
            err_msg
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_xml_large_document() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #BulkCopyXmlLargeTest (id INT NOT NULL, xml_col XML NULL)"
                    .to_string(),
                (),
            )
            .await
            .expect("Failed to create test table");
        client.close_query().await.expect("Failed to close query");

        // Generate a large XML document
        let mut large_xml = String::from("<root>");
        for i in 0..1000 {
            large_xml.push_str(&format!("<item id=\"{}\">{}</item>", i, "Data ".repeat(10)));
        }
        large_xml.push_str("</root>");

        let test_data = vec![XmlRow {
            id: 1,
            xml_col: Some(large_xml),
        }];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyXmlLargeTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&test_data)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 1, "Expected 1 row to be inserted");

        // Verify it was inserted
        client
            .execute("SELECT id FROM #BulkCopyXmlLargeTest".to_string(), ())
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while resultset
                .next_row()
                .await
                .expect("Failed to read row")
                .is_some()
            {
                row_count += 1;
            }
        }
        assert_eq!(row_count, 1, "Expected 1 row in result");
    }
}
