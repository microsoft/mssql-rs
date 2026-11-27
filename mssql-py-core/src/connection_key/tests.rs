// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    collections::{BTreeSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    process::Command,
};

use super::*;
use pyo3::types::IntoPyDict;

fn options(py: Python<'_>) -> Bound<'_, PyDict> {
    [
        ("server", "tcp:localhost,1433"),
        ("user_name", "pool-key-user"),
        ("password", "dummy-password"),
        ("multi_subnet_failover", "No"),
    ]
    .into_py_dict(py)
    .unwrap()
}

fn prepared(options: &Bound<'_, PyDict>) -> PreparedSqlConnection {
    prepare_sql_connection(options).unwrap()
}

#[test]
#[ignore = "Requires an installed mssql-python parser and its native bindings"]
fn python_parser_builds_compatible_keys() {
    use pyo3::types::{PyCFunction, PyModule, PyTuple};
    use std::ffi::CString;

    Python::attach(|py| {
        let compare = PyCFunction::new_closure(
            py,
            None,
            None,
            |args: &Bound<'_, PyTuple>, _| -> PyResult<bool> {
                let left = args.get_item(0)?;
                let right = args.get_item(1)?;
                Ok(prepare_sql_connection(left.cast::<PyDict>()?)?.key
                    == prepare_sql_connection(right.cast::<PyDict>()?)?.key)
            },
        )
        .unwrap();
        let code = CString::new(include_str!("parser_tests.py")).unwrap();
        let module = PyModule::from_code(
            py,
            &code,
            c"pool_key_parser_tests.py",
            c"pool_key_parser_tests",
        )
        .unwrap();
        module
            .getattr("verify_pool_keys")
            .unwrap()
            .call1((compare,))
            .unwrap();
    });
}

#[test]
fn every_accepted_option_has_a_partition_or_equivalence_case() {
    Python::attach(|py| {
        let cases = [
            ("server", "tcp:localhost,1434", true),
            ("user_name", "another-user", true),
            ("password", "another-password", true),
            ("database", "CaseSensitiveDatabase", true),
            ("application_name", "CaseSensitiveApplication", true),
            ("authentication", "SqlPassword", false),
            ("trusted_connection", "No", false),
            ("encryption", "Optional", true),
            ("trust_server_certificate", "Yes", true),
            ("host_name_in_certificate", "localhost", true),
            ("server_certificate", "certificate.pem", true),
            ("server_spn", "MSSQLSvc/localhost:1433", true),
            ("multi_subnet_failover", "Yes", true),
            ("application_intent", "ReadOnly", true),
            ("ip_address_preference", "IPv6First", true),
        ];
        let integer_cases = [
            ("packet_size", 16000),
            ("connect_retry_count", 2),
            ("connect_retry_interval", 11),
            ("keep_alive", 31000),
            ("keep_alive_interval", 2000),
            ("connect_timeout", 16),
        ];
        let expected = STRING_OPTIONS
            .iter()
            .chain(INTEGER_OPTIONS)
            .copied()
            .collect::<BTreeSet<_>>();
        let covered = cases
            .iter()
            .map(|case| case.0)
            .chain(integer_cases.iter().map(|case| case.0))
            .collect::<BTreeSet<_>>();
        assert_eq!(covered, expected);

        let baseline = prepared(&options(py));
        for (field, value, partitions) in cases {
            let input = options(py);
            input.set_item(field, value).unwrap();
            let changed = prepared(&input);
            assert_eq!(changed.key != baseline.key, partitions, "{field}");
        }
        for (field, value) in integer_cases {
            let input = options(py);
            input.set_item(field, value).unwrap();
            assert_ne!(prepared(&input).key, baseline.key, "{field}");
        }
    });
}

#[test]
fn equivalent_spellings_orderings_and_defaults_have_equal_keys() {
    Python::attach(|py| {
        let baseline = prepared(&options(py));
        for (field, value) in [
            ("authentication", "sQlPaSsWoRd"),
            ("trusted_connection", "nO"),
            ("encryption", "YES"),
            ("encryption", "mandatory"),
            ("trust_server_certificate", "nO"),
            ("application_intent", "readWRITE"),
            ("ip_address_preference", "useplatformdefault"),
            ("application_name", "mssql-python"),
        ] {
            let input = options(py);
            input.set_item(field, value).unwrap();
            assert_eq!(prepared(&input).key, baseline.key, "{field}");
        }
        for (field, value) in [
            ("packet_size", 8000),
            ("connect_retry_count", 1),
            ("connect_retry_interval", 10),
            ("keep_alive", 30000),
            ("keep_alive_interval", 1000),
            ("connect_timeout", 15),
        ] {
            let input = options(py);
            input.set_item(field, value).unwrap();
            assert_eq!(prepared(&input).key, baseline.key, "{field}");
        }
        let reversed = options(py);
        reversed.clear();
        for (field, value) in [
            ("password", "dummy-password"),
            ("multi_subnet_failover", "No"),
            ("user_name", "pool-key-user"),
            ("server", "tcp:localhost,1433"),
        ] {
            reversed.set_item(field, value).unwrap();
        }
        assert_eq!(prepared(&reversed).key, baseline.key);
    });
}

#[test]
fn packet_size_uses_odbc_zero_default_and_clamping() {
    Python::attach(|py| {
        for (requested, effective) in [
            (0_u32, 8000_u16),
            (1, 512),
            (511, 512),
            (512, 512),
            (32768, 32768),
            (32769, 32768),
            (u32::MAX, 32768),
        ] {
            let input = options(py);
            input.set_item("packet_size", requested).unwrap();
            let actual = prepared(&input);
            let explicit = options(py);
            explicit.set_item("packet_size", effective).unwrap();
            assert_eq!(actual.spec.context().packet_size, effective);
            assert_eq!(actual.key, prepared(&explicit).key);
        }
        let input = options(py);
        input.set_item("packet_size", 1).unwrap();
        prepared(&input);
        assert_eq!(
            input
                .get_item("packet_size")
                .unwrap()
                .unwrap()
                .extract::<u32>()
                .unwrap(),
            1
        );
    });
}

#[test]
fn private_preparation_does_not_change_direct_connection_defaults() {
    Python::attach(|py| {
        let input = options(py);
        let direct = PyCoreConnection::dict_to_client_context(&input).unwrap();
        let private = prepared(&input);
        assert_eq!(direct.packet_size, 4096);
        assert_eq!(private.spec.context().packet_size, 8000);
        assert!(direct.multi_subnet_failover_policy().is_none());
        assert_eq!(direct.password, private.spec.context().password);
        assert_eq!(direct.database, private.spec.context().database);
        assert_eq!(
            direct.connect_timeout,
            private.spec.context().connect_timeout
        );
        assert!(!input.contains("packet_size").unwrap());
    });
}

#[test]
fn unknown_ip_preference_uses_odbc_ipv4_fallback() {
    Python::attach(|py| {
        let unknown = options(py);
        unknown
            .set_item("ip_address_preference", "unknown")
            .unwrap();
        let explicit = options(py);
        explicit
            .set_item("ip_address_preference", "IPv4First")
            .unwrap();
        assert_eq!(prepared(&unknown).key, prepared(&explicit).key);
    });
}

#[test]
fn credentials_database_and_application_values_keep_exact_semantics() {
    Python::attach(|py| {
        for field in ["user_name", "password", "database", "application_name"] {
            let mut keys = std::collections::HashSet::new();
            for value in [
                "Case",
                "case",
                "Case ",
                " Case",
                "a;b=c|d:0",
                "{a}};b",
                "\u{00e9}",
                "e\u{0301}",
                "\u{1f600}",
            ] {
                let input = options(py);
                input.set_item(field, value).unwrap();
                assert!(keys.insert(prepared(&input).key), "{field}");
            }
        }
    });
}

#[test]
fn structured_serialization_cannot_confuse_field_boundaries() {
    Python::attach(|py| {
        for (left, right) in [
            (("ab", "c"), ("a", "bc")),
            (("a;b", "c"), ("a", "b;c")),
            (("a|b", "c"), ("a", "b|c")),
            (("a\":[\"b", "c"), ("a", "b\":[\"c")),
            (("", "x"), ("x", "")),
        ] {
            let first = options(py);
            first.set_item("database", left.0).unwrap();
            first.set_item("password", left.1).unwrap();
            let second = options(py);
            second.set_item("database", right.0).unwrap();
            second.set_item("password", right.1).unwrap();
            assert_ne!(prepared(&first).key, prepared(&second).key);
        }
        let missing = options(py);
        let empty = options(py);
        empty.set_item("database", "").unwrap();
        assert_eq!(prepared(&missing).key, prepared(&empty).key);
        empty.set_item("database", "master").unwrap();
        assert_ne!(prepared(&missing).key, prepared(&empty).key);
    });
}

#[test]
fn authentication_binding_changes_only_with_principal_or_password() {
    Python::attach(|py| {
        let baseline = prepared(&options(py));
        for field in ["user_name", "password"] {
            let input = options(py);
            input.set_item(field, "different").unwrap();
            let actual = prepared(&input);
            assert_ne!(actual.key, baseline.key);
            assert_ne!(actual.spec.expected_auth(), baseline.spec.expected_auth());
        }
        let input = options(py);
        input.set_item("database", "another-db").unwrap();
        let actual = prepared(&input);
        assert_ne!(actual.key, baseline.key);
        assert_eq!(actual.spec.expected_auth(), baseline.spec.expected_auth());
    });
}

#[test]
fn inherited_msf_is_resolved_and_retained_in_the_spec() {
    Python::attach(|py| {
        let input = options(py);
        input.del_item("multi_subnet_failover").unwrap();
        let actual = prepared(&input);
        let enabled = actual
            .spec
            .context()
            .multi_subnet_failover_policy()
            .unwrap()
            .enabled;
        input
            .set_item("multi_subnet_failover", if enabled { "Yes" } else { "No" })
            .unwrap();
        assert_eq!(actual.key, prepared(&input).key);
        let mut cloned = actual.spec.context().clone();
        cloned.prepare_for_connect().unwrap();
        assert_eq!(
            cloned.multi_subnet_failover_policy(),
            actual.spec.context().multi_subnet_failover_policy()
        );
    });
}

#[test]
fn unsupported_authentication_and_attributes_fail_closed() {
    Python::attach(|py| {
        for (field, value) in [
            ("trusted_connection", "Yes"),
            ("trusted_connection", "true"),
            ("authentication", "ActiveDirectoryDefault"),
            ("authentication", "ActiveDirectoryPassword"),
            ("authentication", "ActiveDirectoryIntegrated"),
            ("authentication", "ActiveDirectoryServicePrincipal"),
            ("authentication", "SqlPassword;secret-value"),
            ("access_token", "secret-value"),
            ("entra_id_token_factory", "secret-value"),
            ("attrs_before", "secret-value"),
            ("pool_key", "secret-value"),
            ("driver", "secret-value"),
            ("language", "secret-value"),
            ("mars_enabled", "secret-value"),
            ("workstation_id", "secret-value"),
            ("vector_type_support", "Yes"),
            ("vector_type_support", "No"),
            ("unknown-secret-value", "secret-value"),
            ("encryption", "true"),
            ("trust_server_certificate", "true"),
            ("multi_subnet_failover", "true"),
            ("application_intent", "secret-value"),
            ("database", "secret-value\0"),
        ] {
            let input = options(py);
            input.set_item(field, value).unwrap();
            let error = prepare_sql_connection(&input).unwrap_err();
            for diagnostic in [error.to_string(), format!("{error:?}")] {
                assert!(!diagnostic.contains("secret-value"));
            }
        }
    });
}

#[test]
fn invalid_integer_types_and_ranges_are_rejected_without_fallback() {
    Python::attach(|py| {
        for field in INTEGER_OPTIONS {
            for value in [-1_i64, i64::from(u32::MAX) + 1] {
                let input = options(py);
                input.set_item(field, value).unwrap();
                assert!(prepare_sql_connection(&input).is_err(), "{field}");
            }
            let input = options(py);
            input.set_item(field, true).unwrap();
            assert!(prepare_sql_connection(&input).is_err(), "{field}");
            input.set_item(field, "100").unwrap();
            assert!(prepare_sql_connection(&input).is_err(), "{field}");
        }
        for (field, value) in [
            ("connect_retry_count", 256),
            ("connect_retry_interval", 0),
            ("connect_retry_interval", 61),
        ] {
            let input = options(py);
            input.set_item(field, value).unwrap();
            assert!(prepare_sql_connection(&input).is_err(), "{field}");
        }
    });
}

#[test]
fn invalid_string_values_have_redacted_errors() {
    Python::attach(|py| {
        for field in STRING_OPTIONS {
            let input = options(py);
            input.set_item(field, 123).unwrap();
            assert!(prepare_sql_connection(&input).is_err(), "{field}");
            input.set_item(field, "sensitive-value\0").unwrap();
            let error = prepare_sql_connection(&input).unwrap_err();
            assert!(!error.to_string().contains("sensitive-value"), "{field}");
            input
                .set_item(field, py.eval(c"chr(0xd800)", None, None).unwrap())
                .unwrap();
            let error = prepare_sql_connection(&input).unwrap_err();
            assert_eq!(error.to_string(), invalid_options().to_string(), "{field}");
        }
    });
}

#[test]
fn preparation_logs_neither_principal_nor_password() {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };

    struct Writer(Arc<Mutex<Vec<u8>>>);
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&output);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || Writer(Arc::clone(&captured)))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::info!("SQL key redaction capture active");
    Python::attach(|py| {
        let input = options(py);
        input
            .set_item("user_name", "unique-principal-secret")
            .unwrap();
        input
            .set_item("password", "unique-password-secret")
            .unwrap();
        prepared(&input);
        input
            .set_item("encryption", "unique-password-secret")
            .unwrap();
        assert!(prepare_sql_connection(&input).is_err());
    });
    let log = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    assert!(log.contains("SQL key redaction capture active"));
    assert!(!log.contains("unique-principal-secret"));
    assert!(!log.contains("unique-password-secret"));
}

#[test]
fn prepared_values_and_keys_have_redacted_diagnostics() {
    Python::attach(|py| {
        let input = options(py);
        input
            .set_item("password", "unique-password-secret")
            .unwrap();
        let actual = prepared(&input);
        assert_eq!(format!("{actual:?}"), "PreparedSqlConnection([REDACTED])");
        let (key, spec) = actual.into_parts();
        assert_eq!(format!("{key:?}"), "PoolKey([REDACTED])");
        assert_eq!(
            format!("{:?}", spec.expected_auth()),
            "AuthBinding([REDACTED])"
        );
        assert_eq!(spec.context().password, "unique-password-secret");
    });
}

#[test]
fn private_key_material_partitions_both_fingerprints() {
    Python::attach(|py| {
        let context = prepared(&options(py));
        let first = fingerprint(context.spec.context().clone(), &[1; 32]).unwrap();
        let second = fingerprint(context.spec.context().clone(), &[2; 32]).unwrap();
        assert_ne!(first.key, second.key);
        assert_ne!(first.spec.expected_auth(), second.spec.expected_auth());
    });
}

#[test]
#[ignore = "Subprocess helper invoked by process_keys_are_private"]
fn emit_process_key_digest() {
    Python::attach(|py| {
        let mut hasher = DefaultHasher::new();
        prepared(&options(py)).key.hash(&mut hasher);
        println!("PROCESS_KEY_DIGEST={}", hasher.finish());
    });
}

#[test]
fn process_keys_are_private() {
    let child = |threads: &str| {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "connection_key::tests::emit_process_key_digest",
                "--ignored",
                "--nocapture",
                "--test-threads",
                threads,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| {
                line.split_once("PROCESS_KEY_DIGEST=")
                    .map(|(_, digest)| digest)
            })
            .expect("child must report an opaque digest")
            .to_owned()
    };
    assert_ne!(child("1"), child("2"));
}
