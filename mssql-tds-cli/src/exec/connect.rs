// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Turning command-line options into a driver connection.

use mssql_tds::connection::client_context::{
    ClientContext, ColumnEncryptionSetting, TdsAuthenticationMethod,
};
use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{CancelHandle, EncryptionOptions, EncryptionSetting, TdsResult};
use mssql_tds::message::login_options::ApplicationIntent;

use crate::cli::validate::{Encrypt, Options};

/// The application name the server sees in `program_name()`.
const APPLICATION_NAME: &str = "SQLCMD";

const DEFAULT_PORT: u16 = 1433;

/// A parsed `-S` value.
#[derive(Debug, PartialEq, Eq)]
pub struct DataSource {
    pub host: String,
    pub port: Option<u16>,
    pub instance: Option<String>,
    /// `admin:` prefix, the other spelling of `-A`.
    pub dedicated_admin: bool,
}

impl DataSource {
    /// Accepts the forms the reference does: `host`, `host\instance`,
    /// `host,port`, `tcp:host,port`, `admin:host`, `(local)` and `.`.
    ///
    /// The `admin:` prefix is recognised and reported, but neither it nor `-A`
    /// actually opens a dedicated admin connection yet.
    pub fn parse(value: &str) -> Self {
        let mut rest = value.trim();
        let mut dedicated_admin = false;

        loop {
            let lowered = rest.to_ascii_lowercase();
            if let Some(stripped) = lowered.strip_prefix("admin:") {
                dedicated_admin = true;
                rest = &rest[rest.len() - stripped.len()..];
            } else if let Some(stripped) = lowered
                .strip_prefix("tcp:")
                .or_else(|| lowered.strip_prefix("np:"))
                .or_else(|| lowered.strip_prefix("lpc:"))
            {
                rest = &rest[rest.len() - stripped.len()..];
            } else {
                break;
            }
        }

        // A port follows a comma; an instance follows a backslash.
        let (head, port) = match rest.rsplit_once(',') {
            Some((head, tail)) => (head, tail.trim().parse::<u16>().ok()),
            None => (rest, None),
        };
        let (host, instance) = match head.split_once('\\') {
            Some((host, instance)) => (host, Some(instance.to_string())),
            None => (head, None),
        };

        let host = match host.trim() {
            "" | "." | "(local)" | "(LOCAL)" => "localhost".to_string(),
            other => other.to_string(),
        };

        Self {
            host,
            port,
            instance,
            dedicated_admin,
        }
    }

    /// The string the driver's connection provider expects.
    ///
    /// A dedicated admin connection has no dedicated port of its own; the
    /// server exposes it through the browser under the `admin` moniker, so the
    /// prefix is preserved for the provider to resolve.
    pub fn to_driver_string(&self) -> String {
        let base = match (&self.instance, self.port) {
            (Some(instance), _) => format!("{}\\{}", self.host, instance),
            (None, Some(port)) => format!("tcp:{},{}", self.host, port),
            (None, None) => format!("tcp:{},{}", self.host, DEFAULT_PORT),
        };
        if self.dedicated_admin {
            format!("admin:{base}")
        } else {
            base
        }
    }
}

/// Builds a driver context from the resolved options.
///
/// Returns the parsed data source alongside the context because the caller
/// needs the bare host name for `SQLCMDSERVER`.
pub fn build_context(options: &Options, workstation: &str) -> (ClientContext, DataSource) {
    let mut source = DataSource::parse(options.server.as_deref().unwrap_or("localhost"));
    // `-A` is the other spelling of the `admin:` prefix.
    source.dedicated_admin |= options.dedicated_admin_connection;

    let mut context = ClientContext::default();
    context.application_name = APPLICATION_NAME.to_string();
    context.workstation_id = options
        .workstation
        .clone()
        .unwrap_or_else(|| workstation.to_string());
    context.data_source = source.to_driver_string();

    if let Some(database) = &options.database {
        context.database = database.clone();
    }
    if let Some(user) = &options.user {
        context.user_name = user.clone();
    }
    if let Some(password) = &options.password {
        context.password = password.clone();
    }
    if let Some(new_password) = &options.new_password {
        context.new_password = new_password.clone();
    }

    context.connect_timeout = options.login_timeout as u32;
    context.packet_size = options.packet_size as u16;
    context.multi_subnet_failover = options.multi_subnet_failover;

    if let Some(intent) = &options.application_intent
        && intent.eq_ignore_ascii_case("readonly")
    {
        context.application_intent = ApplicationIntent::ReadOnly;
    }

    context.encryption_options = encryption(options);

    if options.column_encryption {
        context.column_encryption_setting = ColumnEncryptionSetting::Enabled;
    }

    context.tds_authentication_method = authentication(options);

    // Reaching the server through a tunnel or port-forward means the address
    // dialled is not the name the server should see at login.
    context.login_server_name = options.login_server_name.clone();

    // A federated method needs a token factory registered before the handshake:
    // the server asks for a bearer token mid-login, and without one the
    // connection fails with nothing to send.
    let method = context.tds_authentication_method.clone();
    super::entra::register(
        &mut context,
        super::entra::Credentials {
            method,
            user: options.user.clone().unwrap_or_default(),
            secret: options.password.clone().unwrap_or_default(),
        },
    );

    (context, source)
}

fn encryption(options: &Options) -> EncryptionOptions {
    let mode = match options.encrypt {
        Some(Encrypt::Strict) => EncryptionSetting::Strict,
        Some(Encrypt::On) => EncryptionSetting::On,
        Some(Encrypt::Mandatory) => EncryptionSetting::Required,
        Some(Encrypt::Optional) => EncryptionSetting::PreferOff,
        // Driver 18 negotiates encryption by default.
        None => EncryptionSetting::On,
    };
    EncryptionOptions {
        mode,
        trust_server_certificate: options.trust_server_certificate,
        host_name_in_cert: options.host_name_in_certificate.clone(),
        server_certificate: options.server_certificate.as_ref().map(Into::into),
    }
}

/// Mirrors the reference's `-G` dispatch: what `-G` means depends on which of
/// `-U`, `-P` and `-E` came with it. `--authentication-method` skips the
/// inference and names the method outright; it is validated during option
/// resolution, so an unrecognised name never reaches here.
fn authentication(options: &Options) -> TdsAuthenticationMethod {
    if let Some(named) = options.authentication_method.as_deref()
        && let Some(method) = named_method(named)
    {
        return method;
    }
    if options.use_entra_id {
        return match (&options.user, &options.password, options.trusted_connection) {
            (_, _, true) => TdsAuthenticationMethod::ActiveDirectoryIntegrated,
            (Some(_), Some(_), _) => TdsAuthenticationMethod::ActiveDirectoryPassword,
            (None, Some(_), _) => TdsAuthenticationMethod::AccessToken,
            _ => TdsAuthenticationMethod::ActiveDirectoryIntegrated,
        };
    }
    if options.trusted_connection || options.user.is_none() {
        return TdsAuthenticationMethod::SSPI;
    }
    TdsAuthenticationMethod::Password
}

/// The `--authentication-method` names go-sqlcmd accepts, matched
/// case-insensitively.
///
/// `ActiveDirectoryInteractive` is accepted by name but cannot succeed here:
/// it needs a browser and a loopback redirect listener. It resolves to the
/// default credential chain, which will pick up an existing developer sign-in
/// if there is one.
pub fn named_method(name: &str) -> Option<TdsAuthenticationMethod> {
    use TdsAuthenticationMethod as M;
    let normalized = name.trim().to_ascii_lowercase();
    Some(match normalized.as_str() {
        "activedirectorydefault" => M::ActiveDirectoryDefault,
        "activedirectoryintegrated" => M::ActiveDirectoryIntegrated,
        "activedirectorypassword" => M::ActiveDirectoryPassword,
        "activedirectoryinteractive" => M::ActiveDirectoryInteractive,
        "activedirectorymanagedidentity" => M::ActiveDirectoryManagedIdentity,
        "activedirectorymsi" => M::ActiveDirectoryMSI,
        // The reference treats `Application` as a synonym for the service
        // principal flow.
        "activedirectoryserviceprincipal" | "activedirectoryapplication" => {
            M::ActiveDirectoryServicePrincipal
        }
        "activedirectorydevicecode" | "activedirectorydevicecodeflow" => {
            M::ActiveDirectoryDeviceCodeFlow
        }
        "activedirectoryworkloadidentity" => M::ActiveDirectoryWorkloadIdentity,
        "activedirectoryazcli" => M::ActiveDirectoryAzCli,
        "activedirectoryazuredevelopercli" => M::ActiveDirectoryAzureDeveloperCli,
        "activedirectoryazurepipelines" => M::ActiveDirectoryAzurePipelines,
        "activedirectoryenvironment" => M::ActiveDirectoryEnvironment,
        "activedirectoryclientassertion" => M::ActiveDirectoryClientAssertion,
        "sqlpassword" => M::Password,
        _ => return None,
    })
}

pub async fn connect(
    context: ClientContext,
    source: &DataSource,
    cancel: Option<&CancelHandle>,
) -> TdsResult<TdsClient> {
    let provider = TdsConnectionProvider::new();
    let datasource = source.to_driver_string();
    let mut client = provider.create_client(context, &datasource, cancel).await?;
    // A batch that fails part-way still has result sets to render, and the
    // reference prints them. Without this the driver stops at the first error.
    client.set_defer_batch_errors(true);
    Ok(client)
}
