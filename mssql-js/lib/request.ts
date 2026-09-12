// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { SqlJsConnection } from '.';
import { DataType } from './datatypes';
import { JsSqlDataTypes } from './datatypes/enums';
import { SqlDataTypes, Parameter } from './generated';
import { decodeProjectedResult, RawColumnInfo, RawResult } from './decode';

type ColumnValue = number | string | boolean | Buffer | null | Date | bigint;

// makes a row interface that assigns a key string to a value
export interface RecordSetRow {
  [key: string]: ColumnValue | Array<ColumnValue>;
}

// interface to group together column metadata
export interface Column {
  index: number;
  name: string;
  type: SqlDataTypes | undefined;
}

// creates an array of record set rows with columns and row count properties
export interface RecordSet extends Array<RecordSetRow> {
  columns: Column[];
  rowCount: number;
}

export interface IResult {
  IRecordSets: RecordSet[];
  IRecordSet: RecordSet | null;
  rowCount: number;
  output: {
    [key: string]: number | string | boolean | Buffer | null | Date | bigint;
  };
}

//data types that are able to be parameterized
export type JsSqlParameterTypes =
  | JsSqlDataTypes.VarChar
  | JsSqlDataTypes.Date
  | JsSqlDataTypes.Time
  | JsSqlDataTypes.DateTime2
  | JsSqlDataTypes.DateTimeOffset
  | JsSqlDataTypes.Char
  | JsSqlDataTypes.TinyInt
  | JsSqlDataTypes.Bit
  | JsSqlDataTypes.SmallInt
  | JsSqlDataTypes.Decimal
  | JsSqlDataTypes.Int
  | JsSqlDataTypes.SmallDateTime
  | JsSqlDataTypes.Real
  | JsSqlDataTypes.Money
  | JsSqlDataTypes.DateTime
  | JsSqlDataTypes.Float
  | JsSqlDataTypes.Numeric
  | JsSqlDataTypes.SmallMoney
  | JsSqlDataTypes.BigInt
  | JsSqlDataTypes.NVarChar
  | JsSqlDataTypes.NChar
  | JsSqlDataTypes.Xml
  | JsSqlDataTypes.UniqueIdentifier;

export class Request {
  connection: SqlJsConnection;
  private params: Parameter[];
  constructor(connection: SqlJsConnection) {
    this.connection = connection;
    this.params = [];
  }

  input(varName: string, type: DataType | (() => DataType), value: unknown) {
    //adds a '@' to a variable name if the use does not put one
    if (!varName.startsWith('@')) {
      varName = '@' + varName;
    }
    //if the type is a function, call it to get the DataType
    if (typeof type === 'function') {
      type = type();
    }

    let sqltype: JsSqlParameterTypes;
    if (typeof type === 'object' && 'sqlType' in type) {
      sqltype = type.sqlType as JsSqlParameterTypes;
      let transformed_value = type.transformForNapiWrites(
        value as unknown as number | string | Date | boolean | null,
        this.connection.getEncoding(),
      );

      //collects the input parameters into the global parameters
      this.params.push({
        name: varName,
        dataType: sqltype as unknown as SqlDataTypes,
        value: transformed_value,
        direction: 0,
        length: typeof type.length === 'function' ? type.length() : undefined,
      });
    } else {
      throw new TypeError('Invalid type provided for input');
    }
  }

  output(varName: string, type: DataType | (() => DataType), value: unknown) {
    //adds a '@' to a variable name if the use does not put one
    if (!varName.startsWith('@')) {
      varName = '@' + varName;
    }
    //if the type is a function, call it to get the DataType
    if (typeof type === 'function') {
      type = type();
    }

    let sqltype: JsSqlParameterTypes;
    if (typeof type === 'object' && 'sqlType' in type) {
      sqltype = type.sqlType as JsSqlParameterTypes;
      if (value === undefined) {
        value = null; // default to null if no value is provided
      }
      let transformed_value = type.transformForNapiWrites(
        value as unknown as number | string | Date | boolean | null,
        this.connection.getEncoding(),
      );
      //collects the input parameters into the global parameters
      this.params.push({
        name: varName,
        dataType: sqltype as unknown as SqlDataTypes,
        value: transformed_value,
        direction: 1,
      });
    } else {
      throw new TypeError(
        'Invalid type provided for output. Expected a DataType object.',
      );
    }
  }

  async query(command: string): Promise<IResult> {
    const buffers = await this.connection.queryRaw(
      command,
      this.params.length > 0 ? this.params : undefined,
    );
    const rawResults = buffers.map((buf) =>
      decodeProjectedResult(buf, createRowProjector),
    );
    return reshapeRawToIResult(rawResults);
  }

  async execute(storedProcName: string): Promise<IResult> {
    //will correctly run regardless if there are parameters or not
    await this.connection.executeProc(storedProcName, this.params);

    let result: IResult = await this.createResultFast();

    let returnValues = await this.connection.getReturnValues();
    if (returnValues) {
      result.output = {};
      returnValues.forEach((rv) => {
        let paramName = rv.name;
        let paramValue = rv.value;
        if (paramName.charAt(0) === '@') {
          paramName = paramName.slice(1);
        }

        let transformedVal = SqlJsConnection.transform(rv.metadata, paramValue);
        result.output[paramName] = transformedVal.rowVal;
      });
    }
    await this.connection.closeQuery();

    return result;
  }

  private async createResultFast(): Promise<IResult> {
    const BYTE_BUDGET = 256 * 1024;
    const rawResults: RawResult<RecordSetRow>[] = [];

    while (true) {
      let merged: RawResult<RecordSetRow> | null = null;

      while (true) {
        const chunk = await this.connection.fetchChunk(BYTE_BUDGET);
        if (!chunk) break;

        const decoded = decodeProjectedResult(chunk.data, createRowProjector);
        if (!merged) {
          merged = decoded;
        } else {
          merged.rows.push(...decoded.rows);
          merged.rowCount += decoded.rowCount;
        }

        if (!chunk.hasMore) break;
      }

      if (merged) {
        rawResults.push(merged);
      } else {
        break;
      }

      if (!(await this.connection.nextResultSet())) break;
    }

    return reshapeRawToIResult(rawResults);
  }
}

function createRowProjector(columns: RawColumnInfo[]) {
  const names = columns.map((column) => column.name);
  return (values: readonly unknown[]): RecordSetRow => {
    const row: RecordSetRow = {};
    for (let c = 0; c < names.length; c++) {
      const name = names[c];
      const val = values[c] as ColumnValue;
      if (name === '' && '' in row) {
        if (Array.isArray(row[''])) {
          (row[''] as ColumnValue[]).push(val);
        } else {
          row[''] = [row[''], val];
        }
      } else {
        row[name] = val;
      }
    }
    return row;
  };
}

/** Convert decoded result sets into an `IResult`. */
function reshapeRawToIResult(rawResults: RawResult<RecordSetRow>[]): IResult {
  const recordSets: RecordSet[] = [];
  let totalRowCount = 0;

  for (const raw of rawResults) {
    const colCount = raw.columns.length;
    const colNames: string[] = raw.columns.map((c) => c.name);

    // Build column metadata, collapsing duplicate unnamed columns (matches createResult)
    const columns: Column[] = [];
    let seenEmpty = false;
    for (let i = 0; i < colCount; i++) {
      const name = colNames[i];
      if (name === '' && seenEmpty) {
        continue;
      }
      if (name === '') seenEmpty = true;
      columns.push({
        index: i,
        name,
        type:
          name.length > 0 ? (raw.columns[i].typeId as SqlDataTypes) : undefined,
      });
    }

    const recordSet: RecordSet = Object.assign(raw.rows, {
      columns,
      rowCount: raw.rowCount,
    });

    recordSets.push(recordSet);
    totalRowCount += raw.rowCount;
  }

  return {
    IRecordSets: recordSets,
    IRecordSet: recordSets.length > 0 ? recordSets[0] : null,
    rowCount: totalRowCount,
    output: {},
  };
}
