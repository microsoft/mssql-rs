# mssql-js

Experimental Node.js bindings for the
[mssql-tds](https://github.com/microsoft/mssql-rs/tree/main/mssql-tds) TDS protocol
client, using NAPI-RS and a TypeScript wrapper. The API and packaging
are under development; the instructions below use a local checkout, not a
supported npm release.

## Build from source

Install the repository's pinned Rust toolchain, Node.js, Corepack, and a native
compiler/linker (MSVC Build Tools on Windows). The package pins Yarn 4.9.2;
CI currently builds with Node.js 20. Linux also needs OpenSSL development
libraries and `pkg-config`.

On x86_64 macOS, set `MACOSX_DEPLOYMENT_TARGET=10.15` or later before
`yarn build` to meet the default TDS Network.framework requirement:

```sh
export MACOSX_DEPLOYMENT_TARGET=10.15
```

From `mssql-js`:

```sh
corepack enable
yarn install
yarn build
yarn buildapi
```

`yarn build` compiles the native addon; `yarn buildapi` compiles the TypeScript
wrapper into `dist`.

## Query a database

Set `SQL_PASSWORD` for a SQL Server login. Save this as `example.mjs` in
`mssql-js` after building, then run `node example.mjs`:

```js
import { create_connection, Request } from './dist/index.js';

const password = process.env.SQL_PASSWORD;
if (!password) throw new Error('Set SQL_PASSWORD');

const connection = await create_connection({
  serverName: 'localhost',
  port: 1433,
  userName: 'sa',
  password,
  database: 'master',
  trustServerCertificate: false,
});

try {
  const result = await new Request(connection).query('SELECT 1 AS value');
  console.log(result.IRecordSet);
} finally {
  await connection.close();
}
```

Connections require encryption. Use a server certificate trusted by the host;
set `trustServerCertificate: true` only for local testing with an untrusted
certificate.

## Development

- `yarn test` compiles TypeScript and runs the tests. Database tests use
  `SQL_PASSWORD`, `DB_HOST` (default `localhost`), and `DB_USER` (default `sa`),
  with port 1433 and database `master`.
- `yarn lint` and `yarn format:check` check JS/TypeScript style.
- [Tracing configuration](https://github.com/microsoft/mssql-rs/blob/main/mssql-js/TRACING.md)
  documents diagnostic output and sensitive-data precautions.
- [Microbenchmark](https://github.com/microsoft/mssql-rs/blob/main/mssql-js/perf/README.md)
  measures a connection/query/close cycle.

## License

Licensed under the [MIT License](https://github.com/microsoft/mssql-rs/blob/main/LICENSE).
