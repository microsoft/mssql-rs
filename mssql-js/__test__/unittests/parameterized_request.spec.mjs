// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import test from 'ava';
import { Request, SqlJsConnection } from '../../dist/index.js';
import { TYPES } from '../../dist/datatypes/types.js';
import { decodeProjectedResult, decodeRawResult } from '../../dist/decode.js';

const TDS_INTN = 0x26;

function intResult(values, type = TDS_INTN) {
  const header = Buffer.alloc(19);
  header.writeUInt32LE(0x4d535351, 0);
  header.writeUInt8(1, 4);
  header.writeUInt16LE(1, 5);
  header.writeUInt32LE(values.length, 7);
  header.writeUInt32LE(24, 11);

  const column = Buffer.alloc(5);
  column.writeUInt8(type, 4);

  const name = Buffer.from('value');
  const stringTable = Buffer.alloc(12);
  stringTable.writeUInt32LE(1, 0);
  stringTable.writeUInt32LE(name.length, 8);

  const rows = Buffer.alloc(values.length * 5);
  values.forEach((value, index) => {
    rows.writeUInt8(4, index * 5);
    rows.writeInt32LE(value, index * 5 + 1);
  });
  return Buffer.concat([header, column, stringTable, name, rows]);
}

function fakeConnection(buffers = []) {
  const calls = [];
  const connection = {
    getEncoding: () => 'utf-8',
    async queryRaw(query, params) {
      calls.push({ query, params });
      return buffers;
    },
  };
  for (const method of [
    'execute',
    'executeWithParams',
    'fetchChunk',
    'nextResultSet',
    'closeQuery',
  ]) {
    connection[method] = () => {
      throw new Error(`Unexpected ${method} call`);
    };
  }
  return { connection, calls };
}

function recordSet(values, type = TDS_INTN) {
  return Object.assign(
    values.map((value) => ({ value })),
    {
      columns: [{ index: 0, name: 'value', type }],
      rowCount: values.length,
    },
  );
}

test('raw decoding keeps independent value arrays without a projector', (t) => {
  const result = decodeRawResult(intResult([42, 43]));
  t.deepEqual(result.rows, [[42], [43]]);
  t.not(result.rows[0], result.rows[1]);
});

test('projected decoding creates the column layout once and copies borrowed values', (t) => {
  let layouts = 0;
  let scratch;
  const result = decodeProjectedResult(intResult([42, 43]), (columns) => {
    layouts++;
    t.is(columns[0].name, 'value');
    return (values) => {
      if (scratch) t.is(values, scratch);
      scratch = values;
      return { value: values[0] };
    };
  });
  t.is(layouts, 1);
  t.deepEqual(result.rows, [{ value: 42 }, { value: 43 }]);
  t.not(result.rows[0], result.rows[1]);
});

test('projected decoding propagates projector failures', (t) => {
  const failure = new Error('Projection failed');
  const error = t.throws(() =>
    decodeProjectedResult(intResult([42]), () => () => {
      throw failure;
    }),
  );
  t.is(error, failure);
});

test('parameterized Request.query uses one queryRaw call with transformed parameters', async (t) => {
  const { connection, calls } = fakeConnection([intResult([42])]);
  const request = new Request(connection);
  const label = "O'Brien \u2014 \u6771\u4eac";
  request.input('value', TYPES.Int, '42');
  request.input('@nullable', TYPES.Int, null);
  request.input('label', TYPES.NVarChar(40), label);

  const query =
    'SELECT @value AS value WHERE @nullable IS NULL AND @label IS NOT NULL';
  const result = await request.query(query);

  t.deepEqual(calls, [
    {
      query,
      params: [
        {
          name: '@value',
          dataType: TYPES.Int.sqlType,
          value: 42,
          direction: 0,
          length: undefined,
        },
        {
          name: '@nullable',
          dataType: TYPES.Int.sqlType,
          value: null,
          direction: 0,
          length: undefined,
        },
        {
          name: '@label',
          dataType: TYPES.NVarChar(40).sqlType,
          value: Buffer.from(label, 'utf16le'),
          direction: 0,
          length: 40,
        },
      ],
    },
  ]);
  const expected = recordSet([42]);
  t.deepEqual(result, {
    IRecordSets: [expected],
    IRecordSet: expected,
    rowCount: 1,
    output: {},
  });
  t.is(result.IRecordSet, result.IRecordSets[0]);
});

test('non-parameterized Request.query still uses one queryRaw call', async (t) => {
  const { connection, calls } = fakeConnection([
    intResult([7], TYPES.Int.sqlType),
  ]);
  const result = await new Request(connection).query('SELECT 7 AS value');

  t.is(calls.length, 1);
  t.is(calls[0].query, 'SELECT 7 AS value');
  t.deepEqual(calls[0].params ?? [], []);
  const expected = recordSet([7], TYPES.Int.sqlType);
  t.deepEqual(result, {
    IRecordSets: [expected],
    IRecordSet: expected,
    rowCount: 1,
    output: {},
  });
  t.is(result.IRecordSet, result.IRecordSets[0]);
});

test('parameterized Request.query reshapes all buffered results, including empty sets', async (t) => {
  const { connection, calls } = fakeConnection([
    intResult([42, 43]),
    intResult([]),
    intResult([44]),
  ]);
  const request = new Request(connection);
  request.input('value', TYPES.Int, 42);

  const result = await request.query(
    'SELECT @value AS value UNION ALL SELECT @value + 1; ' +
      'SELECT @value AS value WHERE 1 = 0; SELECT @value + 2 AS value',
  );

  t.is(calls.length, 1);
  const expected = [recordSet([42, 43]), recordSet([]), recordSet([44])];
  t.deepEqual(result, {
    IRecordSets: expected,
    IRecordSet: expected[0],
    rowCount: 3,
    output: {},
  });
  t.is(result.IRecordSet, result.IRecordSets[0]);
});

test('parameterized Request.query returns no recordset for an empty buffer list', async (t) => {
  const { connection, calls } = fakeConnection();
  const request = new Request(connection);
  request.input('value', TYPES.Int, 42);

  const result = await request.query(
    'DECLARE @rows TABLE (value int); INSERT INTO @rows VALUES (@value)',
  );

  t.is(calls.length, 1);
  t.deepEqual(result, {
    IRecordSets: [],
    IRecordSet: null,
    rowCount: 0,
    output: {},
  });
});

test('parameterized Request.query propagates the queryRaw error unchanged', async (t) => {
  const failure = new Error('SQL execution failed');
  const { connection, calls } = fakeConnection();
  connection.queryRaw = async (query, params) => {
    calls.push({ query, params });
    throw failure;
  };
  const request = new Request(connection);
  request.input('value', TYPES.Int, 42);

  const error = await t.throwsAsync(() => request.query('SELECT @value'));

  t.is(error, failure);
  t.is(calls.length, 1);
});

test('SqlJsConnection.queryRaw forwards parameters to the native connection', async (t) => {
  const buffers = [intResult([42])];
  const { connection: nativeConnection, calls } = fakeConnection(buffers);
  const connection = new SqlJsConnection(nativeConnection);
  const params = [
    {
      name: '@value',
      dataType: TYPES.Int.sqlType,
      value: 42,
      direction: 0,
    },
  ];

  const result = await connection.queryRaw('SELECT @value AS value', params);

  t.deepEqual(calls, [{ query: 'SELECT @value AS value', params }]);
  t.is(calls[0].params, params);
  t.is(result, buffers);
});

for (const [name, collation, expected] of [
  [
    'UTF-8 collation',
    { isUtf8: true, sortId: 52, lcidLanguageId: 0x0411 },
    'utf-8',
  ],
  [
    'sort ID collation',
    { isUtf8: false, sortId: 52, lcidLanguageId: 0x0411 },
    'CP1252',
  ],
  [
    'language ID collation',
    { isUtf8: false, sortId: 0, lcidLanguageId: 0x0411 },
    'CP932',
  ],
  ['null collation fallback', null, 'utf-8'],
]) {
  test(`SqlJsConnection.getEncoding lazily caches ${name}`, (t) => {
    let calls = 0;
    const connection = new SqlJsConnection({
      getCollation() {
        calls++;
        return collation;
      },
    });

    t.is(calls, 0);
    t.is(connection.getEncoding(), expected);
    t.is(connection.getEncoding(), expected);
    t.is(connection.getEncoding(), expected);
    t.is(calls, 1);
  });
}

test('SqlJsConnection.getEncoding propagates native errors without caching a fallback', (t) => {
  const failure = new Error('Native collation lookup failed');
  let calls = 0;
  const connection = new SqlJsConnection({
    getCollation() {
      calls++;
      throw failure;
    },
  });

  t.is(calls, 0);
  t.is(
    t.throws(() => connection.getEncoding()),
    failure,
  );
  t.is(
    t.throws(() => connection.getEncoding()),
    failure,
  );
  t.is(calls, 2);
});
