# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Value-level coverage for native bulk-copy datetime string conversion."""

import pytest
import mssql_py_core


@pytest.mark.integration
@pytest.mark.parametrize("sql_type", ["DATETIME2", "DATETIMEOFFSET"])
@pytest.mark.parametrize("scale", range(8))
def test_bulkcopy_datetime_string_fractional_values(client_context, sql_type, scale):
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()
    table_name = f"BulkCopyStringPrecision{sql_type}{scale}"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    try:
        cursor.execute(
            f"CREATE TABLE {table_name} (id INT, event_time {sql_type}({scale}))"
        )
        values = [
            ("2024-07-01 12:00:00.1234567", "1234567", 0),
            ("2024-07-01T12:00:00.0000001Z", "0000001", 0),
            ("2024-07-01T12:00:00.9999999+05:30", "9999999", 330),
            ("2024-07-01T12:00:00.1234567-07:00", "1234567", -420),
            ("2024-07-01T12:00:00Z", "0000000", 0),
            ("2024-07-01T12:00:00.123456Z", "1234560", 0),
            ("2024-W27-120000", "0000000", 0),
            ("2024-W27-120000.1234567", "1234567", 0),
            ("2024-W27-120000.1234567+05:30", "1234567", 330),
            ("2024-W27-1T120000.1234567", "1234567", 0),
            ("2024W271T120000.1234567-07:00", "1234567", -420),
            ("2024-07-01T12000012", "1200000", 0),
            ("2024-07-01T1200001234567+05:30", "1234567", 330),
            ("2024-07-01T12:00:00:00", "0000000", 0),
            ("2024-07-01T12:00:00:0000001Z", "0000001", 0),
            ("2024-07-01T12:00:00:1234567-07:00", "1234567", -420),
            ("2024-07-01T12:00:00.1234567+05:60", "1234567", 360),
            ("2024-07-01T12:00:00.1234567-05:60", "1234567", -360),
            ("2024-07-01T12:00:00.1234567+00:00:60", "1234567", 1),
            ("2024-07-01T12:00:00.1234567+00:90", "1234567", 90),
        ]
        result = cursor.bulkcopy(
            table_name, iter((index, value) for index, (value, _, _) in enumerate(values))
        )
        assert result["rows_copied"] == len(values)
        cursor.execute(
            f"SELECT id, "
            f"CONVERT(VARCHAR(27), CAST(event_time AS DATETIME2(7)), 121), "
            f"DATEPART(TZOFFSET, event_time) FROM {table_name} ORDER BY id"
        )
        rows = cursor.fetchall()
        assert len(rows) == len(values)
        for index, (_, fraction, offset) in enumerate(values):
            expected_fraction = fraction[:scale].ljust(7, "0")
            assert rows[index][0] == index
            assert rows[index][1] == f"2024-07-01 12:00:00.{expected_fraction}"
            assert rows[index][2] == (offset if sql_type == "DATETIMEOFFSET" else 0)
    finally:
        cursor.execute(f"DROP TABLE IF EXISTS {table_name}")
        conn.close()
