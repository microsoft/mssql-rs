// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { create_connection } from '../dist/index.js';
import { decodeRawResult } from '../dist/decode.js';
import fs from 'fs';

export async function openConnection(context) {
  const connection = await create_connection(context);
  if (!connection) {
    throw new Error('Failed to create connection');
  }
  return connection;
}

export async function createContext() {
  const port = Number(process.env.DB_PORT ?? 1433);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error('DB_PORT must be an integer between 1 and 65535');
  }

  const context = {
    serverName: process.env.DB_HOST || 'localhost',
    port,
    userName: process.env.DB_USER || 'sa',
    password: await getPassword(),
    database: 'master',
    trustServerCertificate: true,
  };
  return context;
}

export async function getPassword() {
  let password = process.env.SQL_PASSWORD;
  if (!password) {
    try {
      password = await fs.promises.readFile('/tmp/password', 'utf8');
      password = password.trim();
    } catch (_err) {
      throw new Error(
        `SQL_PASSWORD environment variable not set and /tmp/password file not found: ${_err}`,
      );
    }
  }
  return password;
}

export async function nextRow(connection) {
  const chunk = await connection.fetchChunk(256 * 1024);
  if (!chunk) {
    return [];
  }
  const decoded = decodeRawResult(chunk.data);
  if (decoded.rowCount === 0) {
    return [];
  }
  const row = decoded.rows[0];
  return decoded.columns.map((col, i) => ({
    metadata: { name: col.name, dataType: col.typeId },
    rowVal: row[i],
  }));
}

export async function countAllRows(connection) {
  let total = 0;
  while (true) {
    const chunk = await connection.fetchChunk(256 * 1024);
    if (!chunk) break;
    const decoded = decodeRawResult(chunk.data);
    total += decoded.rowCount;
    if (!chunk.hasMore) {
      if (!(await connection.nextResultSet())) break;
    }
  }
  return total;
}
