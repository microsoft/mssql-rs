# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Run explicitly with ADBC_TEST_LIBRARY and the ADBC_TEST_* connection variables."""

import os

import adbc_driver_manager.dbapi
import pyarrow as pa


def main():
    with adbc_driver_manager.dbapi.connect(
        driver=os.environ["ADBC_TEST_LIBRARY"],
        entrypoint="AdbcDriverMssqlInit",
        db_kwargs={
            "mssql.data_source": os.environ["ADBC_TEST_SERVER"],
            "mssql.database": os.environ["ADBC_TEST_DATABASE"],
            "username": os.environ["ADBC_TEST_USER"],
            "password": os.environ["ADBC_TEST_PASSWORD"],
        },
        autocommit=True,
    ) as connection:
        with connection.cursor() as cursor:
            cursor.execute("SELECT CAST(42 AS int) AS answer")
            assert cursor.fetch_arrow_table().to_pydict() == {"answer": [42]}
            cursor.execute("SELECT CAST(@p1 AS int) AS n", (7,))
            assert cursor.fetch_arrow_table().to_pydict() == {"n": [7]}
            table = pa.table(
                {"n": pa.array([1, 2, 3], type=pa.int32()), "label": ["a", None, "\u03b2"]}
            )
            assert cursor.adbc_ingest("#adbc_python", table, temporary=True) == 3
            cursor.execute("SELECT n,label FROM #adbc_python ORDER BY n")
            assert cursor.fetch_arrow_table().equals(table)
            cursor.execute("DROP TABLE #adbc_python")
        connection.adbc_get_info()
    print("Python driver-manager query, binding, ingestion and release passed")


if __name__ == "__main__":
    main()
