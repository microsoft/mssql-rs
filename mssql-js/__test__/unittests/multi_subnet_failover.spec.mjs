// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import test from 'ava';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

test('MSF startup configuration is exported and freezes on first use', (t) => {
  const script = `
    import assert from 'node:assert/strict';
    import { create_connection, setDefaultMultiSubnetFailover } from './dist/index.js';
    assert.equal(typeof setDefaultMultiSubnetFailover, 'function');
    assert.throws(() => setDefaultMultiSubnetFailover('false'));
    const context = {
      serverName: 'tcp:localhost', port: 1433, userName: 'unused',
      password: 'unused', database: 'master', trustServerCertificate: true,
    };
    await assert.rejects(create_connection(context), /MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT/);
    assert.throws(() => setDefaultMultiSubnetFailover(false), /already frozen/);
  `;
  const result = spawnSync(
    process.execPath,
    ['--input-type=module', '-e', script],
    {
      cwd: fileURLToPath(new URL('../../', import.meta.url)),
      env: {
        ...process.env,
        MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT: 'invalid',
      },
      encoding: 'utf8',
      timeout: 10000,
    },
  );
  t.is(result.status, 0, result.stderr || result.error?.message);
});
