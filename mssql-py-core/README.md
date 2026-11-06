<!--
Published verbatim as the mssql-python-rs PyPI project description (see
`readme` in pyproject.toml); it is republished on the next release. Keep the
content user-facing and every link absolute: repo-relative links do not
resolve on PyPI.
-->

# mssql-python-rs

Native runtime for the
[`mssql-python`](https://github.com/microsoft/mssql-python) SQL Server and
Azure SQL driver. Install and use `mssql-python` for the Python database API;
this package supplies its native components:

- **`mssql_py_core`** — a PyO3 extension that implements the SQL Server TDS
  protocol in Rust. It backs high-throughput paths (such as bulk copy) for the
  [`mssql-python`](https://github.com/microsoft/mssql-python) driver.
- **`mssqlodbc`** (alpha) — an ODBC driver for SQL Server, implemented in
  Rust. It exposes the native ODBC C API. It is under active development and
  validation for use as an opt-in backend for the `mssql-python` driver. It is
  not yet a complete general-purpose replacement for Microsoft ODBC Driver 18
  for SQL Server.

## Bulk-copy datetime strings

Bulk copy parses ISO datetime strings targeting `DATETIME2` and `DATETIMEOFFSET`
in Rust, preserving all seven fractional-second digits (100 ns precision).
Both space and `T` separators and a trailing `Z` are supported on every supported
Python version. Fractions longer than seven digits are rejected rather than
silently truncated. The destination column's scale is applied by the TDS
serializer, using the existing truncation behavior for lower scales.
Python-compatible fractional forms such as `12000012` and `12:00:00:12`
are retained; both represent `12:00:00.12`.

`DATETIME2` retains the local date and time when an offset is present.
`DATETIMEOFFSET` preserves the offset and normalizes its wire timestamp to UTC;
offsets must be whole minutes within `-14:00` through `+14:00`, and both the local
and UTC dates must be within years 1 through 9999. Offset fields are normalized
before validation, matching Python (for example, `+05:60` becomes `+06:00`).
Strings without an offset
continue to use UTC for `DATETIMEOFFSET`. Python `datetime` objects retain their
existing microsecond resolution.

## Platforms

Prebuilt wheels for CPython 3.10+:

- **Windows** — x64, and ARM64 (CPython 3.11+)
- **macOS 15+** — universal2 (x86_64 + arm64)
- **Linux** — glibc (`manylinux_2_28`, `manylinux_2_34`) and musl
  (`musllinux_1_2`), on x86_64 and aarch64

The Linux wheels require system OpenSSL: 1.1 for `manylinux_2_28`, or 3 for
`manylinux_2_34` and `musllinux_1_2` (`libssl3` on Alpine). Windows and macOS
do not require OpenSSL.

## Native connection settings

For SDK authors integrating the native runtime, see the
[connection settings reference](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/connection-behavior.md#connection-policy-and-migration).

## Native async authentication preview (AQE)

Async Query Execution (AQE) authentication is a native `mssql_py_core` preview,
subject to breaking changes without notice and not intended for production use.
The example below calls `PyAsyncConnection` directly; it does not imply that the
downstream `mssql-python` public async wrapper exposes these authentication options.
Use a build containing this preview, not an older published package.

Built-in credentials require the `azure-async` extra, which installs
`azure-identity>=1.25.3` and `aiohttp>=3.12`. For a local preview wheel, replace the
placeholder filename with the wheel for your platform:

```bash
python -m pip install "/path/to/mssql_python_rs-<version>-<python>-<abi>-<platform>.whl[azure-async]"
```

### Built-in credentials

Built-in acquisition uses `azure.identity.aio` on the caller's asyncio loop.
It supports **Azure Public Cloud only**, including the SQL scope
`https://database.windows.net/.default` and validated public FedAuth authorities.
Sovereign-cloud built-in acquisition is not supported.

| `authentication` | Async Azure Identity credential | Native context options |
| --- | --- | --- |
| `ActiveDirectoryDefault` | `DefaultAzureCredential` | Configure the Azure Identity credential chain; the driver requests the server's validated FedAuth tenant. |
| `ActiveDirectoryMSI` | `ManagedIdentityCredential` | Omit `user_name` for system-assigned identity; set it to the client ID for user-assigned identity. |
| `ActiveDirectoryServicePrincipal` | `ClientSecretCredential` | Set `user_name` to the client ID and `password` to the client secret; the tenant comes from FedAuth. |

**Interactive, Device Code, Entra Integrated, and Entra Username/Password
(deprecated) are not supported as built-in async modes in this preview.**
Synchronous `mssql-python` supports additional modes through synchronous Azure
Identity and/or native ODBC authentication; AQE does not yet provide full
authentication-mode parity.

The [Azure Identity async API](https://learn.microsoft.com/en-us/python/api/azure-identity/azure.identity.aio?view=azure-python)
does not expose counterparts to `InteractiveBrowserCredential`,
`DeviceCodeCredential`, or `UsernamePasswordCredential`, nor an equivalent
integrated-auth credential. The synchronous
[username/password credential is deprecated](https://learn.microsoft.com/en-us/python/api/azure-identity/azure.identity.usernamepasswordcredential?view=azure-python)
because it does not support MFA. These are limitations of the available SDK
surface and this preview, not a claim that the flows are inherently impossible
asynchronously. The driver does not wrap synchronous credentials in worker
threads as a fallback. Default can use an existing Azure CLI login, but that
does not constitute built-in Interactive or Device Code support.

These exclusions apply to built-in modes. Caller-owned custom providers may
implement other flows if they satisfy the async contract below.

Configure an Azure identity with access to the database, and set `SQL_SERVER` and
`SQL_DATABASE` before running this native-only example:

```python
import asyncio
import os

from mssql_py_core import PyAsyncConnection


async def main():
    context = {
        "server": os.environ["SQL_SERVER"],
        "database": os.environ["SQL_DATABASE"],
        "authentication": "ActiveDirectoryDefault",
        "encryption": "Mandatory",
        "trust_server_certificate": False,
        "connect_timeout": 30,
        "connect_retry_count": 0,
    }
    connection = await PyAsyncConnection.connect(context, autocommit=True)
    try:
        cursor = connection.cursor()
        try:
            await cursor.execute("SELECT 1", use_prepare=False)
            print(await cursor.fetchone())
        finally:
            await cursor.close()
    finally:
        await connection.close()


asyncio.run(main())
```

### Connect errors

Native async TDS connect failures use the same `mssql_py_core` DB-API exception
mapping for every authentication mode. Transport, TLS, and login-timeout failures
raise `OperationalError`; server login errors raise `DatabaseError` with
`sql_errors` and `info_messages` diagnostics. This replaces the previous
`RuntimeError` mapping for SqlPassword and raw access-token connect failures.
Async Entra cancellation during initial login still raises
`asyncio.CancelledError`. Connection lifecycle and ownership rejections retain
their existing behavior.

### Custom providers and cleanup

Pass a caller-owned credential using the keyword-only `token_provider` argument
to `PyAsyncConnection.connect(context, token_provider=credential)`. Remove
`authentication` from the example context, and do not supply non-empty
`user_name` or `password`, an access token, another token factory, or
`trusted_connection="Yes"`.

The provider must have a callable `get_token(scope)` returning an awaitable.
Its result must expose a non-empty string `token`, such as Azure Identity's
`AccessToken`. The driver passes one scope argument and no tenant keyword;
custom providers retain their own tenant policy. Custom providers do not require
the extra unless their implementation uses its dependencies.

- **Caller-owned:** The driver never closes a custom credential. Keep it usable
  while the connection may acquire tokens, and close it yourself after use.
- **Driver-owned:** A built-in credential is created for each acquisition attempt
  and closed before its token is delivered to TDS. There is no global async
  credential cache.
- **Cleanup:** Acquisition runs on the originating asyncio loop and shares the
  native login deadline. Driver-owned cleanup has a separate five-second limit;
  cleanup failure after successful acquisition fails login. Keep the loop running
  until credential tasks settle. Cancellation is cooperative; a credential that
  suppresses it may continue running after the caller stops waiting.

Provider cancellation during session recovery remains a known limitation: it may
be retried or surface as `OperationalError`. The example disables recovery withcovering the azure-async extra, supported built-in modes, custom token_provider contract, credential ownership, cleanup requirements, and Azure Public Cloud restriction.
`connect_retry_count=0`; preserving recovery-time cancellation requires a separate
TDS and py-core change.

## License

Licensed under the [MIT License](https://github.com/microsoft/mssql-rs/blob/main/LICENSE).
