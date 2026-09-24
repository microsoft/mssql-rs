// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::{
    connection::client_context::ClientContext,
    core::{EncryptionOptions, EncryptionSetting, ServerTrust},
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
}

impl From<JsClientContext> for ClientContext {
    fn from(js_ctx: JsClientContext) -> Self {
        let server_trust = if js_ctx.trust_server_certificate {
            ServerTrust::DangerAcceptAny
        } else {
            ServerTrust::default()
        };
        let encryption_options = EncryptionOptions::new()
            .with_mode(EncryptionSetting::Required)
            .with_server_trust(server_trust);

        info!(
            "Creating ClientContext with server_name: {}, port: {}, user_name: {}, database: {}",
            js_ctx.server_name, js_ctx.port, js_ctx.user_name, js_ctx.database
        );
        let mut context = ClientContext::default();
        context.user_name = js_ctx.user_name;
        context.password = js_ctx.password;
        context.database = js_ctx.database;
        context.encryption_options = encryption_options;
        context
    }
}
