# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Bulk copy of vectors whose base type differs from the destination column.

Each vector column is declared with its source base type (row sources: the
column's first non-NULL ``BulkCopyVector``; Arrow: ``mssql.vector.base_type``
metadata) and SQL Server converts it. Source columns are deliberately ordered
differently from the destination so a wrong column index is detected.
"""

import pytest
import mssql_py_core
from mssql_py_core import BulkCopyVector

TABLE = "BulkCopyVectorBaseTypes"


@pytest.fixture
def cursor(client_context):
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()
    try:
        yield cursor
    finally:
        cursor.execute(f"IF OBJECT_ID('{TABLE}', 'U') IS NOT NULL DROP TABLE {TABLE}")
        conn.close()


def _create_mixed_table(cursor):
    try:
        cursor.execute(f"IF OBJECT_ID('{TABLE}', 'U') IS NOT NULL DROP TABLE {TABLE}")
        cursor.execute(
            f"CREATE TABLE {TABLE} (id INT, a VECTOR(3, float16) NULL, b VECTOR(3) NULL)"
        )
    except Exception as error:  # noqa: BLE001 - servers without FLOAT16 vectors
        pytest.skip(f"VECTOR(float16) is not available: {error}")


def _rows(cursor):
    cursor.execute(f"SELECT id, a, b FROM {TABLE} ORDER BY id")
    return [
        (
            row[0],
            None if row[1] is None else list(row[1]),
            None if row[2] is None else list(row[2]),
        )
        for row in cursor.fetchall()
    ]


@pytest.mark.integration
def test_row_bulkcopy_declares_first_non_null_base_type_per_column(cursor):
    _create_mixed_table(cursor)
    data = [
        (None, 1, None),
        (BulkCopyVector([0.5, 1.5, 2.5], "float16"), 2, None),
        (None, 3, BulkCopyVector([1.0, 2.0, 3.0], "float32")),
        (
            BulkCopyVector([4.0, 5.0, 6.0], "float16"),
            4,
            BulkCopyVector([7.0, 8.0, 9.0], "float32"),
        ),
    ]
    result = cursor.bulkcopy(
        TABLE,
        iter(data),
        batch_size=2,
        column_mappings=[(1, "id"), (2, "a"), (0, "b")],
    )
    assert result["rows_copied"] == 4
    assert _rows(cursor) == [
        (1, None, None),
        (2, None, [0.5, 1.5, 2.5]),
        (3, [1.0, 2.0, 3.0], None),
        (4, [7.0, 8.0, 9.0], [4.0, 5.0, 6.0]),
    ]


@pytest.mark.integration
def test_row_bulkcopy_rejects_other_base_type_after_first_non_null(cursor):
    _create_mixed_table(cursor)
    data = [
        (1, None, BulkCopyVector([1.0, 2.0, 3.0], "float16")),
        (2, None, BulkCopyVector([1.0, 2.0, 3.0], "float32")),
    ]
    with pytest.raises(Exception, match="base"):
        cursor.bulkcopy(TABLE, iter(data))


@pytest.mark.integration
def test_row_bulkcopy_lookahead_snapshots_reused_mutable_cells(cursor):
    cursor.execute(f"IF OBJECT_ID('{TABLE}', 'U') IS NOT NULL DROP TABLE {TABLE}")
    cursor.execute(f"CREATE TABLE {TABLE} (id INT, a VECTOR(1) NULL, b VECTOR(1) NULL)")

    def rows():
        values = [1.0]
        yield (1, values, None)
        values[:] = [2.0]
        yield (2, values, None)
        values[:] = [3.0]
        yield (3, values, None)

    assert cursor.bulkcopy(TABLE, rows())["rows_copied"] == 3
    assert _rows(cursor) == [(1, [1.0], None), (2, [2.0], None), (3, [3.0], None)]


def _vector_field(pa, name, tag):
    metadata = None if tag is None else {"mssql.vector.base_type": tag}
    return pa.field(name, pa.list_(pa.float32(), 3), metadata=metadata)


@pytest.mark.integration
def test_arrow_bulkcopy_declares_tagged_base_types(cursor):
    pa = pytest.importorskip("pyarrow")
    _create_mixed_table(cursor)
    schema = pa.schema(
        [
            _vector_field(pa, "b_src", "float16"),
            pa.field("id", pa.int32()),
            _vector_field(pa, "a_src", "float32"),
        ]
    )
    table = pa.table(
        {
            "b_src": [[4.0, 5.0, 6.0], None],
            "id": [1, 2],
            "a_src": [None, [0.5, 1.5, 2.5]],
        },
        schema=schema,
    )
    result = cursor.bulkcopy_arrow(
        TABLE,
        table,
        batch_size=1000,
        timeout=30,
        column_mappings=[(1, "id"), (2, "a"), (0, "b")],
    )
    assert result["rows_copied"] == 2
    assert _rows(cursor) == [(1, None, [4.0, 5.0, 6.0]), (2, [0.5, 1.5, 2.5], None)]


@pytest.mark.integration
def test_arrow_bulkcopy_rejects_later_batch_with_conflicting_metadata(cursor):
    pa = pytest.importorskip("pyarrow")
    _create_mixed_table(cursor)

    def batch(tag, row_id):
        schema = pa.schema([pa.field("id", pa.int32()), _vector_field(pa, "a", tag)])
        return pa.record_batch(
            [
                pa.array([row_id], pa.int32()),
                pa.array([[1.0, 2.0, 3.0]], schema.field("a").type),
            ],
            schema=schema,
        )

    with pytest.raises(Exception, match="later batch"):
        cursor.bulkcopy_arrow(
            TABLE,
            iter([batch("float16", 1), batch("float32", 2)]),
            batch_size=0,
            timeout=30,
            column_mappings=[(0, "id"), (1, "a")],
        )
    # The rejected batch fails the in-flight bulk-copy batch instead of committing it.
    cursor.execute(f"SELECT COUNT(*) FROM {TABLE}")
    assert cursor.fetchall()[0][0] == 0
