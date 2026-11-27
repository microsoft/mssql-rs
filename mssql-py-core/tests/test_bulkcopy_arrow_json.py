# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for the native JSON data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_json.py. Arrow ``utf8``/``large_utf8`` maps to the native
SQL ``JSON`` type (SQL Server 2025+). Tests skip automatically if the server
does not support the native JSON type.
"""
import pytest
import mssql_py_core
import uuid

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
@pytest.mark.parametrize("arrow_type", [
    pa.string(), pa.large_string(), pa.string_view(),
    pa.json_(pa.string()), pa.json_(pa.large_string()), pa.json_(pa.string_view()),
])
@pytest.mark.parametrize("producer", ["table", "batch", "reader", "iterable", "c_array", "c_stream"])
def test_json_text_producers_slices_and_nulls(connection, arrow_type, producer):
    cursor = connection.cursor()
    cursor.execute("CREATE TABLE #JsonArrowProducer (doc JSON NULL)")
    batch = pa.record_batch(
        [pa.array(["ignored", '{"text":"\u4e2d\U0001f600"}', None], type=arrow_type).slice(1)],
        names=["doc"],
    )

    class CArray:
        def __arrow_c_array__(self, requested_schema=None):
            return batch.__arrow_c_array__(requested_schema)

    class CStream:
        def __arrow_c_stream__(self, requested_schema=None):
            return pa.RecordBatchReader.from_batches(batch.schema, [batch]).__arrow_c_stream__(
                requested_schema
            )

    source = {
        "table": lambda: pa.Table.from_batches([batch]),
        "batch": lambda: batch,
        "reader": lambda: pa.RecordBatchReader.from_batches(batch.schema, [batch]),
        "iterable": lambda: iter([batch]),
        "c_array": CArray,
        "c_stream": CStream,
    }[producer]()
    result = cursor.bulkcopy_arrow(
        "#JsonArrowProducer", source, keep_nulls=True, use_internal_transaction=True,
    )
    assert result["rows_copied"] == 2
    cursor.execute("SELECT JSON_VALUE(doc, '$.text') FROM #JsonArrowProducer")
    assert set(cursor.fetchall()) == {("\u4e2d\U0001f600",), (None,)}


@pytest.mark.integration
@pytest.mark.parametrize("arrow_type", [
    pa.string(), pa.large_string(), pa.string_view(), pa.null(),
    pa.json_(pa.string()), pa.json_(pa.large_string()), pa.json_(pa.string_view()),
])
@pytest.mark.parametrize("values", [[], [None, None]])
def test_json_empty_and_null_arrays(connection, arrow_type, values):
    cursor = connection.cursor()
    cursor.execute("CREATE TABLE #JsonArrowEmpty (doc JSON NULL)")
    result = cursor.bulkcopy_arrow(
        "#JsonArrowEmpty", pa.table({"doc": pa.array(values, type=arrow_type)}),
        keep_nulls=True, use_internal_transaction=True,
    )
    assert result["rows_copied"] == len(values)


@pytest.mark.integration
@pytest.mark.parametrize("empty", [False, True])
@pytest.mark.parametrize("later", [False, True])
@pytest.mark.parametrize("annotation", [
    {"ARROW:extension:name": ""},
    {"ARROW:extension:name": "unknown.extension"},
    {"ARROW:extension:metadata": "malformed"},
])
def test_json_rejects_extension_annotations_before_batch_use(connection, empty, later, annotation):
    cursor = connection.cursor()
    cursor.execute("CREATE TABLE #JsonArrowExtension (doc JSON NULL)")
    plain = pa.record_batch([pa.array(["{}"])], names=["doc"])
    schema = pa.schema([pa.field("doc", pa.string(), metadata=annotation)])
    tagged = pa.record_batch([pa.array([] if empty else ["{}"], type=pa.string())], schema=schema)
    reader = pa.RecordBatchReader.from_batches(
        plain.schema if later else schema, [plain, tagged] if later else [tagged],
    )
    with pytest.raises(ValueError, match="unsupported extension annotations"):
        cursor.bulkcopy_arrow(
            "#JsonArrowExtension", reader, batch_size=10, use_internal_transaction=True,
        )
    cursor.execute("SELECT COUNT(*) FROM #JsonArrowExtension")
    assert cursor.fetchone()[0] == 0


@pytest.mark.integration
@pytest.mark.parametrize("kind", ["type", "name", "missing"])
@pytest.mark.parametrize("empty", [False, True])
def test_json_later_mapped_schema_drift_aborts_batch(connection, kind, empty):
    cursor = connection.cursor()
    cursor.execute("CREATE TABLE #JsonSchemaDrift (doc JSON NULL)")
    first = pa.record_batch([pa.array(["{}"])], names=["doc"])
    if kind == "type":
        later = pa.record_batch([pa.array([] if empty else [1], type=pa.int32())], names=["doc"])
    elif kind == "name":
        later = pa.record_batch([pa.array([] if empty else ["{}"], type=pa.string())], names=["renamed"])
    else:
        later = pa.record_batch([], names=[])
    reader = pa.RecordBatchReader.from_batches(first.schema, [first, later])
    with pytest.raises(ValueError, match="Mapped Arrow source column"):
        cursor.bulkcopy_arrow(
            "#JsonSchemaDrift", reader, use_internal_transaction=True, batch_size=10,
        )
    cursor.execute("SELECT COUNT(*) FROM #JsonSchemaDrift")
    assert cursor.fetchone()[0] == 0


@pytest.mark.integration
@pytest.mark.parametrize("producer", ["c_stream", "c_array"])
def test_json_extension_rejected_through_real_capsules(connection, producer):
    cursor = connection.cursor()
    cursor.execute("CREATE TABLE #JsonCapsuleExtension (doc JSON NULL)")
    schema = pa.schema([pa.field("doc", pa.string(), metadata={"ARROW:extension:name": "unknown.extension"})])
    batch = pa.record_batch([pa.array(["{}"])], schema=schema)

    class CArray:
        def __arrow_c_array__(self, requested_schema=None):
            return batch.__arrow_c_array__(requested_schema)

    source = CArray() if producer == "c_array" else pa.Table.from_batches([batch])
    with pytest.raises(ValueError, match="unsupported extension annotations"):
        cursor.bulkcopy_arrow("#JsonCapsuleExtension", source, use_internal_transaction=True)


@pytest.mark.integration
def test_json_mapping_ignores_unmapped_extensions_and_ordinary_metadata(connection):
    cursor = connection.cursor()
    cursor.execute("CREATE TABLE #JsonUnmappedExtension (doc JSON NULL)")
    schema = pa.schema([
        pa.field("hidden", pa.string(), metadata={"ARROW:extension:name": "unknown"}),
        pa.field("doc", pa.string(), metadata={"application": "ordinary"}),
    ])
    batch = pa.record_batch([pa.array(["not JSON"]), pa.array(["{}"])], schema=schema)
    result = cursor.bulkcopy_arrow(
        "#JsonUnmappedExtension", batch, column_mappings=[(1, "doc")],
        use_internal_transaction=True,
    )
    assert result["rows_copied"] == 1


@pytest.mark.integration
@pytest.mark.parametrize("producer", ["reader", "iterable"])
@pytest.mark.parametrize("later_annotation", [False, True])
@pytest.mark.parametrize("empty", [False, True])
def test_json_reordered_mapping_checks_later_destination_annotations(
    connection, producer, later_annotation, empty,
):
    cursor = connection.cursor()
    cursor.execute("CREATE TABLE #ReorderedJsonArrow (label NVARCHAR(20), doc JSON)")
    schema = pa.schema([("payload", pa.string()), ("label", pa.string()), ("hidden", pa.int32())])
    first = pa.record_batch(
        [pa.array(['{"value":1}']), pa.array(["first"]), pa.array([0], type=pa.int32())],
        schema=schema,
    )
    later_schema = pa.schema([
        pa.field("payload", pa.string(), metadata=(
            {"ARROW:extension:name": "unknown.extension"} if later_annotation else {"application": "plain"}
        )),
        pa.field("label", pa.string()),
        pa.field("ignored", pa.string(), metadata={"ARROW:extension:name": "unknown"}),
    ])
    later = pa.record_batch([
        pa.array([] if empty else ['{"value":2}'], type=pa.string()),
        pa.array([] if empty else ["second"], type=pa.string()),
        pa.array([] if empty else ["ignored"], type=pa.string()),
    ], schema=later_schema)
    source = (
        pa.RecordBatchReader.from_batches(schema, [first, later])
        if producer == "reader" else iter([first, later])
    )
    options = {
        "column_mappings": [(1, "label"), (0, "doc")],
        "batch_size": 10, "use_internal_transaction": True,
    }
    if later_annotation:
        with pytest.raises(ValueError, match="unsupported extension annotations"):
            cursor.bulkcopy_arrow("#ReorderedJsonArrow", source, **options)
        expected = []
    else:
        result = cursor.bulkcopy_arrow("#ReorderedJsonArrow", source, **options)
        assert result["rows_copied"] == (1 if empty else 2)
        expected = [("first", "1")] if empty else [("first", "1"), ("second", "2")]
    cursor.execute(
        "SELECT label, JSON_VALUE(doc, '$.value') FROM #ReorderedJsonArrow ORDER BY label"
    )
    assert cursor.fetchall() == expected


@pytest.mark.integration
@pytest.mark.parametrize("storage", [pa.string(), pa.large_string(), pa.string_view()])
@pytest.mark.parametrize("producer", ["reader", "iterable"])
def test_json_extension_and_plain_batches_share_text_plan(connection, storage, producer):
    cursor = connection.cursor()
    cursor.execute("CREATE TABLE #JsonExtensionBatches (id INT, doc JSON NULL)")
    batches = [
        pa.record_batch([
            pa.array(values, type=pa.json_(storage) if extension else storage),
            pa.array(ids, type=pa.int32()),
        ], names=["payload", "id"])
        for extension, values, ids in [
            (True, ['{"value":1}'], [1]),
            (False, [], []),
            (False, ["[2]", None], [2, 3]),
            (True, [], []),
            (True, ['{"value":4}'], [4]),
        ]
    ]
    source = (
        pa.RecordBatchReader.from_batches(batches[0].schema, batches)
        if producer == "reader" else iter(batches)
    )
    result = cursor.bulkcopy_arrow(
        "#JsonExtensionBatches", source, column_mappings=[(1, "id"), (0, "doc")],
        batch_size=2, keep_nulls=True, use_internal_transaction=True,
    )
    assert result["rows_copied"] == 4
    cursor.execute(
        "SELECT id, JSON_VALUE(doc, '$.value'), JSON_VALUE(doc, '$[0]') "
        "FROM #JsonExtensionBatches ORDER BY id"
    )
    assert cursor.fetchall() == [(1, "1", None), (2, None, "2"), (3, None, None), (4, "4", None)]


@pytest.mark.integration
@pytest.mark.parametrize("bad_text", ["123", "true", "false", '"text"', "null", "", "{"])
@pytest.mark.parametrize("batch_size", [0, 2])
def test_json_extension_leaves_document_validation_to_server(client_context, bad_text, batch_size):
    table = "JsonExtensionFailure_" + uuid.uuid4().hex
    observer = mssql_py_core.PyCoreConnection(client_context)
    writer = mssql_py_core.PyCoreConnection(client_context)
    check = observer.cursor()
    check.execute(f"CREATE TABLE {table} (id INT, doc JSON)")
    try:
        batch = pa.record_batch([
            pa.array([0, 1, 2, 3], type=pa.int32()),
            pa.array(["{}", "[]", "{}", bad_text], type=pa.json_()),
        ], names=["id", "doc"])
        with pytest.raises(RuntimeError, match="(?i)json"):
            writer.cursor().bulkcopy_arrow(
                table, batch, batch_size=batch_size, use_internal_transaction=True,
            )
        check.execute(f"SELECT id FROM {table} ORDER BY id")
        assert check.fetchall() == ([(0,), (1,)] if batch_size else [])
    finally:
        writer.close()
        check.execute(f"DROP TABLE {table}")
        observer.close()


def _create_json_table(cursor, table_name, columns):
    """Create a table with a native JSON column, skipping if unsupported."""
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    try:
        cursor.execute(f"CREATE TABLE {table_name} ({columns})")
    except Exception as exc:  # pragma: no cover - server-version dependent
        pytest.skip(f"native JSON type not supported by this server: {exc}")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_json_basic(client_context):
    """Arrow utf8 bulkcopy into a native JSON column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableJson"
    _create_json_table(cursor, table_name, "id INT, doc JSON")

    docs = [
        '{"id": 1, "name": "Alice"}',
        '{"id": 2, "values": [1, 2, 3]}',
        '{"nested": {"a": true, "b": null}}',
    ]
    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "doc": pa.array(docs),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "doc")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(
        f"SELECT id, JSON_VALUE(doc, '$.id') FROM {table_name} WHERE id = 1"
    )
    row = cursor.fetchone()
    assert row[0] == 1 and str(row[1]) == "1"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_json_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableJson"
    _create_json_table(cursor, table_name, "id INT, doc JSON")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "doc": pa.array(['{"a": 1}', None, '{"c": 3}']),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, JSON_VALUE(doc, '$.a') FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert str(rows[0][1]) == "1"
    assert rows[1][1] is None

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_json_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable JSON column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowNonNullableJson"
    _create_json_table(cursor, table_name, "doc JSON NOT NULL")

    source = pa.table({"doc": pa.array(['{"a": 1}', None])})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
    conn.close()
