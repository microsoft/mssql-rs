# mssql-mock-tds Python bindings

Testing-only Python bindings for the
[mock TDS server](https://github.com/microsoft/mssql-rs/blob/main/mssql-mock-tds/README.md).
The server accepts SQL-password and FedAuth login flows without validating
credentials. It is not a database or authentication service.

The distribution name is `mssql-mock-tds`; the import name is `mssql_mock_tds`.
Wheel publishing is currently configured in the repository's sandbox pipeline,
not a public PyPI release workflow.

## Build from source

Requires Python 3.9 or later and the repository's Rust toolchain. Non-Windows
builds also need OpenSSL development libraries. From a repository checkout,
with a Python virtual environment activated:

```sh
cd mssql-mock-tds-py
python -m pip install .
```

## Use in tests

```python
from mssql_mock_tds import PyMockTdsServer

with PyMockTdsServer(port=0) as server:
    print(server.sql_address)
```

The constructor binds to loopback; port `0` selects an available port. Entering
the context starts the server, and leaving it stops the listener. Put client
calls and assertions inside the context, and close clients before leaving it.
`server.sql_address` is `host,port`; `server.address` is `host:port`.

Use `get_connections()`, `get_last_access_token()`, or
`has_received_token(token)` to inspect received connections and FedAuth tokens.
Keep token values out of logs. For explicit lifecycle management, call
`start()` and `stop()`; an instance cannot be restarted after it has started.

These bindings expose connection inspection, not the Rust query-registration
API. Unregistered queries receive an empty DONE response; the default registered
queries include `SELECT 1`. See the Rust README for protocol limitations.

## TLS

`PyMockTdsServer(tls=True)` uses TDS 7.4 TLS and searches for the repository's
test certificates relative to the working directory. It does not accept a
certificate path. Generate the
[test certificates](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/tests/test_certificates/README.md)
and run from the repository root, `mssql-tds`, or a sibling crate directory.
Certificates are not bundled in the wheel.

## License

MIT
