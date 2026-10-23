// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import assert from 'node:assert/strict';
import { execFileSync, fork } from 'node:child_process';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import os from 'node:os';
import { dirname, resolve } from 'node:path';
import { performance } from 'node:perf_hooks';
import { fileURLToPath, pathToFileURL } from 'node:url';

const filename = fileURLToPath(import.meta.url);
const directory = dirname(filename);
const database = 'mssql_js_benchmark';
const drivers = ['mssql-js', 'tedious'];
const nativeDirectories = {
  'mssql-js': resolve(
    process.env.BENCH_CANDIDATE_PATH || `${directory}/../dist`,
  ),
};
if (process.env.BENCH_BASELINE_PATH) {
  nativeDirectories['mssql-js-baseline'] = resolve(
    process.env.BENCH_BASELINE_PATH,
  );
  drivers.push('mssql-js-baseline');
}
const settings = {
  host: process.env.DB_HOST || '127.0.0.1',
  port: positiveNumber('DB_PORT', 15433),
  rounds: positiveNumber('BENCH_ROUNDS', 6),
  warmupMs: positiveNumber('BENCH_WARMUP_MS', 1000),
  sampleMs: positiveNumber('BENCH_SAMPLE_MS', 3000),
};
const narrowSql = (count) =>
  `SELECT TOP (${count}) id, value FROM dbo.driver_bench_rows ORDER BY id OPTION (MAXDOP 1)`;
const scenarios = [
  { name: 'connect-select-close', sql: 'SELECT 1 AS value', connect: true },
  { name: 'select-one', sql: 'SELECT 1 AS value' },
  {
    name: 'parameterized-lookup',
    sql: 'SELECT id, value FROM dbo.driver_bench_rows WHERE id = @id',
    parameter: 4321,
  },
  {
    name: 'parameterized-rows-10000',
    sql: 'SELECT id, value FROM dbo.driver_bench_rows WHERE id <= @id ORDER BY id OPTION (MAXDOP 1)',
    parameter: 10000,
    rows: 10000,
  },
  {
    name: 'parameterized-rows-100000',
    sql: 'SELECT value AS id, value * 2 AS value FROM GENERATE_SERIES(1, @id) ORDER BY value OPTION (MAXDOP 1)',
    parameter: 100000,
    rows: 100000,
  },
  {
    name: 'parameterized-strings-1000',
    sql: 'SELECT id, label, body FROM dbo.driver_bench_rows WHERE id <= @id ORDER BY id OPTION (MAXDOP 1)',
    parameter: 1000,
    rows: 1000,
  },
  {
    name: 'parameterized-lookup-concurrency-8',
    sql: 'SELECT id, value FROM dbo.driver_bench_rows WHERE id = @id',
    parameter: 4321,
    concurrency: 8,
  },
  { name: 'rows-1000', sql: narrowSql(1000), rows: 1000 },
  { name: 'rows-10000', sql: narrowSql(10000), rows: 10000 },
  {
    name: 'strings-1000',
    sql: 'SELECT TOP (1000) id, label, body FROM dbo.driver_bench_rows ORDER BY id OPTION (MAXDOP 1)',
    rows: 1000,
  },
  { name: 'binary-1mib', sql: 'SELECT payload FROM dbo.driver_bench_lob' },
  {
    name: 'select-one-concurrency-8',
    sql: 'SELECT 1 AS value',
    concurrency: 8,
  },
];
const sessionSql = `
SET NOCOUNT ON;
SET ANSI_NULLS ON;
SET ANSI_PADDING ON;
SET ANSI_WARNINGS ON;
SET ARITHABORT ON;
SET CONCAT_NULL_YIELDS_NULL ON;
SET QUOTED_IDENTIFIER ON;
SET NUMERIC_ROUNDABORT OFF;
SET TRANSACTION ISOLATION LEVEL READ COMMITTED;
SET TEXTSIZE 2147483647;
`;

function positiveNumber(name, fallback) {
  const value = Number(process.env[name] ?? fallback);
  assert(
    Number.isSafeInteger(value) && value > 0,
    `${name} must be a positive integer`,
  );
  return value;
}

async function adapter(driver) {
  if (Object.hasOwn(nativeDirectories, driver)) {
    const dist = nativeDirectories[driver];
    const { create_connection, Request } = await import(
      pathToFileURL(`${dist}/index.js`).href
    );
    const { IntType } = await import(
      pathToFileURL(`${dist}/datatypes/IntType.js`).href
    );
    return {
      connect: (db = database) =>
        create_connection({
          serverName: settings.host,
          port: settings.port,
          userName: 'sa',
          password: process.env.SQL_PASSWORD,
          database: db,
          trustServerCertificate: true,
        }),
      async query(connection, sql, parameter) {
        const request = new Request(connection);
        if (parameter !== undefined)
          request.input('id', new IntType(), parameter);
        const result = await request.query(sql);
        return result.IRecordSet ?? [];
      },
      close: (connection) => connection.close(),
    };
  }
  assert.equal(driver, 'tedious');
  const { Connection, Request, TYPES } = await import('tedious');
  return {
    async connect(db = database) {
      const connection = new Connection({
        server: settings.host,
        authentication: {
          type: 'default',
          options: { userName: 'sa', password: process.env.SQL_PASSWORD },
        },
        options: {
          port: settings.port,
          database: db,
          encrypt: true,
          trustServerCertificate: true,
          packetSize: 8000,
          tdsVersion: '7_4',
          connectTimeout: 30000,
          requestTimeout: 30000,
          useUTC: true,
        },
      });
      // Surface asynchronous connection failures rather than timing partial results.
      connection.on('error', (error) => {
        throw error;
      });
      await new Promise((resolve, reject) => {
        connection.connect((error) => (error ? reject(error) : resolve()));
      });
      return connection;
    },
    query(connection, sql, parameter) {
      return new Promise((resolve, reject) => {
        const rows = [];
        const request = new Request(sql, (error) =>
          error ? reject(error) : resolve(rows),
        );
        request.on('row', (columns) => {
          const row = {};
          for (const column of columns)
            row[column.metadata.colName] = column.value;
          rows.push(row);
        });
        if (parameter !== undefined) {
          request.addParameter('id', TYPES.Int, parameter);
          connection.execSql(request);
        } else {
          // Match the native driver's SQL batch path, not sp_executesql.
          connection.execSqlBatch(request);
        }
      });
    },
    async close(connection) {
      const ended = once(connection, 'end');
      connection.close();
      await ended;
    },
  };
}

function expectedRows(scenario) {
  if (scenario.name === 'binary-1mib') {
    return [{ payload: Buffer.alloc(1024 * 1024, 0x61) }];
  }
  if (!scenario.rows) {
    return scenario.parameter !== undefined
      ? [{ id: scenario.parameter, value: scenario.parameter * 2 }]
      : [{ value: 1 }];
  }
  return Array.from({ length: scenario.rows }, (_, index) => {
    const id = index + 1;
    return scenario.name.endsWith('strings-1000')
      ? { id, label: `row-${id}`, body: 'x'.repeat(512) }
      : { id, value: id * 2 };
  });
}

async function setup() {
  const api = await adapter('tedious');
  const connection = await api.connect('master');
  try {
    await api.query(
      connection,
      `IF DB_ID('${database}') IS NULL CREATE DATABASE ${database}`,
    );
    await api.query(
      connection,
      `USE ${database};
       ${sessionSql}
       DROP TABLE IF EXISTS dbo.driver_bench_rows;
       DROP TABLE IF EXISTS dbo.driver_bench_lob;
       CREATE TABLE dbo.driver_bench_rows (
         id int NOT NULL PRIMARY KEY, value int NOT NULL,
         label nvarchar(64) NOT NULL, body nvarchar(512) NOT NULL
       );
       INSERT dbo.driver_bench_rows
       SELECT value, value * 2, CONCAT(N'row-', value), REPLICATE(N'x', 512)
       FROM GENERATE_SERIES(1, 10000);
       CREATE TABLE dbo.driver_bench_lob (payload varbinary(max) NOT NULL);
       INSERT dbo.driver_bench_lob
       VALUES (CONVERT(varbinary(max), REPLICATE(CAST('a' AS varchar(max)), 1048576)));
       UPDATE STATISTICS dbo.driver_bench_rows WITH FULLSCAN;`,
    );
    return await api.query(
      connection,
      `SELECT @@VERSION AS version,
         CAST(SERVERPROPERTY('ProductVersion') AS varchar(32)) AS productVersion`,
    );
  } finally {
    await api.close(connection);
  }
}

function percentile(sorted, fraction) {
  return sorted[Math.max(0, Math.ceil(sorted.length * fraction) - 1)];
}

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2
    ? sorted[middle]
    : (sorted[middle - 1] + sorted[middle]) / 2;
}

async function worker(driver, scenario) {
  const api = await adapter(driver);
  const connections = [];
  try {
    for (let i = 0; i < (scenario.concurrency ?? 1); i++) {
      const connection = await api.connect();
      connections.push(connection);
      await api.query(connection, sessionSql);
      const rows = await api.query(
        connection,
        scenario.sql,
        scenario.parameter,
      );
      // Strip the native array's metadata properties; compare every cell before timing.
      assert.deepEqual([...rows], expectedRows(scenario));
    }
    const [session] = await api.query(
      connections[0],
      `SELECT encrypt_option, net_packet_size, protocol_version
       FROM sys.dm_exec_connections WHERE session_id = @@SPID`,
    );
    assert.equal(session.encrypt_option, 'TRUE');
    assert(session.net_packet_size > 0);

    const operation = async (connection) => {
      const active = scenario.connect ? await api.connect() : connection;
      try {
        const rows = await api.query(active, scenario.sql, scenario.parameter);
        assert.equal(rows.length, scenario.rows ?? 1);
        return rows.length;
      } finally {
        if (scenario.connect) await api.close(active);
      }
    };
    const runFor = async (duration, record) => {
      const latencies = [];
      const cpuStart = process.cpuUsage();
      const start = performance.now();
      let operations = 0;
      let rowCount = 0;
      await Promise.all(
        connections.map(async (connection) => {
          do {
            const before = performance.now();
            const rows = await operation(connection);
            rowCount += rows;
            if (record) latencies.push(performance.now() - before);
            operations++;
          } while (performance.now() - start < duration);
        }),
      );
      const elapsedMs = performance.now() - start;
      const cpu = process.cpuUsage(cpuStart);
      latencies.sort((a, b) => a - b);
      return {
        operations,
        rowCount,
        elapsedMs,
        opsPerSecond: (operations * 1000) / elapsedMs,
        p50Ms: record ? percentile(latencies, 0.5) : null,
        p95Ms: record ? percentile(latencies, 0.95) : null,
        clientCpuMsPerOp: (cpu.user + cpu.system) / 1000 / operations,
        maxRssMiB: process.resourceUsage().maxRSS / 1024,
      };
    };
    await runFor(settings.warmupMs, false);
    return {
      driver,
      scenario: scenario.name,
      session,
      ...(await runFor(settings.sampleMs, true)),
    };
  } finally {
    for (const connection of connections) await api.close(connection);
  }
}

async function runChild(driver, scenario) {
  const child = fork(filename, ['--worker', driver, scenario.name], {
    stdio: ['ignore', 'inherit', 'inherit', 'ipc'],
    timeout: Math.max(120000, (settings.warmupMs + settings.sampleMs) * 3),
    env: { ...process.env, MSSQLJS_TRACE: 'false' },
  });
  let result;
  child.on('message', (message) => {
    result = message;
  });
  const [code, signal] = await once(child, 'exit');
  assert.equal(code, 0, `${driver}/${scenario.name} failed (signal=${signal})`);
  assert(result, `${driver}/${scenario.name} returned no measurement`);
  return result;
}

async function nativeBuilds() {
  const builds = {};
  for (const [driver, dist] of Object.entries(nativeDirectories)) {
    const names = (await readdir(`${dist}/generated`))
      .filter((name) => name.endsWith('.node'))
      .map((name) => `generated/${name}`);
    assert(names.length > 0, `No native addon found in ${dist}/generated`);
    names.push('request.js', 'connection.js', 'decode.js');
    const hashes = {};
    for (const name of names) {
      hashes[name] = createHash('sha256')
        .update(await readFile(`${dist}/${name}`))
        .digest('hex');
    }
    builds[driver] = { dist, hashes };
  }
  return builds;
}

function pairedSpeedup(samples, scenario, comparator) {
  const ratios = Array.from({ length: settings.rounds }, (_, index) => {
    const pair = samples.filter(
      (sample) => sample.scenario === scenario && sample.round === index + 1,
    );
    return (
      pair.find((sample) => sample.driver === 'mssql-js').opsPerSecond /
      pair.find((sample) => sample.driver === comparator).opsPerSecond
    );
  });
  return {
    speedup: median(ratios),
    minSpeedup: Math.min(...ratios),
    maxSpeedup: Math.max(...ratios),
  };
}

async function main() {
  assert(
    Number(process.versions.node.split('.')[0]) >= 22,
    'Node.js >=22 is required',
  );
  assert(process.env.SQL_PASSWORD, 'SQL_PASSWORD must be set');
  if (process.argv[2] === '--worker') {
    const scenario = scenarios.find((item) => item.name === process.argv[4]);
    assert(scenario, 'Unknown workload');
    process.send(await worker(process.argv[3], scenario));
    process.disconnect();
    return;
  }
  const filter = process.env.BENCH_SCENARIOS?.split(',');
  const selected = filter
    ? filter.map((name) => {
        const scenario = scenarios.find((item) => item.name === name);
        assert(scenario, `Unknown workload: ${name}`);
        return scenario;
      })
    : scenarios;
  const output = resolve(
    process.env.BENCH_OUTPUT || `${directory}/results/comparison.json`,
  );
  const tediousPackage = JSON.parse(
    await readFile(`${directory}/node_modules/tedious/package.json`, 'utf8'),
  );
  const report = {
    startedAt: new Date().toISOString(),
    commit: execFileSync('git', ['rev-parse', 'HEAD'], {
      cwd: directory,
      encoding: 'utf8',
    }).trim(),
    node: process.version,
    tedious: tediousPackage.version,
    platform: `${os.platform()} ${os.release()} ${os.arch()}`,
    cpu: os.cpus()[0].model,
    availableParallelism: os.availableParallelism(),
    nativeBuilds: await nativeBuilds(),
    settings,
    scenarios: selected,
    sqlServer: await setup(),
    samples: [],
  };
  await mkdir(dirname(output), { recursive: true });
  for (let round = 0; round < settings.rounds; round++) {
    for (const [index, scenario] of selected.entries()) {
      const offset = (round + index) % drivers.length;
      let order = [...drivers.slice(offset), ...drivers.slice(0, offset)];
      if (
        drivers.length > 2 &&
        round % (drivers.length * 2) >= drivers.length
      ) {
        order = order.reverse();
      }
      for (const driver of order) {
        const sample = {
          round: round + 1,
          ...(await runChild(driver, scenario)),
        };
        const pairedSample = report.samples.find(
          (item) =>
            item.round === sample.round && item.scenario === sample.scenario,
        );
        if (pairedSample)
          assert.deepEqual(sample.session, pairedSample.session);
        assert.equal(sample.rowCount, sample.operations * (scenario.rows ?? 1));
        report.samples.push(sample);
        console.log(
          `${round + 1}/${settings.rounds} ${scenario.name} ${driver}: ${sample.opsPerSecond.toFixed(1)} ops/s, p95 ${sample.p95Ms.toFixed(3)} ms`,
        );
        await writeFile(output, `${JSON.stringify(report, null, 2)}\n`);
      }
    }
  }
  report.summary = selected.map((scenario) => {
    const summaries = Object.fromEntries(
      drivers.map((driver) => {
        const samples = report.samples.filter(
          (sample) =>
            sample.driver === driver && sample.scenario === scenario.name,
        );
        return [
          driver,
          {
            opsPerSecond: median(samples.map((sample) => sample.opsPerSecond)),
            p50Ms: median(samples.map((sample) => sample.p50Ms)),
            p95Ms: median(samples.map((sample) => sample.p95Ms)),
            clientCpuMsPerOp: median(
              samples.map((sample) => sample.clientCpuMsPerOp),
            ),
            maxRssMiB: median(samples.map((sample) => sample.maxRssMiB)),
          },
        ];
      }),
    );
    return {
      scenario: scenario.name,
      ...summaries,
      ...pairedSpeedup(report.samples, scenario.name, 'tedious'),
      ...(drivers.includes('mssql-js-baseline')
        ? {
            versusBaseline: pairedSpeedup(
              report.samples,
              scenario.name,
              'mssql-js-baseline',
            ),
          }
        : {}),
    };
  });
  report.completedAt = new Date().toISOString();
  await writeFile(output, `${JSON.stringify(report, null, 2)}\n`);
  console.table(
    report.summary.map((item) => ({
      workload: item.scenario,
      'mssql-js ops/s': item['mssql-js'].opsPerSecond.toFixed(1),
      'tedious ops/s': item.tedious.opsPerSecond.toFixed(1),
      'paired speedup': `${item.speedup.toFixed(2)}x`,
      range: `${item.minSpeedup.toFixed(2)}-${item.maxSpeedup.toFixed(2)}x`,
      ...(item.versusBaseline
        ? {
            'baseline ops/s': item['mssql-js-baseline'].opsPerSecond.toFixed(1),
            'vs baseline': `${item.versusBaseline.speedup.toFixed(2)}x`,
            'baseline range': `${item.versusBaseline.minSpeedup.toFixed(2)}-${item.versusBaseline.maxSpeedup.toFixed(2)}x`,
          }
        : {}),
    })),
  );
  console.log(`Raw results: ${output}`);
}

await main();
