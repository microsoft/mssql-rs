# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Compare warmed native fetch loops using the existing test connection settings."""

import argparse
import asyncio
import json
import os
from pathlib import Path
import statistics
import sys
import time
import warnings

import mssql_py_core

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))
from conftest import get_client_context


async def measure(rows, samples, warmups, batch_size):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        connection = await mssql_py_core.PyAsyncConnection.connect(
            get_client_context(), None, autocommit=True
        )
    query = (
        f"SELECT TOP ({rows}) "
        "CONVERT(int, ROW_NUMBER() OVER (ORDER BY (SELECT NULL))) AS n "
        "FROM sys.all_objects AS a CROSS JOIN sys.all_objects AS b "
        "ORDER BY n OPTION (MAXDOP 1)"
    )
    results = {}
    try:
        cursor = connection.cursor()
        for operation in ("fetchone", "fetchmany"):
            durations = []
            for sample in range(warmups + samples):
                await cursor.execute(query, use_prepare=False)
                count = checksum = 0
                started = time.perf_counter()
                if operation == "fetchone":
                    first = cursor.fetchone()
                    has_settlement_hook = hasattr(first, "_cancel_and_settle")
                    row = await first
                    del first
                    while row is not None:
                        count += 1
                        checksum += row[0]
                        row = await cursor.fetchone()
                else:
                    first = cursor.fetchmany(batch_size)
                    has_settlement_hook = hasattr(first, "_cancel_and_settle")
                    batch = await first
                    del first
                    while batch:
                        count += len(batch)
                        checksum += sum(row[0] for row in batch)
                        batch = await cursor.fetchmany(batch_size)
                elapsed = time.perf_counter() - started
                if count != rows or checksum != rows * (rows + 1) // 2:
                    raise AssertionError(
                        f"{operation}: invalid result count={count}, checksum={checksum}"
                    )
                if sample >= warmups:
                    durations.append(elapsed)
            median = statistics.median(durations)
            results[operation] = {
                "seconds": durations,
                "median_seconds": median,
                "median_us_per_row": median * 1_000_000 / rows,
                "median_rows_per_second": rows / median,
                "first_awaitable_has_settlement_hook": has_settlement_hook,
            }
        await cursor.close()
    finally:
        await connection.close()
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rows", type=int, default=10_000)
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--batch-size", type=int, default=256)
    args = parser.parse_args()
    if args.rows <= 0 or args.samples <= 0 or args.warmups < 0 or args.batch_size <= 0:
        parser.error("rows, samples and batch-size must be positive; warmups nonnegative")
    results = asyncio.run(
        measure(args.rows, args.samples, args.warmups, args.batch_size)
    )
    print(
        json.dumps(
            {
                "configuration": vars(args),
                "python_version": sys.version.split()[0],
                "native_module": mssql_py_core.__file__,
                "runtime_workers": os.environ.get("TOKIO_WORKER_THREADS"),
                "results": results,
            }
        )
    )


if __name__ == "__main__":
    main()
