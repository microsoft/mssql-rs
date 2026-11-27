# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Real consumer parsing exercised inside the Rust unit-test executable."""

from unittest import TestCase

from mssql_python.async_query._connection_context import build_async_connection_context
from mssql_python.constants import _ALLOWED_CONNECTION_STRING_PARAMS
from mssql_python.exceptions import ConnectionStringParseError, NotSupportedError

_BASE = [
    ("Server", "tcp:localhost,1433", "server"),
    ("UID", "pool-key-user", "user_name"),
    ("PWD", "dummy-password", "password"),
    ("MultiSubnetFailover", "No", "multi_subnet_failover"),
]


def _connection_string(entries):
    return ";".join(keyword + "={" + value.replace("}", "}}") + "}" for keyword, value in entries)


def _context(keyword=None, value=None, field=None, timeout=15):
    entries = [(key, item) for key, item, name in _BASE if name != field]
    if keyword is not None:
        entries.append((keyword, value))
    return build_async_connection_context(_connection_string(entries), timeout)


def verify_pool_keys(same_key):
    check = TestCase()
    baseline = _context()
    cases = [
        ("Server", "tcp:localhost,1434", "server", True),
        ("Addr", "tcp:localhost,1434", "server", True),
        ("Address", "tcp:localhost,1434", "server", True),
        ("UID", "another-user", "user_name", True),
        ("PWD", "another-password", "password", True),
        ("Authentication", "SqlPassword", "authentication", False),
        ("Trusted_Connection", "No", "trusted_connection", False),
        ("Database", "CaseSensitiveDatabase", "database", True),
        ("APP", "KeyApplication", "application_name", True),
        ("Application Name", "KeyApplication", "application_name", True),
        ("Encrypt", "Optional", "encryption", True),
        ("TrustServerCertificate", "Yes", "trust_server_certificate", True),
        ("Trust_Server_Certificate", "Yes", "trust_server_certificate", True),
        ("HostnameInCertificate", "localhost", "host_name_in_certificate", True),
        ("ServerCertificate", "certificate.pem", "server_certificate", True),
        ("ServerSPN", "MSSQLSvc/localhost:1433", "server_spn", True),
        ("MultiSubnetFailover", "Yes", "multi_subnet_failover", True),
        ("ApplicationIntent", "ReadOnly", "application_intent", True),
        ("ConnectRetryCount", "2", "connect_retry_count", True),
        ("ConnectRetryInterval", "11", "connect_retry_interval", True),
        ("KeepAlive", "31", "keep_alive", True),
        ("KeepAliveInterval", "2", "keep_alive_interval", True),
        ("IPAddressPreference", "IPv6First", "ip_address_preference", True),
        ("Packet Size", "16000", "packet_size", True),
        ("PacketSize", "16000", "packet_size", True),
    ]
    check.assertEqual(
        {keyword.lower() for keyword, *_ in cases},
        set(_ALLOWED_CONNECTION_STRING_PARAMS) - {"driver", "vectortypesupport"},
        "Every accepted keyword spelling needs an explicit key-contract case",
    )
    for keyword, value, field, partitions in cases:
        actual = _context(keyword, value, field)
        check.assertIn(field, actual, keyword)
        expected = value
        if field in {
            "packet_size",
            "connect_retry_count",
            "connect_retry_interval",
            "keep_alive",
            "keep_alive_interval",
        }:
            expected = int(value)
        if field in {"keep_alive", "keep_alive_interval"}:
            expected *= 1000
        if field == "application_name":
            expected = "MSSQL-Python|" + value
        check.assertEqual(actual[field], expected, keyword)
        check.assertEqual(not same_key(baseline, actual), partitions, keyword)
        check.assertTrue(same_key(actual, _context(keyword.upper(), value, field)), keyword)
    check.assertEqual(_context("KeepAlive", "31")["keep_alive"], 31000)
    check.assertEqual(_context("KeepAliveInterval", "2")["keep_alive_interval"], 2000)
    for keyword in ("VectorTypeSupport", "VECTORTYPESUPPORT"):
        for value in ("Yes", "No"):
            with check.assertRaises((ConnectionStringParseError, ValueError)):
                same_key(baseline, _context(keyword, value))

    entries = [(key, value) for key, value, _ in _BASE]
    reversed_context = build_async_connection_context(
        _connection_string(list(reversed(entries))), 15
    )
    check.assertTrue(same_key(baseline, reversed_context))
    check.assertTrue(
        same_key(
            baseline,
            build_async_connection_context(
                "Server=tcp:localhost,1433;UID=pool-key-user;"
                "PWD=dummy-password;MultiSubnetFailover=No",
                15,
            ),
        )
    )
    for alias in ("Addr", "Address"):
        check.assertTrue(same_key(baseline, _context(alias, "tcp:localhost,1433", "server")))
    check.assertTrue(
        same_key(
            _context("APP", "ExactApplication"),
            _context("Application Name", "ExactApplication"),
        )
    )
    check.assertTrue(
        same_key(
            _context("PacketSize", "16000"),
            _context("Packet Size", "16000"),
        )
    )
    for keyword, field in (
        ("UID", "user_name"),
        ("PWD", "password"),
        ("Database", "database"),
        ("APP", "application_name"),
    ):
        for value in ("Case", "case", " a;b=c|d:0} ", "\u00e9", "e\u0301", "\U0001f600"):
            actual = _context(keyword, value, field)
            expected = dict(baseline)
            expected[field] = "MSSQL-Python|" + value if field == "application_name" else value
            check.assertTrue(same_key(actual, expected), keyword)
        check.assertFalse(
            same_key(_context(keyword, "Case", field), _context(keyword, "case", field)),
            keyword,
        )

    first_wins = [
        ("Addr", "tcp:localhost,1433"),
        ("Server", "tcp:localhost,1434"),
        ("UID", "pool-key-user"),
        ("PWD", "dummy-password"),
        ("MultiSubnetFailover", "No"),
    ]
    check.assertTrue(
        same_key(
            baseline,
            build_async_connection_context(_connection_string(first_wins), 15),
        )
    )
    for invalid in (
        entries + [("Server", "tcp:localhost,1434")],
        entries + [("SERVER", "tcp:localhost,1434")],
        entries + [("Database", "")],
        [(key, value) for key, value in entries if key != "PWD"] + [("PWD", "")],
        entries + [("Driver", "customer-driver")],
        entries + [("UnknownPoolOption", "value")],
    ):
        with check.assertRaises((ConnectionStringParseError, ValueError)):
            build_async_connection_context(_connection_string(invalid), 15)
    with check.assertRaises(NotSupportedError):
        _context("Authentication", "ActiveDirectoryDefault")
    with check.assertRaises(ValueError):
        same_key(baseline, _context("Trusted_Connection", "Yes"))
    for requested, effective in (("0", "8000"), ("1", "512"), ("32769", "32768")):
        check.assertTrue(
            same_key(_context("PacketSize", requested), _context("PacketSize", effective))
        )
    check.assertTrue(
        same_key(
            _context("IPAddressPreference", "unknown"),
            _context("IPAddressPreference", "IPv4First"),
        )
    )
    check.assertTrue(same_key(_context(timeout=0), _context(timeout=15)))
    check.assertFalse(same_key(baseline, _context(timeout=16)))
