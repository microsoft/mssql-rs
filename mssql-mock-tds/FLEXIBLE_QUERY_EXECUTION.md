# Flexible Query Execution Implementation

## Overview

The mock TDS server has been refactored to support flexible query execution, allowing users to define custom queries with their expected column definitions and row data. This replaces the previous hardcoded approach where only `SELECT 1` was supported.

## Architecture

### Data Structures (`query_response.rs`)

#### `SqlDataType`
Enum representing supported SQL data types:
- `TinyInt` - 1 byte integer (0-255)
- `SmallInt` - 2 byte signed integer
- `Int` - 4 byte signed integer
- `BigInt` - 8 byte signed integer

Each type knows its TDS type code (0x26 for IntN) and max length.

#### `ColumnValue`
Enum representing actual values that can be in a result set:
- `TinyInt(u8)`
- `SmallInt(i16)`
- `Int(i32)`
- `BigInt(i64)`
- `Null`

Each variant can serialize itself to TDS format with proper length indicators.

#### `ColumnDefinition`
Defines a column in a result set:
```rust
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: SqlDataType,
}
```

#### `Row`
Represents a single row of data:
```rust
pub struct Row {
    pub values: Vec<ColumnValue>,
}
```

#### `QueryResponse`
Complete result set definition:
```rust
pub struct QueryResponse {
    pub columns: Vec<ColumnDefinition>,
    pub rows: Vec<Row>,
}
```

Includes helper methods like `select_one()` and `select_multiple_types()`.

#### `QueryRegistry`
Manages query-to-response mappings:
```rust
pub struct QueryRegistry {
    responses: HashMap<String, QueryResponse>,
}
```

- Queries are normalized to uppercase for case-insensitive matching
- Default queries (`SELECT 1`, `SELECT CAST(1 AS BIGINT), 2, 3`) pre-registered
- Thread-safe via `Arc<Mutex<QueryRegistry>>`

### Protocol Updates (`protocol.rs`)

#### New Function: `build_query_result()`
Generic function that builds TDS packets from a `QueryResponse`:

1. Creates ColMetadata token with column count and definitions
2. For each column:
   - UserType (0)
   - Flags (0x0000)
   - Type code from `SqlDataType`
   - Max length from `SqlDataType`
   - Column name (UTF-16LE encoded)
3. For each row:
   - Row token (0xD1)
   - Each value serialized via `ColumnValue::write_to_buffer()`
4. DONE token with row count
5. Wrapped in TDS packet with TabularResult type

#### Removed Function: `build_select_one_result()`
No longer needed - replaced by generic `build_query_result()`.

### Server Updates (`server.rs`)

#### `MockTdsServer` struct
Now includes:
```rust
pub struct MockTdsServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    query_registry: Arc<Mutex<QueryRegistry>>,  // NEW
}
```

#### New Method: `query_registry()`
Returns a clone of the registry Arc for external registration:
```rust
pub fn query_registry(&self) -> Arc<Mutex<QueryRegistry>>
```

#### SQL Batch Handling
Updated to use registry:
1. Parse SQL from packet
2. Lock registry and lookup query
3. If found: call `build_query_result()` with response
4. If not found and starts with SELECT: return empty result set
5. Otherwise: return DONE token

## Usage Examples

### Basic Usage (Default Queries)
```rust
let server = MockTdsServer::new("127.0.0.1:0").await?;
// SELECT 1 already registered by default
```

### Custom Query Registration
```rust
let server = MockTdsServer::new("127.0.0.1:0").await?;
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
```

### Multiple Rows
```rust
reg.register(
    "SELECT * FROM users",
    QueryResponse::new(
        vec![
            ColumnDefinition::new("id", SqlDataType::Int),
            ColumnDefinition::new("age", SqlDataType::SmallInt),
        ],
        vec![
            Row::new(vec![ColumnValue::Int(1), ColumnValue::SmallInt(25)]),
            Row::new(vec![ColumnValue::Int(2), ColumnValue::SmallInt(30)]),
            Row::new(vec![ColumnValue::Int(3), ColumnValue::SmallInt(35)]),
        ],
    ),
);
```

### NULL Values
```rust
reg.register(
    "SELECT 1, NULL, 3",
    QueryResponse::new(
        vec![
            ColumnDefinition::new("", SqlDataType::Int),
            ColumnDefinition::new("", SqlDataType::Int),
            ColumnDefinition::new("", SqlDataType::Int),
        ],
        vec![Row::new(vec![
            ColumnValue::Int(1),
            ColumnValue::Null,
            ColumnValue::Int(3),
        ])],
    ),
);
```

## Testing

Six tests verify the implementation:

1. **test_connect_to_mock_server** - Basic connectivity
2. **test_execute_select_one** - Default query (SELECT 1)
3. **test_execute_multiple_queries** - Sequential queries
4. **test_connection_reuse** - Multiple connections
5. **test_custom_query_response** - Custom query with BigInt, Int, Int
6. **test_query_with_nulls** - NULL value handling

All tests passing ✅

## Benefits

1. **Flexibility**: Tests can define any query response needed
2. **Type Safety**: Rust enums ensure valid data types
3. **Maintainability**: No hardcoded responses scattered through code
4. **Extensibility**: Easy to add new data types by extending enums
5. **Testability**: Each test can have isolated query responses

## Future Enhancements

1. **String Types**: VARCHAR, NVARCHAR with proper length handling
2. **DateTime Types**: DATETIME, DATE, TIME with proper encoding
3. **Decimal Types**: DECIMAL, NUMERIC with precision/scale
4. **Pattern Matching**: Regex or wildcard-based query matching
5. **Multiple Result Sets**: Support for queries returning multiple result sets
6. **Dynamic Responses**: Callbacks or closures for computed responses

## Implementation Notes

### TDS Type Codes
All integer types use IntN (0x26) with varying lengths:
- TinyInt: length = 1
- SmallInt: length = 2
- Int: length = 4
- BigInt: length = 8

### NULL Encoding
For IntN types, NULL is encoded as a length byte of 0 with no data bytes.

### Query Normalization
Queries are converted to uppercase before lookup to enable case-insensitive matching:
```rust
let query = query.into().to_uppercase();
self.responses.insert(query, response);
```

### Thread Safety
`QueryRegistry` is wrapped in `Arc<Mutex<>>` to allow:
- Sharing across async tasks (Arc)
- Mutable access for registration (Mutex)
- Safe concurrent query lookups

## Files Modified

1. **Created**: `mssql-mock-tds/src/query_response.rs` (new module)
2. **Modified**: `mssql-mock-tds/src/lib.rs` (export new module)
3. **Modified**: `mssql-mock-tds/src/protocol.rs` (added `build_query_result`, removed `build_select_one_result`)
4. **Modified**: `mssql-mock-tds/src/server.rs` (added registry, updated query handling)
5. **Modified**: `mssql-tds/tests/test_mock_server.rs` (added 2 new tests)
6. **Modified**: `mssql-mock-tds/README.md` (updated documentation)

## Summary

The refactoring successfully transformed the mock TDS server from a fixed-response system to a flexible, configurable query execution engine. Users can now:
- Register any number of custom queries
- Define result sets with multiple columns and rows
- Use various integer data types
- Handle NULL values properly
- Test complex scenarios without modifying server code

All existing tests continue to pass, and new tests demonstrate the enhanced capabilities.
