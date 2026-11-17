# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""No-server contracts for the typed host-driver bulk-copy bridge."""

import math

import pytest
from mssql_py_core import BulkCopyVector


@pytest.mark.parametrize("base_type,dimensions", [("float32", 1998), ("float16", 3996)])
def test_bridge_accepts_both_base_types_and_dimension_limits(base_type, dimensions):
    BulkCopyVector([0.1] * dimensions, base_type)
    with pytest.raises(ValueError, match="dimension"):
        BulkCopyVector([0.0] * (dimensions + 1), base_type)
    with pytest.raises(ValueError, match="dimension"):
        BulkCopyVector([], base_type)


@pytest.mark.parametrize("base_type", ["float32", "float16"])
@pytest.mark.parametrize("value", [math.nan, math.inf, -math.inf, 1e40])
def test_bridge_rejects_nonfinite_and_float32_overflow(base_type, value):
    with pytest.raises(ValueError, match="index 0"):
        BulkCopyVector([value], base_type)


def test_bridge_float16_range_and_invalid_type():
    BulkCopyVector([65504.0, -65504.0], "float16")
    with pytest.raises(ValueError, match="index 0"):
        BulkCopyVector([65505.0], "float16")
    with pytest.raises(ValueError, match="base_type"):
        BulkCopyVector([1.0], "float64")
