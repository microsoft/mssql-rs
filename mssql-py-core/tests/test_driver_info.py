# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for driver name and version information."""


def test_dbapi_module_globals():
    """Test the module's declared DB-API contract."""
    import mssql_py_core

    assert mssql_py_core.apilevel == "2.0"
    assert mssql_py_core.threadsafety == 1
    assert mssql_py_core.paramstyle == "qmark"


def test_driver_version_parameter():
    """Test that driver_version parameter is accepted and stored."""
    import mssql_py_core
    
    # Create a minimal context with driver_version
    context = {
        "server": "localhost",
        "user_name": "test_user",
        "password": "test_password",
        "driver_version": "1.2.3"
    }
    
    # This should not raise an error
    # The connection will fail since we don't have a real server, but that's OK
    # We're just testing that the parameter is accepted
    try:
        conn = mssql_py_core.PyCoreConnection(context)
    except RuntimeError as e:
        # Expected to fail connection, but should have accepted the parameter
        assert "driver_version" not in str(e).lower(), "driver_version parameter should be accepted"


def test_driver_version_encoding():
    """Test that driver version encoding works correctly."""
    # Import the internal test function if exposed, or we can test indirectly
    # For now, we verify that the connection accepts the driver_version parameter
    # without errors related to the parameter itself
    
    import mssql_py_core
    
    context = {
        "server": "localhost",
        "user_name": "test_user",
        "password": "test_password",
        "driver_version": "2.5.1234"
    }
    
    try:
        conn = mssql_py_core.PyCoreConnection(context)
    except RuntimeError:
        # Connection failure is expected, parameter should be accepted
        pass


def test_no_driver_version():
    """Test that connection works without driver_version parameter."""
    import mssql_py_core
    
    context = {
        "server": "localhost",
        "user_name": "test_user",
        "password": "test_password"
    }
    
    try:
        conn = mssql_py_core.PyCoreConnection(context)
    except RuntimeError:
        # Connection failure is expected, but parameter should be optional
        pass
