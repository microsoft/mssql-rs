// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import test from 'ava';
import { createContext, openConnection } from '../../db.mjs';
import { Request } from '../../../dist/index.js';
import { TYPES } from '../../../dist/datatypes/types.js';

const TDS_INTN = 0x26;

function recordSet(rows, columns) {
  return Object.assign(rows, { columns, rowCount: rows.length });
}

function assertResult(t, actual, recordSets) {
  t.deepEqual(actual, {
    IRecordSets: recordSets,
    IRecordSet: recordSets[0] ?? null,
    rowCount: recordSets.reduce((total, rows) => total + rows.length, 0),
    output: {},
  });
  t.is(actual.IRecordSet, actual.IRecordSets[0] ?? null);
}

test('parameterized query preserves scalar, null, and Unicode values', async (t) => {
  const connection = await openConnection(await createContext());
  try {
    const label = "O'Brien \u2014 \u6771\u4eac";
    const request = new Request(connection);
    request.input('value', TYPES.Int, 42);
    request.input('@nullable', TYPES.Int, null);
    request.input('label', TYPES.NVarChar(40), label);

    const result = await request.query(
      'SELECT @value AS value, @nullable AS nullable_value, @label AS label',
    );

    assertResult(t, result, [
      recordSet(
        [{ value: 42, nullable_value: null, label }],
        [
          { index: 0, name: 'value', type: TDS_INTN },
          { index: 1, name: 'nullable_value', type: TDS_INTN },
          { index: 2, name: 'label', type: TYPES.NVarChar(40).sqlType },
        ],
      ),
    ]);
  } finally {
    await connection.close();
  }
});

test('parameterized empty result set preserves column metadata', async (t) => {
  const connection = await openConnection(await createContext());
  try {
    const request = new Request(connection);
    request.input('value', TYPES.Int, 42);

    const result = await request.query(`
      SELECT @value AS value, CAST(@value AS nvarchar(16)) AS label
      WHERE 1 = 0
    `);

    assertResult(t, result, [
      recordSet(
        [],
        [
          { index: 0, name: 'value', type: TDS_INTN },
          { index: 1, name: 'label', type: TYPES.NVarChar(16).sqlType },
        ],
      ),
    ]);
  } finally {
    await connection.close();
  }
});

test('parameterized query collects rows after leading DML and no-row statements', async (t) => {
  const connection = await openConnection(await createContext());
  try {
    const request = new Request(connection);
    request.input('value', TYPES.Int, 42);

    const result = await request.query(`
      SET NOCOUNT OFF;
      DECLARE @rows TABLE (value int);
      INSERT INTO @rows (value) VALUES (@value), (@value + 1);
      UPDATE @rows SET value = value + 1;
      SELECT value FROM @rows ORDER BY value;
      DELETE FROM @rows;
    `);

    assertResult(t, result, [
      recordSet(
        [{ value: 43 }, { value: 44 }],
        [{ index: 0, name: 'value', type: TDS_INTN }],
      ),
    ]);
  } finally {
    await connection.close();
  }
});

test('parameterized multiple results preserve empty sets and anonymous columns', async (t) => {
  const connection = await openConnection(await createContext());
  try {
    const request = new Request(connection);
    request.input('value', TYPES.Int, 42);

    const result = await request.query(`
      SET NOCOUNT ON;
      SELECT @value AS first_empty WHERE 1 = 0;
      SELECT @value AS named, @value + 1, CAST(NULL AS int), @value + 2;
      SELECT @value AS middle_empty WHERE 1 = 0;
      SELECT @value + 3;
      SELECT @value AS last_empty WHERE 1 = 0;
    `);

    assertResult(t, result, [
      recordSet([], [{ index: 0, name: 'first_empty', type: TDS_INTN }]),
      recordSet(
        [{ named: 42, '': [43, null, 44] }],
        [
          { index: 0, name: 'named', type: TDS_INTN },
          { index: 1, name: '', type: undefined },
        ],
      ),
      recordSet([], [{ index: 0, name: 'middle_empty', type: TDS_INTN }]),
      recordSet([{ '': 45 }], [{ index: 0, name: '', type: undefined }]),
      recordSet([], [{ index: 0, name: 'last_empty', type: TDS_INTN }]),
    ]);
  } finally {
    await connection.close();
  }
});

test('parameterized DML-only query returns no recordset and allows connection reuse', async (t) => {
  const connection = await openConnection(await createContext());
  try {
    await new Request(connection).query(
      'CREATE TABLE #ParameterizedRows (value int)',
    );
    const request = new Request(connection);
    request.input('value', TYPES.Int, 42);

    const result = await request.query(`
      SET NOCOUNT OFF;
      INSERT INTO #ParameterizedRows (value) VALUES (@value);
      UPDATE #ParameterizedRows SET value = @value + 1;
    `);
    assertResult(t, result, []);

    const nextResult = await request.query(
      'SELECT value FROM #ParameterizedRows WHERE value = @value + 1',
    );
    assertResult(t, nextResult, [
      recordSet([{ value: 43 }], [{ index: 0, name: 'value', type: TDS_INTN }]),
    ]);
  } finally {
    await connection.close();
  }
});

test('parameterized query buffers more than 256 KiB and collects subsequent results', async (t) => {
  const connection = await openConnection(await createContext());
  try {
    const count = 100_000;
    const request = new Request(connection);
    request.input('count', TYPES.Int, count);

    const result = await request.query(`
      SELECT value FROM GENERATE_SERIES(1, @count) ORDER BY value;
      SELECT @count AS total;
    `);

    t.deepEqual(Object.keys(result).sort(), [
      'IRecordSet',
      'IRecordSets',
      'output',
      'rowCount',
    ]);
    t.true(Array.isArray(result.IRecordSets));
    t.is(result.IRecordSets.length, 2);
    t.is(result.rowCount, count + 1);
    t.deepEqual(result.output, {});

    const [rows, totals] = result.IRecordSets;
    t.true(result.IRecordSet === rows);
    t.true(Array.isArray(rows));
    t.is(rows.length, count);
    t.is(rows.rowCount, count);
    t.deepEqual(rows.columns, [{ index: 0, name: 'value', type: TDS_INTN }]);
    const mismatch = rows.findIndex(
      (row, index) => row.value !== index + 1 || Object.keys(row).length !== 1,
    );
    t.is(mismatch, -1, 'Every generated row must contain its ordered integer');
    t.deepEqual(
      totals,
      recordSet(
        [{ total: count }],
        [{ index: 0, name: 'total', type: TDS_INTN }],
      ),
    );
  } finally {
    await connection.close();
  }
});

for (const [position, query, message] of [
  [
    'before rows',
    `
      THROW 51000, N'parameterized error before rows', 1;
      SELECT @value AS value;
    `,
    /parameterized error before rows/,
  ],
  [
    'after rows',
    `
      SELECT @value AS value;
      THROW 51001, N'parameterized error after rows', 1;
    `,
    /parameterized error after rows/,
  ],
]) {
  test(`parameterized SQL error ${position} propagates and allows connection reuse`, async (t) => {
    const connection = await openConnection(await createContext());
    try {
      const request = new Request(connection);
      request.input('value', TYPES.Int, 42);

      await t.throwsAsync(() => request.query(query), { message });

      const nextResult = await request.query('SELECT @value AS value');
      assertResult(t, nextResult, [
        recordSet(
          [{ value: 42 }],
          [{ index: 0, name: 'value', type: TDS_INTN }],
        ),
      ]);

      const unparameterizedResult = await new Request(connection).query(
        'SELECT 7 AS value',
      );
      assertResult(t, unparameterizedResult, [
        recordSet(
          [{ value: 7 }],
          [{ index: 0, name: 'value', type: TYPES.Int.sqlType }],
        ),
      ]);
    } finally {
      await connection.close();
    }
  });
}
