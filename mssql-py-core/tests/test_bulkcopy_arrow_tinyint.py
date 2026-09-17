# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for TINYINT data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_tinyint.py. Arrow ``uint8`` maps naturally to SQL
``TINYINT`` (0-255); wider Arrow integer types are range-checked per cell when
they target ``TINYINT``.

Basic, auto-mapping, and non-nullable cases are in test_bulkcopy_arrow_integers.py.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_tinyint_out_of_range_raises(client_context):
    """A wider Arrow int whose value exceeds TINYINT range must raise, not wrap."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowTinyIntRange"
    cursor.execute(f"CREATE TABLE {table_name} (value TINYINT NOT NULL)")

    # 256 exceeds TINYINT (0-255); int16 is a valid source that targets TINYINT.
    source = pa.table({"value": pa.array([1, 256, 3], type=pa.int16())})

    with pytest.raises(ValueError, match="(?i)out of range"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
