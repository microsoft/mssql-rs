// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Startup defaults and per-connection applicability for MultiSubnetFailover.
//!
//! Explicit connection values override the application default, then
//! `MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT`, then the built-in enabled default.
//! Defaults freeze on first use, including a disconnected ODBC attribute read.

use std::ffi::OsString;
use std::sync::Mutex;

use crate::core::TdsResult;
use crate::error::Error;

use super::datasource_parser::ParsedDataSource;

/// Deployment-wide default. Only case-insensitive `true` and `false` are accepted.
pub const DEFAULT_ENVIRONMENT_VARIABLE: &str = "MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT";

/// Where the requested MSF value originated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultiSubnetFailoverSource {
    /// A connection keyword, attribute, or native option.
    Connection,
    /// The application's startup configuration.
    Application,
    /// The deployment environment.
    Environment,
    /// The driver's built-in enabled default.
    BuiltIn,
}

/// Why an inherited MSF default was suppressed for a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultiSubnetFailoverInapplicable {
    /// The selected transport is not a normal TCP connection.
    Transport,
    /// Database mirroring and MSF are mutually exclusive.
    FailoverPartner,
    /// This endpoint needs SQL Browser discovery before a TCP port is known.
    InstanceDiscovery,
    /// An implicit local Windows connection retains its shared-memory-first path.
    LocalServer,
}

/// Effective policy retained for redirects, recovery, and secondary connections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MultiSubnetFailoverPolicy {
    /// Whether the connection uses parallel TCP attempts.
    pub enabled: bool,
    /// Source of the requested value, before applying transport restrictions.
    pub source: MultiSubnetFailoverSource,
    /// An inherited enabled value was suppressed for this reason.
    pub inapplicable: Option<MultiSubnetFailoverInapplicable>,
}

#[derive(Default)]
struct Defaults {
    application: Option<bool>,
    frozen: Option<Result<MultiSubnetFailoverPolicy, &'static str>>,
}

impl Defaults {
    fn configure(&mut self, enabled: bool) -> Result<(), &'static str> {
        if self.frozen.is_some() {
            return Err(
                "MultiSubnetFailover defaults are already frozen; configure them before first use",
            );
        }
        self.application = Some(enabled);
        Ok(())
    }

    fn resolve(
        &mut self,
        environment: impl FnOnce() -> Option<OsString>,
    ) -> Result<MultiSubnetFailoverPolicy, &'static str> {
        if let Some(frozen) = self.frozen {
            return frozen;
        }
        let resolved = (|| {
            let (enabled, source) = if let Some(value) = self.application {
                (value, MultiSubnetFailoverSource::Application)
            } else if let Some(value) = environment() {
                let value = value
                    .to_str()
                    .ok_or("MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT must be true or false")?;
                let enabled = if value.eq_ignore_ascii_case("true") {
                    true
                } else if value.eq_ignore_ascii_case("false") {
                    false
                } else {
                    return Err("MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT must be true or false");
                };
                (enabled, MultiSubnetFailoverSource::Environment)
            } else {
                (true, MultiSubnetFailoverSource::BuiltIn)
            };
            Ok(MultiSubnetFailoverPolicy {
                enabled,
                source,
                inapplicable: None,
            })
        })();
        self.frozen = Some(resolved);
        resolved
    }
}

static DEFAULTS: Mutex<Defaults> = Mutex::new(Defaults {
    application: None,
    frozen: None,
});

fn configuration_error(message: &str) -> Error {
    tracing::error!(message, "Invalid MultiSubnetFailover configuration");
    Error::UsageError(message.to_string())
}

/// Sets this loaded driver's application default before its first use.
///
/// Explicit per-connection values still win. Configuration after defaults
/// have been read fails rather than changing existing pools or connections.
/// Separately loaded native bindings each have their own startup configuration;
/// use the environment default to configure all of them.
pub fn set_default_multi_subnet_failover(enabled: bool) -> TdsResult<()> {
    let mut state = DEFAULTS
        .lock()
        .map_err(|_| configuration_error("MultiSubnetFailover configuration lock is poisoned"))?;
    state.configure(enabled).map_err(configuration_error)
}

/// Returns and freezes the application/deployment default, before applicability.
pub fn default_multi_subnet_failover() -> TdsResult<MultiSubnetFailoverPolicy> {
    let mut state = DEFAULTS
        .lock()
        .map_err(|_| configuration_error("MultiSubnetFailover configuration lock is poisoned"))?;
    state
        .resolve(|| std::env::var_os(DEFAULT_ENVIRONMENT_VARIABLE))
        .map_err(configuration_error)
}

pub(crate) fn resolve_connection_policy(
    explicit: Option<bool>,
    parsed: &ParsedDataSource,
    failover_partner: &str,
) -> TdsResult<MultiSubnetFailoverPolicy> {
    let default = DEFAULTS
        .lock()
        .map_err(|_| configuration_error("MultiSubnetFailover configuration lock is poisoned"))?
        .resolve(|| std::env::var_os(DEFAULT_ENVIRONMENT_VARIABLE));
    resolve_policy(explicit, default, parsed, failover_partner)
}

pub(crate) fn resolve_policy(
    explicit: Option<bool>,
    default: Result<MultiSubnetFailoverPolicy, &'static str>,
    parsed: &ParsedDataSource,
    failover_partner: &str,
) -> TdsResult<MultiSubnetFailoverPolicy> {
    let mut policy = match explicit {
        Some(enabled) => MultiSubnetFailoverPolicy {
            enabled,
            source: MultiSubnetFailoverSource::Connection,
            inapplicable: None,
        },
        None => default.map_err(configuration_error)?,
    };
    if !policy.enabled {
        return Ok(policy);
    }
    let restriction = if !matches!(parsed.protocol_name.as_str(), "" | "tcp") {
        Some((
            MultiSubnetFailoverInapplicable::Transport,
            "MultiSubnetFailover is only supported with TCP connections",
        ))
    } else if !failover_partner.is_empty() {
        Some((
            MultiSubnetFailoverInapplicable::FailoverPartner,
            "MultiSubnetFailover cannot be used with FailoverPartner",
        ))
    } else if parsed.needs_ssrp() {
        Some((
            MultiSubnetFailoverInapplicable::InstanceDiscovery,
            "MultiSubnetFailover requires an explicit TCP port for a named instance",
        ))
    } else {
        None
    };
    if let Some((reason, message)) = restriction {
        if explicit == Some(true) {
            return Err(configuration_error(message));
        }
        policy.enabled = false;
        policy.inapplicable = Some(reason);
    }
    #[cfg(windows)]
    if policy.enabled && explicit.is_none() && parsed.protocol_name.is_empty() && parsed.is_local()
    {
        policy.enabled = false;
        policy.inapplicable = Some(MultiSubnetFailoverInapplicable::LocalServer);
    }
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_defaults_are_frozen_with_connection_precedence() {
        for application in ["unset", "false", "true"] {
            for environment in ["unset", "false", "true", "invalid"] {
                for explicit in ["unset", "false", "true"] {
                    let scenario = format!("{application}/{environment}/{explicit}");
                    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                    child.args([
                        "--exact",
                        "connection::multi_subnet_failover::tests::startup_policy_probe",
                        "--nocapture",
                    ]);
                    child.env("MSSQL_RS_TEST_MSF_POLICY", &scenario);
                    child.env_remove(DEFAULT_ENVIRONMENT_VARIABLE);
                    if environment != "unset" {
                        child.env(DEFAULT_ENVIRONMENT_VARIABLE, environment);
                    }
                    let output = child.output().unwrap();
                    assert!(
                        output.status.success(),
                        "{scenario}: {}\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr),
                    );
                }
            }
        }
    }

    #[test]
    fn startup_policy_probe() {
        use crate::connection::client_context::ClientContext;

        let Ok(scenario) = std::env::var("MSSQL_RS_TEST_MSF_POLICY") else {
            return;
        };
        let parts: Vec<_> = scenario.split('/').collect();
        let application = parts[0].parse::<bool>().ok();
        let environment = parts[1].parse::<bool>().ok();
        let explicit = parts[2].parse::<bool>().ok();
        if let Some(value) = application {
            set_default_multi_subnet_failover(value).unwrap();
        }
        let mut context = ClientContext::with_data_source("db.example.invalid");
        context.multi_subnet_failover = explicit;
        let result = context.prepare_datasource("db.example.invalid");
        if parts[1] == "invalid" && application.is_none() && explicit.is_none() {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains(DEFAULT_ENVIRONMENT_VARIABLE)
            );
        } else {
            let expected = explicit.or(application).or(environment).unwrap_or(true);
            assert_eq!(result.unwrap().parallel_connect, expected);
            let policy = context.multi_subnet_failover_policy().unwrap();
            assert_eq!(policy.enabled, expected);
            let mut secondary = context.clone();
            secondary.prepare_datasource("tcp:secondary,1433").unwrap();
            assert_eq!(secondary.multi_subnet_failover_policy(), Some(policy));
            let machine = hostname::get().unwrap().into_string().unwrap();
            for (alias, tcp_host) in [
                ("localhost", "localhost"),
                ("LOCALHOST", "localhost"),
                (".", "localhost"),
                ("(local)", "localhost"),
                ("(Local)", "localhost"),
                ("(LOCAL)", "localhost"),
                ("127.0.0.1", "127.0.0.1"),
                ("::1", "::1"),
                (machine.as_str(), "localhost"),
            ] {
                let mut local = ClientContext::with_data_source(alias);
                local.multi_subnet_failover = explicit;
                let parsed = local.prepare_datasource(alias).unwrap();
                let local_policy = if cfg!(windows) && explicit.is_none() && expected {
                    MultiSubnetFailoverPolicy {
                        enabled: false,
                        source: policy.source,
                        inapplicable: Some(MultiSubnetFailoverInapplicable::LocalServer),
                    }
                } else {
                    policy
                };
                assert_eq!(parsed.parallel_connect, local_policy.enabled, "{alias}");
                assert_eq!(local.multi_subnet_failover_policy(), Some(local_policy));
                if local_policy.enabled {
                    assert_eq!(parsed.protocol_name, "tcp");
                    assert_eq!(parsed.server_name, tcp_host);
                    assert!(matches!(
                        local.transport_context,
                        crate::connection::client_context::TransportContext::Tcp { ref host, .. }
                            if host == tcp_host
                    ));
                }
                #[cfg(windows)]
                if !local_policy.enabled {
                    use crate::connection::connection_actions::ConnectionAction;
                    let chain = parsed.to_connection_actions(15_000);
                    assert!(
                        chain.actions().iter().any(|action| matches!(
                            action,
                            ConnectionAction::TrySequence { actions, .. } if matches!(
                                actions.as_slice(),
                                [
                                    ConnectionAction::ConnectSharedMemory { .. },
                                    ConnectionAction::ConnectTcp { .. },
                                    ConnectionAction::ConnectNamedPipe { .. }
                                ]
                            )
                        )),
                        "{alias}: {}",
                        chain.describe()
                    );
                }
                let mut secondary = local.clone();
                secondary.prepare_datasource(alias).unwrap();
                assert_eq!(secondary.multi_subnet_failover_policy(), Some(local_policy));
            }
            for target in [
                "tcp:localhost",
                "tcp:(LOCAL)",
                "localhost,1433",
                "127.0.0.1,1433",
            ] {
                let mut local = ClientContext::with_data_source(target);
                local.multi_subnet_failover = explicit;
                let parsed = local.prepare_datasource(target).unwrap();
                assert_eq!(parsed.parallel_connect, expected);
                assert_eq!(local.multi_subnet_failover_policy(), Some(policy));
            }
        }
        assert!(set_default_multi_subnet_failover(false).is_err());
        let default = default_multi_subnet_failover();
        if parts[1] == "invalid" && application.is_none() {
            assert!(default.is_err());
        } else {
            assert_eq!(
                default.unwrap().enabled,
                application.or(environment).unwrap_or(true)
            );
        }
    }

    #[test]
    fn precedence_includes_explicit_false() {
        for application in [None, Some(false), Some(true)] {
            for deployment in [None, Some(false), Some(true)] {
                for explicit in [None, Some(false), Some(true)] {
                    let mut defaults = Defaults {
                        application,
                        frozen: None,
                    };
                    let default = defaults
                        .resolve(|| deployment.map(|v| OsString::from(v.to_string())))
                        .unwrap();
                    let parsed = ParsedDataSource::parse("tcp:db.example.com,1433", false).unwrap();
                    let actual = resolve_policy(explicit, Ok(default), &parsed, "").unwrap();
                    assert_eq!(
                        actual.enabled,
                        explicit.or(application).or(deployment).unwrap_or(true)
                    );
                    let expected_source = if explicit.is_some() {
                        MultiSubnetFailoverSource::Connection
                    } else if application.is_some() {
                        MultiSubnetFailoverSource::Application
                    } else if deployment.is_some() {
                        MultiSubnetFailoverSource::Environment
                    } else {
                        MultiSubnetFailoverSource::BuiltIn
                    };
                    assert_eq!(actual.source, expected_source);
                }
            }
        }
    }

    #[test]
    fn startup_configuration_freezes_and_does_not_reread_environment() {
        let mut defaults = Defaults::default();
        let first = defaults.resolve(|| Some("false".into())).unwrap();
        assert!(!first.enabled);
        assert_eq!(
            defaults
                .resolve(|| panic!("must not reread environment"))
                .unwrap(),
            first
        );
        assert!(defaults.configure(true).is_err());
    }

    #[test]
    fn invalid_environment_is_explicit_and_higher_priority_values_win() {
        for value in ["", "yes", "0", " true ", "invalid"] {
            let mut defaults = Defaults::default();
            assert!(defaults.resolve(|| Some(value.into())).is_err());
            assert!(defaults.resolve(|| None).is_err());
            let parsed = ParsedDataSource::parse("tcp:host,1433", false).unwrap();
            let explicit =
                resolve_policy(Some(false), Err("invalid default"), &parsed, "").unwrap();
            assert!(!explicit.enabled);
        }
        let mut defaults = Defaults::default();
        defaults.configure(true).unwrap();
        assert!(
            defaults
                .resolve(|| panic!("app default overrides environment"))
                .unwrap()
                .enabled
        );
        for value in ["TRUE", "TrUe", "FALSE", "FaLsE"] {
            let mut defaults = Defaults::default();
            assert_eq!(
                defaults.resolve(|| Some(value.into())).unwrap().enabled,
                value.eq_ignore_ascii_case("true")
            );
        }
    }

    #[test]
    fn inherited_defaults_preserve_discovery_and_failover_options() {
        let enabled = Defaults::default().resolve(|| None).unwrap();
        for (target, partner, reason) in [
            (
                "tcp:host\\instance",
                "",
                MultiSubnetFailoverInapplicable::InstanceDiscovery,
            ),
            (
                "tcp:host,1433",
                "partner",
                MultiSubnetFailoverInapplicable::FailoverPartner,
            ),
            ("admin:host", "", MultiSubnetFailoverInapplicable::Transport),
        ] {
            let parsed = ParsedDataSource::parse(target, false).unwrap();
            let policy = resolve_policy(None, Ok(enabled), &parsed, partner).unwrap();
            assert!(!policy.enabled);
            assert_eq!(policy.inapplicable, Some(reason));
            assert!(resolve_policy(Some(true), Ok(enabled), &parsed, partner).is_err());
            assert!(
                !resolve_policy(Some(false), Ok(enabled), &parsed, partner)
                    .unwrap()
                    .enabled
            );
        }
        let parsed = ParsedDataSource::parse("tcp:host\\instance,1433", false).unwrap();
        assert!(
            resolve_policy(None, Ok(enabled), &parsed, "")
                .unwrap()
                .enabled
        );
    }

    #[cfg(windows)]
    #[test]
    fn inherited_defaults_preserve_windows_local_transports() {
        let enabled = Defaults::default().resolve(|| None).unwrap();
        for target in ["np:localhost", "lpc:localhost", "(localdb)\\MSSQLLocalDB"] {
            let parsed = ParsedDataSource::parse(target, false).unwrap();
            let policy = resolve_policy(None, Ok(enabled), &parsed, "").unwrap();
            assert!(!policy.enabled, "{target}");
            assert_eq!(
                policy.inapplicable,
                Some(MultiSubnetFailoverInapplicable::Transport)
            );
            assert!(resolve_policy(Some(true), Ok(enabled), &parsed, "").is_err());
        }
    }
}
