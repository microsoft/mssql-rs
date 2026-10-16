// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { codepageByLanguageId, codepageBySortId, Encoding } from './codepages';
import { tdsToJsTransformers } from './datatypes';
import { JsSqlDataTypes } from './datatypes/enums';
import {
  connect,
  Connection,
  ChunkResult,
  JsClientContext,
  Metadata,
  NapiDecimalParts,
  NapiF64,
  NapiIsolationLevel,
  NapiSqlDateTime,
  NapiSqlDateTime2,
  NapiSqlDateTimeOffset,
  NapiSqlMoney,
  NapiSqlTime,
  OutputParams,
  Parameter,
} from './generated';
import { IsolationLevel } from './transactions';

export async function create_connection(
  context: JsClientContext,
): Promise<SqlJsConnection> {
  const internal_connection = await connect(context);
  return new SqlJsConnection(internal_connection);
}

export class SqlJsConnection {
  private encoding?: Encoding;

  constructor(private internal_connection: Connection) {
    this.internal_connection = internal_connection;
  }

  /**
   * Begins a transaction with an optional isolation level and name.
   */
  async beginTransaction(
    isolationLevel: IsolationLevel = IsolationLevel.ReadCommitted,
    name?: string,
  ): Promise<void> {
    return this.internal_connection.beginTransaction(
      isolationLevel as unknown as NapiIsolationLevel,
      name,
    );
  }

  /**
   * Commits the current transaction.
   */
  async commitTransaction(): Promise<void> {
    return this.internal_connection.commitTransaction();
  }

  /**
   * Rolls back the current transaction or to a savepoint if a name is provided.
   */
  async rollbackTransaction(name?: string): Promise<void> {
    return this.internal_connection.rollbackTransaction(name);
  }

  /**
   * Creates a savepoint with the given name in the current transaction.
   */
  async saveTransaction(name: string): Promise<void> {
    return this.internal_connection.saveTransaction(name);
  }

  getEncoding(): Encoding {
    if (this.encoding !== undefined) {
      return this.encoding;
    }
    let db_collation = this.internal_connection.getCollation();
    let encoding: Encoding = 'utf-8';
    if (db_collation != null) {
      if (db_collation.isUtf8) {
        encoding = 'utf-8';
      } else if (db_collation.sortId !== 0) {
        encoding = codepageBySortId[db_collation.sortId];
      } else {
        encoding = codepageByLanguageId[db_collation.lcidLanguageId];
      }
    } else {
      encoding = 'utf-8';
    }
    // The native connection's collation snapshot is immutable.
    this.encoding = encoding;
    return encoding;
  }

  async execute(query: string, params?: Array<Parameter>): Promise<void> {
    if (params && params.length > 0) {
      return this.internal_connection.executeWithParams(query, params);
    } else {
      return this.internal_connection.execute(query);
    }
  }

  async executeProc(
    storedProcName: string,
    namedParams: Array<Parameter>,
  ): Promise<void> {
    return this.internal_connection.executeProc(storedProcName, namedParams);
  }

  async nextResultSet() {
    return this.internal_connection.nextResultSet();
  }

  async close(): Promise<void> {
    return this.internal_connection.close();
  }

  async closeQuery(): Promise<void> {
    return this.internal_connection.closeQuery();
  }

  async queryRaw(query: string, params?: Array<Parameter>): Promise<Buffer[]> {
    return this.internal_connection.queryRaw(query, params);
  }

  async fetchChunk(byteBudget: number): Promise<ChunkResult | null> {
    return this.internal_connection.fetchChunk(byteBudget);
  }

  async getReturnValues(): Promise<Array<OutputParams> | null> {
    return this.internal_connection.getReturnValues();
  }

  // Transforms the NAPI types to JS types based on the metadata.
  static transform(
    metadata: Metadata,
    row:
      | number
      | bigint
      | boolean
      | Buffer
      | null
      | NapiSqlDateTime
      | NapiSqlTime
      | NapiSqlDateTime
      | NapiSqlDateTime2
      | NapiSqlDateTimeOffset
      | NapiSqlMoney
      | NapiDecimalParts
      | string
      | NapiF64,
  ): Row {
    let jsdatatype = metadata.dataType as unknown as JsSqlDataTypes;

    // Use transformer dictionary if available
    const transformer = tdsToJsTransformers[jsdatatype];
    if (transformer) {
      return {
        metadata,
        rowVal: transformer(metadata, row),
      };
    }

    switch (jsdatatype) {
      default:
        return {
          metadata,
          rowVal: null,
        };
    }
  }
}

export interface Row {
  metadata: Metadata;
  rowVal: number | string | boolean | Buffer | null | Date | bigint;
}
