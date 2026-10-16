// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/**
 * Decodes the compact binary buffer returned by `Connection.queryRaw()`.
 *
 * All decoded values match the types produced by the old NapiRowWriter +
 * JS-transformer pipeline so that `Request.query()` can be wired to the
 * fast binary path without any observable behaviour change.
 *
 * Buffer layout:
 * ```
 * Header (19 bytes):
 *   magic(u32 LE)  version(u8)  col_count(u16 LE)  row_count(u32 LE)
 *   string_table_offset(u32 LE)  rows_affected(i32 LE)
 *
 * Column descriptors (col_count × 5 bytes):
 *   name_string_idx(u32 LE)  type_id(u8)
 *
 * String table:
 *   entry_count(u32 LE)
 *   [offset(u32 LE), len(u32 LE)] × entry_count
 *   [utf8 bytes...]
 *
 * Row data:
 *   [tag(u8) value...] per cell, col_count cells per row
 * ```
 */

import { LocalDate, Month } from '@js-joda/core';
import { DateWithNanosecondsDelta } from './transformers/datetime';

// Cell type tags — must match binary_row_writer.rs constants.
const TAG_NULL = 0;
const TAG_BOOL = 1;
const TAG_U8 = 2;
const TAG_I16 = 3;
const TAG_I32 = 4;
const TAG_I64 = 5;
const TAG_F32 = 6;
const TAG_F64 = 7;
const TAG_STRING_REF = 8;
const TAG_BYTES = 9;
const TAG_DECIMAL = 10;
const TAG_UUID = 11;
const TAG_DATE = 12;
const TAG_TIME = 13;
const TAG_DATETIME = 14;
const TAG_SMALLDATETIME = 15;
const TAG_DATETIME2 = 16;
const TAG_DATETIMEOFFSET = 17;
const TAG_MONEY = 18;
const TAG_SMALLMONEY = 19;

const MAGIC = 0x4d535351; // "MSSQ"

const SQL_EPOCH_DATE = LocalDate.of(1, Month.JANUARY, 1);

/** Column descriptor from the binary header. */
export interface RawColumnInfo {
  name: string;
  typeId: number;
}

/** Decoded result set. */
export interface RawResult<T = unknown[]> {
  columns: RawColumnInfo[];
  rows: T[];
  rowCount: number;
  rowsAffected: number;
}

type RowProjectorFactory<T> = (
  columns: RawColumnInfo[],
) => (values: unknown[]) => T;

/**
 * Decode a binary buffer produced by `Connection.queryRaw()`.
 *
 * All multi-byte integers are little-endian. Strings in the string table
 * are UTF-8 encoded. Temporal and decimal types are decoded to the same
 * JS representations as the existing NapiRowWriter + transformer pipeline.
 */
export function decodeRawResult(buf: Buffer): RawResult {
  return decodeResult(buf, () => (values) => values, false);
}

/** The projector must not retain the borrowed, reusable values array. */
export function decodeProjectedResult<T>(
  buf: Buffer,
  createProjector: RowProjectorFactory<T>,
): RawResult<T> {
  return decodeResult(buf, createProjector, true);
}

function decodeResult<T>(
  buf: Buffer,
  createProjector: RowProjectorFactory<T>,
  reuseRow: boolean,
): RawResult<T> {
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  let pos = 0;

  // --- Header ---
  const magic = view.getUint32(pos, true);
  pos += 4;
  if (magic !== MAGIC) {
    throw new Error(
      `Invalid binary result magic: 0x${magic.toString(16)} (expected 0x${MAGIC.toString(16)})`,
    );
  }
  const version = view.getUint8(pos);
  pos += 1;
  if (version !== 1) {
    throw new Error(`Unsupported binary result version: ${version}`);
  }

  const colCount = view.getUint16(pos, true);
  pos += 2;
  const rowCount = view.getUint32(pos, true);
  pos += 4;
  const stringTableOffset = view.getUint32(pos, true);
  pos += 4;
  const rowsAffected = view.getInt32(pos, true);
  pos += 4;

  // --- Column descriptors ---
  const colNameIndices: number[] = [];
  const colTypeIds: number[] = [];
  for (let i = 0; i < colCount; i++) {
    colNameIndices.push(view.getUint32(pos, true));
    pos += 4;
    colTypeIds.push(view.getUint8(pos));
    pos += 1;
  }

  // --- String table ---
  if (stringTableOffset < 0 || stringTableOffset >= buf.byteLength) {
    throw new Error(`Invalid string table offset: ${stringTableOffset}`);
  }
  pos = stringTableOffset;
  const entryCount = view.getUint32(pos, true);
  pos += 4;

  const stringEntries: Array<{ offset: number; len: number }> = [];
  for (let i = 0; i < entryCount; i++) {
    const offset = view.getUint32(pos, true);
    pos += 4;
    const len = view.getUint32(pos, true);
    pos += 4;
    if (offset + len > buf.byteLength) {
      throw new Error(
        `Invalid string entry bounds: offset=${offset}, len=${len}`,
      );
    }
    stringEntries.push({ offset, len });
  }

  const stringDataStart = pos;
  const textDecoder = new globalThis.TextDecoder('utf-8');

  function getString(idx: number): string {
    if (idx < 0 || idx >= stringEntries.length) {
      throw new Error(`Invalid string index: ${idx}`);
    }
    const entry = stringEntries[idx];
    const bytes = buf.subarray(
      stringDataStart + entry.offset,
      stringDataStart + entry.offset + entry.len,
    );
    return textDecoder.decode(bytes);
  }

  // Compute total string data size to find row data start
  let maxEnd = 0;
  for (const e of stringEntries) {
    const end = e.offset + e.len;
    if (end > maxEnd) maxEnd = end;
  }
  pos = stringDataStart + maxEnd;

  // --- Column names ---
  const columns: RawColumnInfo[] = [];
  for (let i = 0; i < colCount; i++) {
    columns.push({
      name: getString(colNameIndices[i]),
      typeId: colTypeIds[i],
    });
  }

  // --- Row data ---
  const project = createProjector(columns);
  const scratch = reuseRow ? new Array<unknown>(colCount) : undefined;
  const rows: T[] = [];

  for (let r = 0; r < rowCount; r++) {
    const row: unknown[] = scratch ?? new Array(colCount);
    for (let c = 0; c < colCount; c++) {
      const tag = view.getUint8(pos);
      pos += 1;

      switch (tag) {
        case TAG_NULL:
          row[c] = null;
          break;

        case TAG_BOOL:
          row[c] = view.getUint8(pos) !== 0;
          pos += 1;
          break;

        case TAG_U8:
          row[c] = view.getUint8(pos);
          pos += 1;
          break;

        case TAG_I16:
          row[c] = view.getInt16(pos, true);
          pos += 2;
          break;

        case TAG_I32:
          row[c] = view.getInt32(pos, true);
          pos += 4;
          break;

        case TAG_I64: {
          const lo = BigInt(view.getUint32(pos, true));
          const hi = BigInt(view.getInt32(pos + 4, true));
          row[c] = (hi << 32n) | lo;
          pos += 8;
          break;
        }

        case TAG_F32:
          row[c] = view.getFloat32(pos, true);
          pos += 4;
          break;

        case TAG_F64:
          row[c] = view.getFloat64(pos, true);
          pos += 8;
          break;

        case TAG_STRING_REF: {
          const idx = view.getUint32(pos, true);
          pos += 4;
          row[c] = getString(idx);
          break;
        }

        case TAG_BYTES: {
          const len = view.getUint32(pos, true);
          pos += 4;
          row[c] = Buffer.from(buf.subarray(pos, pos + len));
          pos += len;
          break;
        }

        // Matches fromNapiToJsDecimalTransformer: reconstruct number from
        // int_parts array, divide by 10^scale, apply sign.
        case TAG_DECIMAL: {
          const isPositive = view.getUint8(pos) !== 0;
          pos += 1;
          const scale = view.getUint8(pos);
          pos += 1;
          pos += 1; // precision (unused for number conversion)
          const partCount = view.getUint8(pos);
          pos += 1;
          let value = 0;
          for (let p = 0; p < partCount; p++) {
            const part = view.getInt32(pos, true);
            pos += 4;
            value += (part >>> 0) * Math.pow(0x100000000, p);
          }
          value = value / Math.pow(10, scale);
          row[c] = isPositive ? value : -value;
          break;
        }

        case TAG_UUID: {
          const bytes = buf.subarray(pos, pos + 16);
          pos += 16;
          const hex = Array.from(bytes)
            .map((b) => b.toString(16).padStart(2, '0'))
            .join('');
          row[c] =
            hex.slice(0, 8) +
            '-' +
            hex.slice(8, 12) +
            '-' +
            hex.slice(12, 16) +
            '-' +
            hex.slice(16, 20) +
            '-' +
            hex.slice(20);
          break;
        }

        // Matches fromNapiToJsDateTransformer:
        //   Date.UTC(2000, 0, daysSince010101 - 730118)
        case TAG_DATE: {
          const days = view.getUint32(pos, true);
          pos += 4;
          row[c] = new Date(Date.UTC(2000, 0, days - 730118));
          break;
        }

        // Matches fromNapiToJsTimeTransformer:
        //   time_nanoseconds is in 100ns units.
        //   → millis = time_in_100ns / 10_000
        //   → Date(UTC 1970-01-01 + millis) with .nanosecondsDelta
        case TAG_TIME: {
          pos += 1; // scale (not needed for time→Date conversion)
          const time100ns = readBigUint64(view, pos);
          pos += 8;
          row[c] = time100nsToDate(time100ns);
          break;
        }

        // Matches fromNapiToJsDateTimeTransformer:
        //   Date.UTC(1900, 0, 1 + days, 0, 0, 0, round(time * (3 + 1/3)))
        case TAG_DATETIME: {
          const days = view.getInt32(pos, true);
          pos += 4;
          const time300 = view.getUint32(pos, true);
          pos += 4;
          const ms = Math.round(time300 * (3 + 1 / 3));
          row[c] = new Date(Date.UTC(1900, 0, 1 + days, 0, 0, 0, ms));
          break;
        }

        // Matches fromNapiToJsSmallDateTimeTransformer:
        //   Date.UTC(1900, 0, 1 + days, 0, minutes)
        case TAG_SMALLDATETIME: {
          const days = view.getUint16(pos, true);
          pos += 2;
          const minutes = view.getUint16(pos, true);
          pos += 2;
          row[c] = new Date(Date.UTC(1900, 0, 1 + days, 0, minutes));
          break;
        }

        // Matches fromNapiToJsDatetime2Transformer:
        //   Uses @js-joda LocalDate for date, time100nsToDate for time,
        //   combines into DateWithNanosecondsDelta.
        case TAG_DATETIME2: {
          pos += 1; // scale
          const time100ns = readBigUint64(view, pos);
          pos += 8;
          const days = view.getUint32(pos, true);
          pos += 4;
          row[c] = datetime2ToDate(days, time100ns);
          break;
        }

        // Matches fromNapiToJsDateTimeOffsetTransformer:
        //   Delegates to datetime2 conversion, discards offset.
        case TAG_DATETIMEOFFSET: {
          pos += 1; // scale
          const time100ns = readBigUint64(view, pos);
          pos += 8;
          const days = view.getUint32(pos, true);
          pos += 4;
          pos += 2; // offset (i16, discarded — server returns UTC)
          row[c] = datetime2ToDate(days, time100ns);
          break;
        }

        // Matches moneyTransformer: reassemble mixed-endian TDS money as
        // BigInt to avoid JS Number precision loss on 64-bit scaled integers.
        case TAG_MONEY: {
          const lsb = view.getInt32(pos, true);
          pos += 4;
          const msb = view.getInt32(pos, true);
          pos += 4;
          const lsb64 = BigInt(lsb) & 0xffffffffn;
          const combined = lsb64 | (BigInt(msb) << 32n);
          row[c] = Number(combined) / 10_000;
          break;
        }

        // Matches smallMoneyTransformer: val / 10000.
        case TAG_SMALLMONEY: {
          const val = view.getInt32(pos, true);
          pos += 4;
          row[c] = val / 10_000;
          break;
        }

        default:
          throw new Error(`Unknown cell tag ${tag} at offset ${pos - 1}`);
      }
    }
    rows.push(project(row));
  }

  return { columns, rows, rowCount, rowsAffected };
}

// --- Helpers ---

function readBigUint64(view: DataView, pos: number): bigint {
  const lo = BigInt(view.getUint32(pos, true));
  const hi = BigInt(view.getUint32(pos + 4, true));
  return lo | (hi << 32n);
}

/**
 * Convert 100-nanosecond time value to DateWithNanosecondsDelta.
 * Matches fromNapiToJsTimeTransformer exactly.
 */
function time100nsToDate(time100ns: bigint): DateWithNanosecondsDelta {
  const val = Number(time100ns);
  const millis = val / 10_000;
  const nanosDelta = (val % 10_000) / 1e7;
  const d = new Date(Date.UTC(1970, 0, 1, 0, 0, 0, millis));
  (d as DateWithNanosecondsDelta).nanosecondsDelta = nanosDelta;
  return d as DateWithNanosecondsDelta;
}

/**
 * Convert days-since-0001-01-01 + 100ns time to DateWithNanosecondsDelta.
 * Matches fromNapiToJsDatetime2Transformer: uses @js-joda for the date part,
 * time100nsToDate for the time part, then combines them.
 */
function datetime2ToDate(
  days: number,
  time100ns: bigint,
): DateWithNanosecondsDelta {
  const localDate = SQL_EPOCH_DATE.plusDays(days);
  const timePart = time100nsToDate(time100ns);

  const d = new Date(
    Date.UTC(
      localDate.year(),
      localDate.monthValue() - 1,
      localDate.dayOfMonth(),
      0,
      0,
      0,
      +timePart, // coerce Date to millis-since-epoch (time part only)
    ),
  );
  (d as DateWithNanosecondsDelta).nanosecondsDelta = timePart.nanosecondsDelta;
  return d as DateWithNanosecondsDelta;
}
