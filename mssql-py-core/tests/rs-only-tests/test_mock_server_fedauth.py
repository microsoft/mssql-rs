# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""
Integration tests for FedAuth (Access Token) authentication using the mock TDS server.

This test validates that the mssql-py-core driver correctly sends access tokens
to the SQL Server using FedAuth (Federated Authentication) protocol.

The mock TDS server Python bindings (mssql-mock-tds-py) are used to verify
that the server received the exact access token that was sent.

Usage:
    ./dev/test-python.sh
"""

import socket
import pytest
import secrets

# Try to import the mock TDS server Python bindings
try:
    import mssql_mock_tds_py
    MOCK_TDS_PY_AVAILABLE = True
except ImportError:
    MOCK_TDS_PY_AVAILABLE = False


def is_port_available(port: int, host: str = "127.0.0.1") -> bool:
    """Check if a port is available."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        try:
            s.bind((host, port))
            return True
        except OSError:
            return False


def find_available_port(start_port: int = 14330, max_attempts: int = 100) -> int:
    """Find an available port starting from start_port."""
    for port in range(start_port, start_port + max_attempts):
        if is_port_available(port):
            return port
    raise RuntimeError(f"No available port found in range {start_port}-{start_port + max_attempts}")


@pytest.fixture
def mock_server_port():
    """Get port 0 for OS-assigned port selection."""
    return 0


@pytest.mark.skipif(
    not MOCK_TDS_PY_AVAILABLE,
    reason="mssql_mock_tds_py not available. Build it with: cd mssql-mock-tds-py && maturin develop",
)
class TestMockServerFedAuth:
    """Test FedAuth (access token) authentication with mock TDS server."""

    def test_connect_with_access_token(self, mock_server_port):
        """
        Test connecting to mock TDS server using access token authentication.
        
        This test:
        1. Starts a mock TDS server with FedAuth support
        2. Creates a client context with an access token
        3. Connects using mssql-py-core with the token
        4. Verifies the connection succeeds
        5. Verifies the server received the correct token
        """
        import mssql_py_core

        # The mock access token we'll send
        mock_token = "mock_access_token_for_python_integration_test_12345"

        # Create and start the mock server using Python bindings
        # Use tls=True to enable TLS encryption for secure token transmission
        server = mssql_mock_tds_py.PyMockTdsServer(port=mock_server_port, tls=True)
        
        with server:
            # Build client context for the mock server with access token auth
            # Use Optional encryption to enable TLS when available
            client_context = {
                "server": server.sql_address,
                "database": "master",
                "access_token": mock_token,
                "encryption": "Optional",
                "trust_server_certificate": True,
            }

            # Connect using access token authentication
            conn = mssql_py_core.PyCoreConnection(client_context)
            
            # If we got here, the connection was established
            assert conn is not None
            assert conn.is_connected()
            
            # Clean up
            conn.close()
            assert not conn.is_connected()
            
            # Give the server a moment to process the connection info
            import time
            time.sleep(0.1)
            
            # Verify the server received the correct token
            assert server.connection_count() >= 1, "Server should have recorded at least one connection"
            assert server.has_received_token(mock_token), \
                f"Server should have received token '{mock_token}'"
            
            # Also verify via get_last_access_token
            received_token = server.get_last_access_token()
            assert received_token == mock_token, \
                f"Token mismatch: sent '{mock_token}', received '{received_token}'"

    def test_connect_with_unique_access_token(self, mock_server_port):
        """
        Test that unique access tokens are correctly transmitted and received.
        
        This test uses a unique token to verify the exact token is being
        properly sent through the TDS protocol and received by the server.
        """
        import mssql_py_core
        import time

        # Generate a unique token for this test
        unique_token = f"unique_token_{secrets.token_hex(16)}"

        server = mssql_mock_tds_py.PyMockTdsServer(port=mock_server_port, tls=True)
        
        with server:
            client_context = {
                "server": server.sql_address,
                "database": "master",
                "access_token": unique_token,
                "encryption": "Optional",
                "trust_server_certificate": True,
            }

            conn = mssql_py_core.PyCoreConnection(client_context)
            assert conn is not None
            assert conn.is_connected()
            conn.close()
            
            # Wait for connection info to be stored
            time.sleep(0.1)
            
            # Verify the unique token was received
            received_token = server.get_last_access_token()
            assert received_token == unique_token, \
                f"Unique token mismatch: sent '{unique_token}', received '{received_token}'"
            
            # Also check via has_received_token
            assert server.has_received_token(unique_token), \
                f"Server should have received unique token"

    def test_mock_server_starts_successfully(self, mock_server_port):
        """Test that the mock TDS server starts and listens on the expected port."""
        server = mssql_mock_tds_py.PyMockTdsServer(port=mock_server_port)
        
        with server:
            # Verify we can connect at the TCP level
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(5.0)
                s.connect(("127.0.0.1", server.port))
                # Connection succeeded - server is listening
                assert True

    def test_execute_query_with_access_token(self, mock_server_port):
        """
        Test executing a query after connecting with access token.
        
        The mock server supports basic SELECT queries.
        Verifies both the query result and that the token was received.
        """
        import mssql_py_core
        import time

        mock_token = "mock_token_for_query_execution"

        server = mssql_mock_tds_py.PyMockTdsServer(port=mock_server_port, tls=True)
        
        with server:
            client_context = {
                "server": server.sql_address,
                "database": "master",
                "access_token": mock_token,
                "encryption": "Optional",
                "trust_server_certificate": True,
            }

            conn = mssql_py_core.PyCoreConnection(client_context)
            assert conn.is_connected()
            
            # Create a cursor and execute a simple query
            cursor = conn.cursor()
            cursor.execute("SELECT 1")
            
            # Fetch the result
            row = cursor.fetchone()
            assert row is not None
            assert row[0] == 1
            
            # Close cursor first - cursor holds Arc reference to TdsClient
            # which prevents the TCP connection from closing
            cursor.close()
            conn.close()
            
            # Ensure references are dropped so TdsClient is fully released
            del cursor
            del conn
            
            # Wait for connection info to be stored
            time.sleep(0.3)
            
            # Verify token was received
            assert server.has_received_token(mock_token), \
                "Server should have received the access token used for query execution"

    def test_get_all_connections(self, mock_server_port):
        """Test retrieving all connection info from the server."""
        import mssql_py_core
        import time

        token = "test_token_for_connection_list"
        
        server = mssql_mock_tds_py.PyMockTdsServer(port=mock_server_port, tls=True)
        
        with server:
            client_context = {
                "server": server.sql_address,
                "database": "master",
                "access_token": token,
                "encryption": "Optional",
                "trust_server_certificate": True,
            }

            conn = mssql_py_core.PyCoreConnection(client_context)
            conn.close()
            
            time.sleep(0.1)
            
            # Get all connections
            connections = server.get_connections()
            assert len(connections) >= 1, "Should have at least one connection"
            
            # Check the connection info
            conn_info = connections[0]
            assert conn_info.authenticated is True
            assert conn_info.access_token == token

    def test_clear_connections(self, mock_server_port):
        """Test clearing stored connection info."""
        import mssql_py_core
        import time

        server = mssql_mock_tds_py.PyMockTdsServer(port=mock_server_port, tls=True)
        
        with server:
            client_context = {
                "server": server.sql_address,
                "database": "master",
                "access_token": "token_to_clear",
                "encryption": "Optional",
                "trust_server_certificate": True,
            }

            conn = mssql_py_core.PyCoreConnection(client_context)
            conn.close()
            
            time.sleep(0.1)
            
            # Verify we have a connection
            assert server.connection_count() >= 1
            
            # Clear connections
            server.clear_connections()
            
            # Verify cleared
            assert server.connection_count() == 0

    def test_user_agent_format(self, mock_server_port):
        """Test that MS-PYTHON is correctly sent as the driver name in the user agent."""
        import mssql_py_core
        import time

        server = mssql_mock_tds_py.PyMockTdsServer(port=mock_server_port, tls=True)

        with server:
            client_context = {
                "server": server.sql_address,
                "database": "master",
                "encryption": "Optional",
                "trust_server_certificate": True,
            }

            conn = mssql_py_core.PyCoreConnection(client_context)
            conn.close()

            time.sleep(0.1)

            connections = server.get_connections()
            assert len(connections) >= 1, "Should have at least one connection" 
            
            # User Agent Format: 1|DriverName|DriverVersion|Arch|OS|OSDetails|Runtime
            user_agent = connections[0].user_agent
            assert user_agent is not None, "A user agent must be presented to the SQL Server"
            
            # The library_name statically defined in connection.rs must be the second field
            parts = user_agent.split("|")
            assert len(parts) == 7, "User agent must match strict Microsoft format"
            print(f"\n\n---> CAPTURED USER-AGENT STRING:\n{user_agent}\n")
            assert parts[1] == "MS-PYTHON", f"Expected MS-PYTHON but got {parts[1]}"
            assert parts[6].startswith("Python "), f"Expected Python globally cached runtime details, but got {parts[6]}"

if __name__ == "__main__":
    pytest.main([__file__, "-v"])
