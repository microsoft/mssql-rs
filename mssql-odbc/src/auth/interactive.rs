// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `Authentication=ActiveDirectoryInteractive` — Entra sign-in through the
//! native `mssql-auth` library.
//!
//! # Platform support
//!
//! Windows only, matching msodbcsql, which delegates the entire interactive
//! experience to `mssql-auth.dll` — a Windows-only library it loads with
//! `LoadLibraryExA` (`SNI_FedAuth.cpp:249`) — and contains no browser or
//! loopback code of its own. This module is compiled only on Windows for the
//! same reason msodbcsql's Unix makefile omits `SNI_FedAuth` from its source
//! list altogether.
//!
//! Off Windows, [`super::entra::configure_auth`] resolves the request to
//! `ActiveDirectoryIntegrated`, mirroring the fall-through that msodbcsql's
//! `authMode` ternary produces (`Parse.cpp:3657-3660`). The resulting
//! diagnostic still names `ActiveDirectoryInteractive`, since that is the
//! keyword the application supplied.
//!
//! # Mechanism
//!
//! [`super::msqa`] binds the `mssql-auth.dll` entry points and hosts OneAuth's
//! sign-in window. This module owns the seam into mssql-tds: it adapts
//! OneAuth's blocking, message-pumping acquisition to the async
//! [`EntraIdTokenFactory`] contract and bounds it by the connection's login
//! deadline.

use std::sync::Arc;

use async_trait::async_trait;
use mssql_tds::connection::client_context::{EntraIdTokenFactory, TdsAuthenticationMethod};
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;
use tracing::debug;

/// msodbcsql's Entra application id, presented so this driver appears in tenant
/// sign-in logs and conditional-access policies exactly as the C++ ODBC driver
/// does. Declared at `Parse.cpp:3608` and `AzureADAuth.cpp:839`, both annotated
/// `// ODBC client ID`.
const PUBLIC_CLIENT_ID: &str = "2c1229aa-16c5-4ff5-b46b-4f7fe2a2a9c8";

/// The redirect URI registered for the application above. OneAuth resolves it
/// internally rather than listening on it, so no loopback port is opened.
/// Hardcoded by msodbcsql at `Parse.cpp:3606`.
const REDIRECT_URI: &str = "https://sqlaad/";

/// Login timeout applied when the application has not set
/// `SQL_ATTR_LOGIN_TIMEOUT`.
///
/// Interactive sign-in involves a human — reading a prompt, entering a
/// password, completing MFA — so the 15-second ODBC default would abort almost
/// every attempt. This raises only the overall login deadline; the per-attempt
/// TCP connect timeout is untouched, so an unreachable server still fails fast.
pub(super) const LOGIN_TIMEOUT_SECS: u32 = 330;

/// Acquires access tokens by driving OneAuth's interactive sign-in.
#[derive(Clone)]
pub(crate) struct InteractiveTokenFactory {
    /// `UID` from the connection string, pre-filled into the sign-in prompt and
    /// used as the token-cache key so a second connection for the same account
    /// does not prompt again.
    login_hint: String,
    /// Server name, shown in the sign-in window title so a user facing several
    /// prompts can tell which connection raised each one.
    server: String,
}

impl InteractiveTokenFactory {
    pub(crate) fn new(login_hint: Option<String>, server: String) -> Self {
        Self {
            login_hint: login_hint.unwrap_or_default(),
            server,
        }
    }

    /// Runs OneAuth's acquisition on a blocking thread.
    ///
    /// The acquisition pumps a Win32 message loop, so it cannot run on a Tokio
    /// worker. `spawn_blocking` tasks are also not cancellable, so when the
    /// caller's login deadline expires the future is dropped while the sign-in
    /// window is still up; [`CancelUiOnDrop`] closes it rather than leaving it
    /// orphaned on the user's desktop.
    async fn acquire(&self, spn: &str, sts_url: &str) -> TdsResult<String> {
        // msodbcsql titles the window "Authenticate to database on %s" with the
        // server name (`local.rc:786`, applied at `Parse.cpp:3618-3619`).
        let window_title = format!("Authenticate to database on {}", self.server);
        debug!(
            server = %self.server,
            "interactive: acquiring an Entra token via mssql-auth"
        );

        let ui_thread_id = Arc::new(super::msqa::UiThreadId::default());
        let cancel_on_drop = CancelUiOnDrop {
            ui_thread_id: Arc::clone(&ui_thread_id),
        };

        let (spn, sts_url, login_hint) = (
            spn.to_string(),
            sts_url.to_string(),
            self.login_hint.clone(),
        );
        let worker = Arc::clone(&ui_thread_id);
        let result = tokio::task::spawn_blocking(move || {
            super::msqa::acquire_token(
                &sts_url,
                &spn,
                &login_hint,
                PUBLIC_CLIENT_ID,
                REDIRECT_URI,
                &window_title,
                &worker,
            )
        })
        .await;

        // The sign-in finished on its own, so there is no window to close.
        cancel_on_drop.disarm();

        match result {
            Ok(token) => token,
            Err(e) => Err(Error::ConnectionError(format!(
                "Entra interactive sign-in did not run to completion: {e}"
            ))),
        }
    }
}

#[async_trait]
impl EntraIdTokenFactory for InteractiveTokenFactory {
    async fn create_token(
        &self,
        spn: String,
        sts_url: String,
        _auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        let token = self.acquire(&spn, &sts_url).await?;
        Ok(super::entra::encode_utf16le(&token))
    }
}

/// Closes an in-flight sign-in window if the acquisition future is dropped —
/// which is what happens when the connection's login deadline expires.
struct CancelUiOnDrop {
    ui_thread_id: Arc<super::msqa::UiThreadId>,
}

impl CancelUiOnDrop {
    /// Suppresses cancellation after the acquisition has already returned.
    fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for CancelUiOnDrop {
    fn drop(&mut self) {
        super::msqa::cancel_ui(&self.ui_thread_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_matches_msodbcsql() {
        // Parse.cpp:3608 and AzureADAuth.cpp:839, both marked "// ODBC client ID".
        // Diverging would make this driver's sign-ins appear under a different
        // application in tenant sign-in logs and conditional-access policies.
        assert_eq!(PUBLIC_CLIENT_ID, "2c1229aa-16c5-4ff5-b46b-4f7fe2a2a9c8");
        // Parse.cpp:3606.
        assert_eq!(REDIRECT_URI, "https://sqlaad/");
    }

    #[test]
    fn login_timeout_default_outlasts_a_human_sign_in() {
        // The ODBC default is 15s, which cannot cover reading a prompt plus MFA.
        const { assert!(LOGIN_TIMEOUT_SECS > 300) };
    }

    #[test]
    fn factory_defaults_login_hint_to_empty() {
        let factory = InteractiveTokenFactory::new(None, "server".to_string());
        assert!(factory.login_hint.is_empty());
        assert_eq!(factory.server, "server");
    }

    #[test]
    fn factory_keeps_the_supplied_login_hint() {
        let factory =
            InteractiveTokenFactory::new(Some("user@contoso.com".to_string()), "s".to_string());
        assert_eq!(factory.login_hint, "user@contoso.com");
    }
}
