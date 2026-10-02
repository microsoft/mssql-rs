// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Initial connection and query execution layer for the Windows OLE DB provider.
//!
//! This crate currently provides the TDS-backed synchronous execution core. The
//! COM provider interfaces and OLE DB rowset/accessor binding are not implemented
//! yet.

mod connection_string;
mod session;

pub use connection_string::{ConnectionOptions, ConnectionStringError};
pub use session::{MssqlSession, QueryResult, SessionError};

#[cfg(test)]
mod tests {
    use super::{ConnectionOptions, ConnectionStringError};

    #[test]
    fn parses_common_oledb_connection_properties() {
        let options = ConnectionOptions::parse(
            "Provider=MSOLEDBSQL;Data Source={tcp:localhost,1433};Initial Catalog=master;\
             User ID=sa;Encrypt=Mandatory;\
             TrustServerCertificate=yes",
        )
        .unwrap();

        assert_eq!(options.data_source, "tcp:localhost,1433");
        assert_eq!(options.initial_catalog, "master");
        assert_eq!(options.user_id, "sa");
        assert!(options.password.is_empty());
        assert!(options.trust_server_certificate);
    }

    #[test]
    fn rejects_missing_data_source() {
        assert_eq!(
            ConnectionOptions::parse("User ID=sa"),
            Err(ConnectionStringError::MissingDataSource)
        );
    }

    #[test]
    fn rejects_integrated_authentication_until_supported() {
        assert_eq!(
            ConnectionOptions::parse("Data Source=localhost;Integrated Security=SSPI"),
            Err(ConnectionStringError::UnsupportedProperty(
                "Integrated Security".into()
            ))
        );
    }

    #[test]
    fn rejects_unterminated_braced_values() {
        assert_eq!(
            ConnectionOptions::parse("Data Source={localhost"),
            Err(ConnectionStringError::InvalidValue)
        );
    }
}
