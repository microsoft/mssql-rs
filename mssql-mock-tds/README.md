# Mock TDS Server

A mock TDS (Tabular Data Stream) server implementation for testing TDS clients without requiring an actual SQL Server instance. This crate provides both a library and a CLI.

## Overview

This crate provides a lightweight mock server that implements enough of the TDS protocol to test client connectivity, authentication, and query execution. It's designed specifically for testing the `mssql-tds` client implementation with configurable query responses.

## Features

- ✅ **PreLogin negotiation** - Handles PreLogin packets and signals encryption support
- ✅ **TLS encryption (TDS 7.4)** - Supports optional TLS with wrapped TDS packets
- ✅ **Strict TLS (TDS 8.0)** - Supports strict TLS mode where TLS starts immediately
- ✅ **Login7 authentication** - Accepts login requests and sends LoginAck responses
- ✅ **Packet size negotiation** - Properly negotiates packet size via EnvChange tokens
- ✅ **Database collation** - Returns collation information via EnvChange tokens
- ✅ **Database context** - Signals database name via EnvChange tokens
- ✅ **Flexible query execution** - Register custom queries with their expected responses
- ✅ **Multiple data types** - Support for TinyInt, SmallInt, Int, BigInt
- ✅ **NULL values** - Properly serializes NULL values in result sets
- ✅ **Result set parsing** - Correctly formats ColMetadata and Row tokens
- ✅ **Multiple queries** - Can handle multiple sequential queries on same connection
- ✅ **Connection reuse** - Supports multiple connections sequentially
- ❌ **String data types** - VARCHAR, NVARCHAR not yet supported

## CLI Usage

The crate includes a CLI for running the mock server standalone:

```bash
# Start without TLS
cargo run -p mssql-mock-tds -- --port 1433

# Start with optional TLS (TDS 7.4 mode)
cargo run -p mssql-mock-tds -- --port 1433 --tls-mode optional \
    --cert path/to/cert.pem --key path/to/key.pem

# Start with strict TLS (TDS 8.0 mode)
cargo run -p mssql-mock-tds -- --port 1433 --tls-mode strict \
    --cert path/to/cert.pem --key path/to/key.pem

# Use PKCS#12 file instead of PEM
cargo run -p mssql-mock-tds -- --port 1433 --tls-mode optional \
    --pfx path/to/identity.pfx --pfx-password mypassword

# Enable verbose logging
cargo run -p mssql-mock-tds -- --port 1433 -vv
```

### CLI Options

| Option | Description | Default |
|--------|-------------|---------|
| `-H, --host` | Host address to bind to | `127.0.0.1` |
| `-p, --port` | Port to listen on | `1433` |
| `-m, --tls-mode` | TLS mode: `none`, `optional`, `strict` | `none` |
| `-c, --cert` | Path to PEM certificate file | - |
| `-k, --key` | Path to PEM private key file | - |
| `--pfx` | Path to PKCS#12 (.pfx) identity file | - |
| `--pfx-password` | Password for PKCS#12 file | `""` |
| `-v, --verbose` | Enable verbose logging (repeat for more) | - |

## Library Usage

### Starting the Mock Server

```rust
use mssql_mock_tds::MockTdsServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create server on a random port
    let server = MockTdsServer::new("127.0.0.1:0").await?;
    let addr = server.local_addr();
    
    println!("Mock server listening on {}", addr);
    
    // Run server (blocks until shutdown)
    server.run().await?;
    
    Ok(())
}
```

### Running with Shutdown Signal

```rust
use mssql_mock_tds::MockTdsServer;
use tokio::sync::oneshot;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockTdsServer::new("127.0.0.1:1434").await?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    
    // Spawn server task
    let server_handle = tokio::spawn(async move {
        server.run_with_shutdown(shutdown_rx).await
    });
    
    // Do some work...
    // ...
    
    // Shutdown server
    let _ = shutdown_tx.send(());
    server_handle.await??;
    
    Ok(())
}
```

### Configuring Custom Query Responses

The mock server allows you to register custom query responses with specific column definitions and row data:

```rust
use mssql_mock_tds::{
    MockTdsServer, QueryResponse, ColumnDefinition, Row, ColumnValue, SqlDataType
};

#[tokio::test]
async fn test_custom_query() -> Result<(), Box<dyn std::error::Error>> {
    // Start mock server
    let server = MockTdsServer::new("127.0.0.1:0").await?;
    let server_addr = server.local_addr();
    
    // Register a custom query response
    let registry = server.query_registry();
    {
        let mut reg = registry.lock().await;
        reg.register(
            "SELECT CAST(1 AS BIGINT), 2, 3",
            QueryResponse::new(
                vec![
                    ColumnDefinition::new("col1", SqlDataType::BigInt),
                    ColumnDefinition::new("col2", SqlDataType::Int),
                    ColumnDefinition::new("col3", SqlDataType::Int),
                ],
                vec![Row::new(vec![
                    ColumnValue::BigInt(1),
                    ColumnValue::Int(2),
                    ColumnValue::Int(3),
                ])],
            ),
        );
    }
    
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        server.run_with_shutdown(shutdown_rx).await
    });
    
    // Connect and execute the custom query
    let provider = TdsConnectionProvider {};
    let mut client = provider.create_client(context, None).await?;
    
    client.execute("SELECT CAST(1 AS BIGINT), 2, 3".to_string(), None, None).await?;
    // ... process results ...
    
    let _ = shutdown_tx.send(());
    Ok(())
}
```

### Supported Data Types

The mock server currently supports the following SQL data types:

- `SqlDataType::TinyInt` - 1 byte integer (0-255)
- `SqlDataType::SmallInt` - 2 byte integer
- `SqlDataType::Int` - 4 byte integer
- `SqlDataType::BigInt` - 8 byte integer

You can also use `ColumnValue::Null` to represent NULL values in result sets.

### Testing with TdsClient

```rust
use mssql_mock_tds::MockTdsServer;
use mssql_tds::connection::client_context::{ClientContext, TransportContext};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting, ServerTrust};

#[tokio::test]
async fn test_connectivity() -> Result<(), Box<dyn std::error::Error>> {
    // Start mock server
    let server = MockTdsServer::new("127.0.0.1:0").await?;
    let server_addr = server.local_addr();
    
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        server.run_with_shutdown(shutdown_rx).await
    });
    
    // Create client context
    let context = ClientContext {
        transport_context: TransportContext::Tcp {
            host: server_addr.ip().to_string(),
            port: server_addr.port(),
        },
        user_name: "sa".to_string(),
        password: "password".to_string(),
        database: "master".to_string(),
        encryption_options: EncryptionOptions::new()
            .with_mode(EncryptionSetting::PreferOff)
            .with_server_trust(ServerTrust::DangerAcceptAny),
        ..Default::default()
    };
    
    // Connect to mock server
    let provider = TdsConnectionProvider {};
    let mut client = provider.create_client(context, None).await?;
    
    // Execute query
    client.execute("SELECT 1".to_string(), None, None).await?;
    client.close_query().await?;
    
    // Cleanup
    let _ = shutdown_tx.send(());
    
    Ok(())
}
```

## Architecture

### Protocol Module (`protocol.rs`)

Handles TDS protocol packet parsing and generation:
- Packet header parsing
- Token generation (LoginAck, EnvChange, Done, ColMetadata, Row)
- SQL batch parsing
- PreLogin response building
- Generic query result builder

### Query Response Module (`query_response.rs`)

Defines the data structures for configurable query responses:
- `SqlDataType` - Supported SQL data types
- `ColumnValue` - Values that can be serialized in a row
- `ColumnDefinition` - Column metadata (name and type)
- `Row` - A single row of data
- `QueryResponse` - Complete result set definition
- `QueryRegistry` - Manages query-to-response mappings

### Server Module (`server.rs`)

Implements the mock server logic:
- TCP connection handling
- Packet routing and response generation
- Connection state management
- Query registry lookup for custom responses
- Graceful shutdown support

## Current Limitations

1. **No SSL/TLS Support**: The server signals encryption as "NOT_SUPPORTED" during PreLogin
2. **Limited Data Types**: Only integer types (TinyInt, SmallInt, Int, BigInt) currently supported
3. **No Transaction Support**: Transaction commands are acknowledged but not enforced
4. **No Stored Procedures**: RPC calls are acknowledged but not executed
5. **Case Insensitive Queries**: Query matching is case-insensitive and exact match only

## Test Status

✅ **All Tests Passing (6/6):**
- `test_connect_to_mock_server` - Basic connectivity test
- `test_execute_select_one` - Query execution with result parsing
- `test_execute_multiple_queries` - Multiple query execution
- `test_connection_reuse` - Connection pooling behavior
- `test_custom_query_response` - Custom queries with multiple data types
- `test_query_with_nulls` - NULL value handling

## Known Issues

None currently! All basic functionality is working.

## Development

To run the mock server tests:

```bash
# Run all tests
cargo test --package mssql-tds --test test_mock_server

# Run a specific test
cargo test --package mssql-tds --test test_mock_server test_connect_to_mock_server -- --nocapture

# Run with single thread to see output sequentially
cargo test --package mssql-tds --test test_mock_server -- --test-threads=1 --nocapture
```

## Future Enhancements

- [ ] Support SSL/TLS encryption
- [ ] Add string data types (VARCHAR, NVARCHAR, TEXT)
- [ ] Add date/time data types (DATETIME, DATE, TIME)
- [ ] Add decimal/numeric types
- [ ] Add transaction support
- [ ] Support parameterized queries
- [ ] Add stored procedure execution
- [ ] Implement result set cursor navigation
- [ ] Add error injection for testing error handling
- [ ] Support multiple concurrent connections
- [ ] Pattern-based query matching (regex or wildcards)

## License

Copyright (c) Microsoft Corporation.
Licensed under the MIT License.
