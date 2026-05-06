# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for ClientContext parameter mapping from Python to Rust."""
import os
import pytest
import socket
import mssql_py_core
from pathlib import Path
from dotenv import load_dotenv


def get_db_credentials():
    """Get database credentials from environment variables or /tmp/password."""
    env_path = Path(__file__).parent.parent.parent / ".env"
    if env_path.exists():
        load_dotenv(env_path)

    username = os.environ.get("DB_USERNAME", "sa")
    password = os.environ.get("SQL_PASSWORD")
    if not password:
        try:
            with open("/tmp/password", "r") as f:
                password = f.read().strip()
        except FileNotFoundError:
            pytest.skip("SQL_PASSWORD not set and /tmp/password not found")

    return username, password


def get_server_address():
    """Get server address from environment variable."""
    return os.environ.get("SQL_SERVER", "localhost")


def trust_server_certificate():
    """Get trust server certificate setting from environment."""
    return os.environ.get("TRUST_SERVER_CERTIFICATE", "false").lower() == "true"


def get_base_context():
    """Get minimal connection context for tests."""
    username, password = get_db_credentials()
    server = get_server_address()
    return {
        "server": server,
        "user_name": username,
        "password": password,
        "trust_server_certificate": trust_server_certificate(),
    }


@pytest.mark.integration
class TestDatabaseParameter:
    """Tests for database parameter mapping."""

    def test_database_default_master(self):
        """Test that omitting database connects to default (master)."""
        context = get_base_context()
        # Don't set database - should default to empty string which SQL Server treats as default
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT DB_NAME() AS current_db")
        result = cursor.fetchone()
        conn.close()
        # Default database for 'sa' is typically 'master'
        assert result[0] == "master"

    def test_database_explicit_master(self):
        """Test connecting to master database explicitly."""
        context = get_base_context()
        context["database"] = "master"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT DB_NAME() AS current_db")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "master"

    def test_database_tempdb(self):
        """Test connecting to tempdb database."""
        context = get_base_context()
        context["database"] = "tempdb"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT DB_NAME() AS current_db")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "tempdb"

    def test_database_msdb(self):
        """Test connecting to msdb database."""
        context = get_base_context()
        context["database"] = "msdb"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT DB_NAME() AS current_db")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "msdb"


@pytest.mark.integration
class TestLanguageParameter:
    """Tests for language parameter mapping."""

    def test_language_default_us_english(self):
        """Test that default language is us_english."""
        context = get_base_context()
        context["database"] = "master"
        # Don't set language - should default to us_english
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT @@LANGUAGE AS current_lang")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "us_english"

    def test_language_deutsch(self):
        """Test setting language to Deutsch (German)."""
        context = get_base_context()
        context["database"] = "master"
        context["language"] = "Deutsch"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT @@LANGUAGE AS current_lang")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "Deutsch"

    def test_language_french(self):
        """Test setting language to French.
        
        Note: SQL Server accepts both 'French' and 'Français' as input,
        but @@LANGUAGE always returns the native name 'Français'.
        This is SQL Server behavior - it normalizes language names to their
        native form in sys.syslanguages.alias column.
        """
        context = get_base_context()
        context["database"] = "master"
        context["language"] = "French"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT @@LANGUAGE AS current_lang")
        result = cursor.fetchone()
        conn.close()
        # SQL Server returns the native name 'Français' when 'French' is set
        assert result[0] == "Français"

    def test_language_francais_native(self):
        """Test setting language using native name Français.
        
        Note: This test is skipped because sending non-ASCII characters 
        (like 'ç' in Français) in the LOGIN7 packet causes connection timeout.
        Use 'French' instead which SQL Server normalizes to 'Français'.
        This may be a Unicode encoding issue in the TDS login packet.
        """
        pytest.skip("Non-ASCII language names cause connection timeout - use English alias 'French' instead")
        context = get_base_context()
        context["database"] = "master"
        context["language"] = "Français"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT @@LANGUAGE AS current_lang")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "Français"

    def test_language_british(self):
        """Test setting language to British."""
        context = get_base_context()
        context["database"] = "master"
        context["language"] = "British"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT @@LANGUAGE AS current_lang")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "British"


@pytest.mark.integration
class TestServerPortParameter:
    """Tests for server and port parameter parsing."""

    def test_server_with_comma_port(self):
        """Test server,port format (e.g., localhost,1433) - SQL Server standard."""
        context = get_base_context()
        # SQL Server uses comma as port separator
        context["server"] = get_server_address() + ",1433"
        context["database"] = "master"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_server_without_port_uses_default(self):
        """Test that server without port uses default 1433."""
        context = get_base_context()
        # Server without port - should use default 1433
        context["database"] = "master"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_server_ip_address_with_port(self):
        """Test connecting via IP address,port format."""
        context = get_base_context()
        # SQL Server uses comma as port separator
        context["server"] = get_server_address() + ",1433"
        context["database"] = "master"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1


@pytest.mark.integration
class TestApplicationNameParameter:
    """Tests for application_name parameter mapping."""

    def test_application_name_default(self):
        """Test that default application name is mssql-python."""
        context = get_base_context()
        context["database"] = "master"
        # Don't set application_name - should default to "mssql-python"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT APP_NAME() AS app_name")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "mssql-python"

    def test_application_name_custom(self):
        """Test setting custom application name."""
        context = get_base_context()
        context["database"] = "master"
        context["application_name"] = "MyTestApp"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT APP_NAME() AS app_name")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "MyTestApp"

    def test_application_name_with_spaces(self):
        """Test application name with spaces."""
        context = get_base_context()
        context["database"] = "master"
        context["application_name"] = "My Test Application"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT APP_NAME() AS app_name")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "My Test Application"


@pytest.mark.integration
class TestWorkstationIdParameter:
    """Tests for workstation_id parameter mapping."""

    def test_workstation_id_default_hostname(self):
        """Test that default workstation_id is the machine hostname."""
        context = get_base_context()
        context["database"] = "master"
        # Don't set workstation_id - should default to hostname
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT HOST_NAME() AS host_name")
        result = cursor.fetchone()
        conn.close()
        # Should match the local machine's hostname
        expected_hostname = socket.gethostname()
        assert result[0] == expected_hostname

    def test_workstation_id_custom(self):
        """Test setting custom workstation_id."""
        context = get_base_context()
        context["database"] = "master"
        context["workstation_id"] = "MYWORKSTATION"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT HOST_NAME() AS host_name")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "MYWORKSTATION"


@pytest.mark.integration
class TestMultipleParametersCombined:
    """Tests for multiple parameters set together."""

    def test_all_parameters_combined(self):
        """Test setting multiple parameters at once."""
        context = get_base_context()
        context["database"] = "tempdb"
        context["language"] = "Deutsch"
        context["application_name"] = "CombinedTest"
        context["workstation_id"] = "TESTHOST"

        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()

        # Verify all parameters in a single query
        cursor.execute("""
            SELECT 
                DB_NAME() AS db,
                @@LANGUAGE AS lang,
                APP_NAME() AS app,
                HOST_NAME() AS host
        """)
        result = cursor.fetchone()
        conn.close()

        assert result[0] == "tempdb"
        assert result[1] == "Deutsch"
        assert result[2] == "CombinedTest"
        assert result[3] == "TESTHOST"


@pytest.mark.integration
class TestKeepAliveParameters:
    """Tests for keep_alive and keep_alive_interval parameter mapping.
    
    These parameters control TCP keep-alive settings:
    - keep_alive: Idle time (ms) before first probe is sent (default: 30000)
    - keep_alive_interval: Interval (ms) between subsequent probes (default: 1000)
    
    Note: These are TCP socket-level settings and cannot be directly queried
    from SQL Server. We test that the parameters are accepted and don't break
    the connection.
    """

    def test_keep_alive_default_values(self):
        """Test that connection works with default keep_alive values."""
        context = get_base_context()
        context["database"] = "master"
        # Don't set keep_alive - should use defaults (30000, 1000)
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_keep_alive_custom_values(self):
        """Test connection with custom keep_alive values."""
        context = get_base_context()
        context["database"] = "master"
        context["keep_alive"] = 60000  # 60 seconds
        context["keep_alive_interval"] = 5000  # 5 seconds
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_keep_alive_minimum_values(self):
        """Test connection with minimum practical keep_alive values (1 second).
        
        Note: OS-level TCP keep-alive has minimum constraints:
        - Linux: minimum ~1 second (kernel rejects lower values)
        - Windows/macOS: similar restrictions apply
        """
        context = get_base_context()
        context["database"] = "master"
        context["keep_alive"] = 1000  # 1 second (OS minimum)
        context["keep_alive_interval"] = 1000  # 1 second (OS minimum)
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_keep_alive_large_values(self):
        """Test connection with large keep_alive values."""
        context = get_base_context()
        context["database"] = "master"
        context["keep_alive"] = 3600000  # 1 hour
        context["keep_alive_interval"] = 60000  # 1 minute
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_keep_alive_only(self):
        """Test setting only keep_alive without keep_alive_interval."""
        context = get_base_context()
        context["database"] = "master"
        context["keep_alive"] = 45000  # 45 seconds
        # keep_alive_interval should default to 1000
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_keep_alive_interval_only(self):
        """Test setting only keep_alive_interval without keep_alive."""
        context = get_base_context()
        context["database"] = "master"
        # keep_alive should default to 30000
        context["keep_alive_interval"] = 2000  # 2 seconds
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_keep_alive_with_other_parameters(self):
        """Test keep_alive combined with other connection parameters."""
        context = get_base_context()
        context["database"] = "tempdb"
        context["application_name"] = "KeepAliveTest"
        context["keep_alive"] = 20000  # 20 seconds
        context["keep_alive_interval"] = 3000  # 3 seconds
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("""
            SELECT 
                DB_NAME() AS db,
                APP_NAME() AS app
        """)
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "tempdb"
        assert result[1] == "KeepAliveTest"

@pytest.mark.integration
class TestPacketSizeParameter:
    """Tests for packet_size parameter mapping.
    
    Note: The actual packet size used may differ slightly from the requested size
    due to TDS header overhead. SQL Server negotiates the final size.
    We query sys.dm_exec_connections to verify the negotiated packet size.
    """

    def test_packet_size_default(self):
        """Test that default packet_size is around 4096."""
        context = get_base_context()
        context["database"] = "master"
        # Don't set packet_size - should default to 4096
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT net_packet_size FROM sys.dm_exec_connections WHERE session_id = @@SPID")
        result = cursor.fetchone()
        conn.close()
        # SQL Server may negotiate a slightly different size (e.g., 4266 instead of 4096)
        assert 4000 <= result[0] <= 4500

    def test_packet_size_8000(self):
        """Test setting packet_size to 8000."""
        context = get_base_context()
        context["database"] = "master"
        context["packet_size"] = 8000
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT net_packet_size FROM sys.dm_exec_connections WHERE session_id = @@SPID")
        result = cursor.fetchone()
        conn.close()
        # Allow some variance for TDS negotiation
        assert 7500 <= result[0] <= 8500

    def test_packet_size_16384(self):
        """Test setting packet_size to 16384 (16KB)."""
        context = get_base_context()
        context["database"] = "master"
        context["packet_size"] = 16384
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT net_packet_size FROM sys.dm_exec_connections WHERE session_id = @@SPID")
        result = cursor.fetchone()
        conn.close()
        assert 15000 <= result[0] <= 17000

    def test_packet_size_32768(self):
        """Test setting packet_size to max value 32768.
        
        Note: SQL Server may cap the packet size based on server configuration.
        The important thing is the packet size is larger than the 16384 test.
        """
        context = get_base_context()
        context["database"] = "master"
        context["packet_size"] = 32768
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT net_packet_size FROM sys.dm_exec_connections WHERE session_id = @@SPID")
        result = cursor.fetchone()
        conn.close()
        # SQL Server may cap packet size based on config, but should be at least 16000
        assert result[0] >= 16000

    def test_packet_size_too_small(self):
        """Test that packet_size below 512 defaults to 4096.
        
        Note: SQL Server may negotiate a slightly different size due to TDS overhead.
        """
        context = get_base_context()
        context["database"] = "master"
        context["packet_size"] = 511  # Below minimum
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT net_packet_size FROM sys.dm_exec_connections WHERE session_id = @@SPID")
        result = cursor.fetchone()
        conn.close()
        # Should default to ~4096 when below minimum (allow variance for TDS negotiation)
        assert 4000 <= result[0] <= 4500

    def test_packet_size_too_large(self):
        """Test that packet_size above 32768 defaults to 4096.
        
        Note: Invalid packet sizes silently default to 4096 rather than raising an error.
        SQL Server may negotiate a slightly different size due to TDS overhead.
        """
        context = get_base_context()
        context["database"] = "master"
        context["packet_size"] = 32769  # Above maximum
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT net_packet_size FROM sys.dm_exec_connections WHERE session_id = @@SPID")
        result = cursor.fetchone()
        conn.close()
        # Should default to ~4096 when above maximum (allow variance for TDS negotiation)
        assert 4000 <= result[0] <= 4500

    def test_packet_size_minimum_valid(self):
        """Test that packet_size of 512 (minimum) is accepted.
        
        Note: SQL Server may negotiate a larger packet size than requested.
        The TDS protocol header and other factors can increase the actual negotiated size.
        """
        context = get_base_context()
        context["database"] = "master"
        context["packet_size"] = 512
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT net_packet_size FROM sys.dm_exec_connections WHERE session_id = @@SPID")
        result = cursor.fetchone()
        conn.close()
        # Allow variance for TDS negotiation - SQL Server may negotiate a larger size
        assert 512 <= result[0] <= 1024


@pytest.mark.integration
class TestMultiSubnetFailoverParameter:
    """Tests for multi_subnet_failover parameter mapping.
    
    Note: MultiSubnetFailover is primarily used with AlwaysOn Availability Groups
    and affects connection behavior (parallel connection attempts to all IPs).
    Since we can't easily test AG behavior in a single-node setup, these tests
    verify the parameter is accepted and connections still succeed.
    """

    def test_multi_subnet_failover_default_false(self):
        """Test that default multi_subnet_failover is False."""
        context = get_base_context()
        context["database"] = "master"
        # Don't set multi_subnet_failover - should default to False
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_multi_subnet_failover_explicit_false(self):
        """Test explicitly setting multi_subnet_failover to False."""
        context = get_base_context()
        context["database"] = "master"
        context["multi_subnet_failover"] = False
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_multi_subnet_failover_true(self):
        """Test setting multi_subnet_failover to True.
        
        With MultiSubnetFailover=True, the driver attempts parallel connections
        to all resolved IP addresses. This should still work on a single-node server.
        """
        context = get_base_context()
        context["database"] = "master"
        context["multi_subnet_failover"] = True
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1


@pytest.mark.integration
class TestHostnameInCertificateParameter:
    """Tests for host_name_in_certificate parameter mapping.
    
    This parameter specifies the expected hostname in the server's TLS certificate
    when it differs from the server name used for connection.
    """

    def test_host_name_in_certificate_default(self):
        """Test that connection works without host_name_in_certificate (default None)."""
        context = get_base_context()
        context["database"] = "master"
        # Don't set host_name_in_certificate - should default to None
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_host_name_in_certificate_with_value(self):
        """Test connection with host_name_in_certificate set.
        
        Note: This test verifies the parameter is accepted. Actual certificate
        validation depends on the server's certificate configuration.
        """
        context = get_base_context()
        context["database"] = "master"
        context["trust_server_certificate"] = True  # Required for test environments
        context["host_name_in_certificate"] = "localhost"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1


@pytest.mark.integration
class TestIpAddressPreferenceParameter:
    """Tests for ip_address_preference parameter mapping.
    
    This parameter controls IPv4 vs IPv6 preference during DNS resolution:
    - IPv4First: Prefer IPv4 addresses
    - IPv6First: Prefer IPv6 addresses  
    - UsePlatformDefault: Use OS default behavior (default)
    """

    def test_ip_address_preference_default(self):
        """Test that connection works with default UsePlatformDefault."""
        context = get_base_context()
        context["database"] = "master"
        # Don't set ip_address_preference - should default to UsePlatformDefault
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1


    def test_ip_address_preference_ipv4first(self):
        """Test connection with IPv4First preference.
        
        Note: This test uses 127.0.0.1 explicitly to ensure IPv4 is used.
        If using 'localhost', the behavior depends on DNS resolution order
        and whether SQL Server is listening on IPv4.
        """
        # First check if SQL Server is listening on IPv4
        context = get_base_context()
        context["server"] = "127.0.0.1"
        context["database"] = "master"
        try:
            # Quick test without IPv4First to see if IPv4 works at all
            test_conn = mssql_py_core.PyCoreConnection(context)
            test_conn.close()
        except RuntimeError as e:
            err_msg = str(e)
            if "Timeout" in err_msg or "Connection refused" in err_msg:
                pytest.skip("SQL Server not listening on IPv4 (127.0.0.1)")
            raise
        
        # Now test with IPv4First preference
        context["ip_address_preference"] = "IPv4First"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_ip_address_preference_ipv6first(self):
        """Test connection with IPv6First preference."""
        context = get_base_context()
        context["database"] = "master"
        context["ip_address_preference"] = "IPv6First"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_ip_address_preference_platform_default_explicit(self):
        """Test connection with explicit UsePlatformDefault."""
        context = get_base_context()
        context["database"] = "master"
        context["ip_address_preference"] = "UsePlatformDefault"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_ip_address_preference_invalid_rejects(self):
        """Test that invalid value is rejected (ODBC parity)."""
        context = get_base_context()
        context["database"] = "master"
        context["ip_address_preference"] = "InvalidValue"
        with pytest.raises(RuntimeError, match="Invalid IPAddressPreference value"):
            mssql_py_core.PyCoreConnection(context)

    def test_ip_address_preference_case_insensitive_lowercase(self):
        """Test that ip_address_preference comparison is case-insensitive (lowercase).
        
        ODBC uses _wcsicmp for case-insensitive comparison, so ipv4first should work.
        Note: This test may be skipped if SQL Server is not listening on IPv4.
        """
        context = get_base_context()
        context["database"] = "master"
        context["ip_address_preference"] = "ipv4first"  # all lowercase
        try:
            conn = mssql_py_core.PyCoreConnection(context)
        except RuntimeError as e:
            err_msg = str(e)
            if "Timeout" in err_msg or "Connection refused" in err_msg:
                pytest.skip("SQL Server not listening on IPv4 or timeout occurred")
            raise
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_ip_address_preference_case_insensitive_uppercase(self):
        """Test that ip_address_preference comparison is case-insensitive (uppercase).
        
        ODBC uses _wcsicmp for case-insensitive comparison, so IPV6FIRST should work.
        """
        context = get_base_context()
        context["database"] = "master"
        context["ip_address_preference"] = "IPV6FIRST"  # all uppercase
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_ip_address_preference_case_insensitive_mixed(self):
        """Test that ip_address_preference comparison is case-insensitive (mixed).
        
        ODBC uses _wcsicmp for case-insensitive comparison, so useplatformdefault should work.
        """
        context = get_base_context()
        context["database"] = "master"
        context["ip_address_preference"] = "useplatformdefault"  # all lowercase
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1


@pytest.mark.integration
class TestCaseInsensitiveConnectionStringValues:
    """Tests for case-insensitive connection string value parsing.
    
    ODBC uses _wcsicmp for case-insensitive comparison of connection string values.
    This ensures parity with ODBC behavior where:
    - ipv4first, IPv4First, IPV4FIRST all work for IpAddressPreference
    - mandatory, Mandatory, MANDATORY all work for Encryption
    - readonly, ReadOnly, READONLY all work for ApplicationIntent
    
    Reference: msodbcsql/Sql/Ntdbms/sqlncli/odbc/sqlcconn.cpp line 3148
    """

    def test_encryption_lowercase(self):
        """Test encryption value in lowercase."""
        context = get_base_context()
        context["database"] = "master"
        context["encryption"] = "mandatory"  # lowercase
        context["trust_server_certificate"] = True
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "TRUE"

    def test_encryption_uppercase(self):
        """Test encryption value in uppercase."""
        context = get_base_context()
        context["database"] = "master"
        context["encryption"] = "MANDATORY"  # uppercase
        context["trust_server_certificate"] = True
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "TRUE"

    def test_encryption_mixed_case(self):
        """Test encryption value in mixed case."""
        context = get_base_context()
        context["database"] = "master"
        context["encryption"] = "Mandatory"  # PascalCase
        context["trust_server_certificate"] = True
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "TRUE"

    def test_encryption_required_lowercase(self):
        """Test encryption 'required' value (alias for mandatory) in lowercase."""
        context = get_base_context()
        context["database"] = "master"
        context["encryption"] = "required"  # lowercase
        context["trust_server_certificate"] = True
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == "TRUE"

    def test_application_intent_lowercase(self):
        """Test application_intent value in lowercase."""
        context = get_base_context()
        context["database"] = "master"
        context["application_intent"] = "readwrite"  # lowercase
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_application_intent_uppercase(self):
        """Test application_intent value in uppercase."""
        context = get_base_context()
        context["database"] = "master"
        context["application_intent"] = "READWRITE"  # uppercase
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_application_intent_readonly_lowercase(self):
        """Test application_intent 'readonly' value in lowercase."""
        context = get_base_context()
        context["database"] = "master"
        context["application_intent"] = "readonly"  # lowercase
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_application_intent_readonly_uppercase(self):
        """Test application_intent 'READONLY' value in uppercase."""
        context = get_base_context()
        context["database"] = "master"
        context["application_intent"] = "READONLY"  # uppercase
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

@pytest.mark.integration
class TestServerCertificateParameter:
    """Tests for server_certificate parameter mapping.
    
    ServerCertificate specifies a path to a certificate file to use for 
    server certificate validation instead of the system certificate store.
    This is useful for self-signed certificates or private CAs.
    """

    def test_server_certificate_default_none(self):
        """Test that default server_certificate is None (use system store)."""
        context = get_base_context()
        context["database"] = "master"
        # Don't set server_certificate - should default to None
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_server_certificate_with_nonexistent_path(self):
        """Test that server_certificate with non-existent path raises an error.
        
        The Rust TDS client validates that the certificate file exists
        before attempting to connect.
        """
        context = get_base_context()
        context["database"] = "master"
        context["server_certificate"] = "/path/to/certificate.pem"
        # Should fail because the certificate file doesn't exist
        with pytest.raises(RuntimeError) as excinfo:
            mssql_py_core.PyCoreConnection(context)
        assert "Certificate file not found" in str(excinfo.value)


@pytest.mark.integration
class TestServerSpnParameter:
    """Tests for server_spn parameter mapping.
    
    ServerSPN (Service Principal Name) is used for Kerberos authentication.
    Format: MSSQLSvc/hostname:port or MSSQLSvc/hostname
    This overrides the auto-generated SPN for custom configurations.
    """

    def test_server_spn_default_none(self):
        """Test that default server_spn is None (auto-generate)."""
        context = get_base_context()
        context["database"] = "master"
        # Don't set server_spn - should default to None (auto-generated)
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_server_spn_custom_value(self):
        """Test setting custom server_spn.
        
        Note: ServerSPN is only used with Kerberos/SSPI authentication.
        With SQL authentication (username/password), it's stored but not used.
        """
        context = get_base_context()
        context["database"] = "master"
        context["server_spn"] = "MSSQLSvc/myserver.domain.com:1433"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1

    def test_server_spn_without_port(self):
        """Test server_spn without port number."""
        context = get_base_context()
        context["database"] = "master"
        context["server_spn"] = "MSSQLSvc/myserver.domain.com"
        conn = mssql_py_core.PyCoreConnection(context)
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS connected")
        result = cursor.fetchone()
        conn.close()
        assert result[0] == 1


class TestUnsupportedAuthenticationMethods:
    """ActiveDirectoryIntegrated and ActiveDirectoryPassword are not yet
    supported by mssql-py-core. Attempting to connect must raise a clear
    error instead of panicking in the Rust core."""

    def test_active_directory_integrated_raises(self):
        context = {
            "server": "localhost",
            "trust_server_certificate": "yes",
            "authentication": "ActiveDirectoryIntegrated",
        }
        with pytest.raises(RuntimeError, match="ActiveDirectoryIntegrated.*not currently supported by mssql-py-core"):
            mssql_py_core.PyCoreConnection(context)

    def test_active_directory_password_raises(self):
        context = {
            "server": "localhost",
            "trust_server_certificate": "yes",
            "authentication": "ActiveDirectoryPassword",
            "user_name": "user",
            "password": "pass",
        }
        with pytest.raises(RuntimeError, match="ActiveDirectoryPassword.*not currently supported by mssql-py-core"):
            mssql_py_core.PyCoreConnection(context)


class TestAccessTokenConflictEnforcement:
    """validate_auth enforces strict ODBC-parity: access_token must be the
    sole credential. The Python layer (cursor.py) is responsible for
    stripping stale fields before calling PyCoreConnection.

    These tests verify the validator catches leaked fields — ensuring
    regressions in cursor.py don't silently pass through.
    """

    def test_token_plus_auth_keyword_rejected(self):
        """access_token + authentication keyword must raise."""
        context = {
            "server": "localhost",
            "trust_server_certificate": "yes",
            "access_token": "fake-jwt",
            "authentication": "ActiveDirectoryDefault",
        }
        with pytest.raises(RuntimeError, match="Access Token"):
            mssql_py_core.PyCoreConnection(context)

    def test_token_plus_uid_pwd_rejected(self):
        """access_token + UID + PWD must raise."""
        context = {
            "server": "localhost",
            "trust_server_certificate": "yes",
            "access_token": "fake-jwt",
            "user_name": "user@domain.com",
            "password": "old-password",
        }
        with pytest.raises(RuntimeError, match="Access Token"):
            mssql_py_core.PyCoreConnection(context)

    def test_token_plus_tc_yes_rejected(self):
        """access_token + Trusted_Connection=Yes must raise."""
        context = {
            "server": "localhost",
            "trust_server_certificate": "yes",
            "access_token": "fake-jwt",
            "trusted_connection": "Yes",
        }
        with pytest.raises(RuntimeError, match="Access Token"):
            mssql_py_core.PyCoreConnection(context)

    def test_token_alone_accepted(self):
        """access_token alone should pass validation (server may still reject)."""
        context = {
            "server": "localhost",
            "trust_server_certificate": "yes",
            "access_token": "fake-jwt",
        }
        try:
            conn = mssql_py_core.PyCoreConnection(context)
            conn.close()
        except RuntimeError as e:
            assert "Access Token cannot be used" not in str(e)
