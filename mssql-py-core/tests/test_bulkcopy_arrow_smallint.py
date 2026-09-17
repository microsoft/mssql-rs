# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for SMALLINT data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_smallint.py. Arrow ``int16`` maps naturally to SQL
``SMALLINT`` (-32768..32767); wider Arrow integer types are range-checked.

Basic, auto-mapping, and non-nullable cases are in test_bulkcopy_arrow_integers.py.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_smallint_out_of_range_raises(client_context):
    """A wider Arrow int whose value exceeds SMALLINT range must raise."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowSmallIntRange"
    cursor.execute(f"CREATE TABLE {table_name} (value SMALLINT NOT NULL)")

    source = pa.table({"value": pa.array([1, 40000, 3], type=pa.int32())})

    with pytest.raises(ValueError, match="(?i)out of range"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
