// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::{
    connection::client_context::ClientContext,
    core::{EncryptionOptions, EncryptionSetting},
};
use tracing::info;

#[napi(object)]
#[derive(Clone)]
pub struct JsClientContext {
    pub server_name: String,
    pub port: u16,
    pub user_name: String,
    pub password: String,
    pub database: String,
    pub trust_server_certificate: bool,
    /// Omit to inherit startup defaults; false explicitly selects sequential TCP.
    pub multi_subnet_failover: Option<bool>,
}

impl From<JsClientContext> for ClientContext {
    fn from(js_ctx: JsClientContext) -> Self {
        let encryption_options = EncryptionOptions {
            mode: EncryptionSetting::Required,
            trust_server_certificate: js_ctx.trust_server_certificate,
            host_name_in_cert: None,
            server_certificate: None,
        };

        info!(
            "Creating ClientContext with server_name: {}, port: {}, user_name: {}, database: {}",
            js_ctx.server_name, js_ctx.port, js_ctx.user_name, js_ctx.database
        );
        let mut context = ClientContext::default();
        context.user_name = js_ctx.user_name;
        context.password = js_ctx.password;
        context.database = js_ctx.database;
        context.encryption_options = encryption_options;
        context.multi_subnet_failover = js_ctx.multi_subnet_failover;
        context
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msf_preserves_explicit_and_inherited_options() {
        for choice in [None, Some(false), Some(true)] {
            let native: ClientContext = JsClientContext {
                server_name: "localhost".into(),
                port: 1433,
                user_name: String::new(),
                password: String::new(),
                database: String::new(),
                trust_server_certificate: false,
                multi_subnet_failover: choice,
            }
            .into();
            assert_eq!(native.multi_subnet_failover, choice);
        }
    }
}
