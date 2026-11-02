// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SSPI Authentication Handler
//!
//! This module provides the `SspiAuthHandler` which manages the SSPI/GSSAPI
//! authentication flow with SQL Server. It abstracts the platform-specific
//! security context operations and handles the multi-round authentication
//! protocol.
//!
//! # Authentication Flow
//!
//! 1. **Initial Token**: Generate first SSPI token for Login7 packet
//! 2. **Challenge-Response Loop**: Exchange tokens until authentication completes
//!    - Receive SSPI challenge token from server (token type 0xED)
//!    - Generate response using security context
//!    - Send response in SSPI message (packet type 0x11)
//! 3. **Completion**: Security context indicates authentication complete
//!
//! # Platform Support
//!
//! - **Windows**: Uses SSPI with Negotiate/Kerberos/NTLM packages
//! - **Linux/macOS**: Uses GSSAPI with Kerberos
//!
//! # Example
//!
//! ```rust,ignore
//! let config = context.integrated_auth_config();
//! let mut handler = SspiAuthHandler::new(&config, "server.domain.com", 1433)?;
//!
//! // Get initial token for Login7
//! let initial_token = handler.get_initial_token()?;
//!
//! // Later, when server sends challenge:
//! if let Some(response) = handler.process_challenge(&challenge_data)? {
//!     // Send response to server
//! }
//! ```

use crate::core::TdsResult;
use crate::error::Error;
use crate::security::{
    IntegratedAuthConfig, SecurityContext, SecurityError, create_security_context,
};

/// State machine for SSPI authentication
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SspiAuthState {
    /// Not started yet
    Initial,
    /// Sent initial token, waiting for server challenge
    WaitingForChallenge,
    /// Processing challenge, sending response
    SendingResponse,
    /// Authentication completed successfully
    Complete,
    /// Authentication failed
    Failed,
}

/// Handler for SSPI/GSSAPI authentication with SQL Server.
///
/// This handler manages the security context and handles the multi-round
/// authentication protocol used by SSPI and GSSAPI.
pub(crate) struct SspiAuthHandler {
    /// The platform-specific security context
    security_context: Box<dyn SecurityContext>,
    /// Current authentication state
    state: SspiAuthState,
    /// Target SPN, recorded on the login's connect stage.
    target_spn: String,
}

impl SspiAuthHandler {
    /// Creates a new SSPI authentication handler.
    ///
    /// # Arguments
    ///
    /// * `config` - The integrated authentication configuration
    /// * `server` - The server hostname or IP address
    /// * `port` - The server port
    ///
    /// # Returns
    ///
    /// A new handler ready to generate the initial token, or an error
    /// if the security context could not be created.
    pub(crate) fn new(config: &IntegratedAuthConfig, server: &str, port: u16) -> TdsResult<Self> {
        let security_context = create_security_context(config, server, port)?;
        let target_spn = security_context.imported_name().to_string();

        Ok(Self {
            security_context,
            state: SspiAuthState::Initial,
            target_spn,
        })
    }

    /// Creates a new handler using a pre-created security context.
    ///
    /// This is useful for testing with mock security contexts.
    #[cfg(test)]
    pub(crate) fn with_context(
        security_context: Box<dyn SecurityContext>,
        target_spn: String,
    ) -> Self {
        Self {
            security_context,
            state: SspiAuthState::Initial,
            target_spn,
        }
    }

    /// Generates the initial SSPI token to include in the Login7 packet.
    ///
    /// This must be called before any challenge-response exchange.
    ///
    /// # Returns
    ///
    /// The initial authentication token bytes, or an error if token
    /// generation failed.
    pub(crate) fn get_initial_token(&mut self) -> TdsResult<Vec<u8>> {
        if self.state != SspiAuthState::Initial {
            return Err(Error::Security(SecurityError::InitContextFailed {
                message: "get_initial_token called in wrong state".to_string(),
                code: 0,
            }));
        }

        let result = self.security_context.generate_token(None)?;

        if result.is_complete {
            // Single-round authentication (possible with Kerberos)
            self.state = SspiAuthState::Complete;
        } else {
            self.state = SspiAuthState::WaitingForChallenge;
        }

        Ok(result.data)
    }

    /// Processes a challenge token from the server and generates a response.
    ///
    /// # Arguments
    ///
    /// * `challenge` - The SSPI challenge token received from the server
    ///
    /// # Returns
    ///
    /// * `Ok(Some(data))` - Response token to send to server
    /// * `Ok(None)` - Authentication complete, no more tokens needed
    /// * `Err(...)` - Authentication failed
    pub(crate) fn process_challenge(&mut self, challenge: &[u8]) -> TdsResult<Option<Vec<u8>>> {
        match self.state {
            SspiAuthState::WaitingForChallenge | SspiAuthState::SendingResponse => {}
            SspiAuthState::Complete => return Ok(None),
            SspiAuthState::Initial => {
                return Err(Error::Security(SecurityError::InitContextFailed {
                    message: "process_challenge called before get_initial_token".to_string(),
                    code: 0,
                }));
            }
            SspiAuthState::Failed => {
                return Err(Error::Security(SecurityError::InitContextFailed {
                    message: "Authentication already failed".to_string(),
                    code: 0,
                }));
            }
        }

        self.state = SspiAuthState::SendingResponse;

        let result = match self.security_context.generate_token(Some(challenge)) {
            Ok(r) => r,
            Err(e) => {
                self.state = SspiAuthState::Failed;
                return Err(Error::Security(e));
            }
        };

        if result.is_complete {
            self.state = SspiAuthState::Complete;
            // Server may still expect a final response token
            if result.data.is_empty() {
                return Ok(None);
            }
        }

        Ok(Some(result.data))
    }

    /// Returns true if authentication is complete.
    #[cfg(test)]
    pub(crate) fn is_complete(&self) -> bool {
        self.state == SspiAuthState::Complete
    }

    /// Returns the current authentication state.
    #[cfg(test)]
    pub(crate) fn state(&self) -> SspiAuthState {
        self.state
    }

    /// Returns the target SPN being used for authentication.
    pub(crate) fn target_spn(&self) -> &str {
        &self.target_spn
    }

    /// Returns the security package name being used.
    pub(crate) fn package_name(&self) -> &str {
        self.security_context.package_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::mock::MockSecurityContext;

    #[test]
    fn test_sspi_handler_single_round() {
        let mock = MockSecurityContext::single_round("test_token".as_bytes().to_vec());
        let mut handler =
            SspiAuthHandler::with_context(Box::new(mock), "MSSQLSvc/server:1433".to_string());

        assert_eq!(handler.state(), SspiAuthState::Initial);

        let initial_token = handler.get_initial_token().unwrap();
        assert_eq!(initial_token, b"test_token");
        assert!(handler.is_complete());
    }

    #[test]
    fn test_sspi_handler_multi_round() {
        let mock = MockSecurityContext::multi_round(
            b"ntlm_type1".to_vec(), // Initial token
            b"ntlm_type3".to_vec(), // Response to challenge
        );
        let mut handler =
            SspiAuthHandler::with_context(Box::new(mock), "MSSQLSvc/server:1433".to_string());

        // Get initial token
        let initial_token = handler.get_initial_token().unwrap();
        assert_eq!(initial_token, b"ntlm_type1");
        assert!(!handler.is_complete());
        assert_eq!(handler.state(), SspiAuthState::WaitingForChallenge);

        // Process challenge
        let response = handler.process_challenge(b"ntlm_type2_challenge").unwrap();
        assert!(response.is_some());
        assert_eq!(response.unwrap(), b"ntlm_type3");
        assert!(handler.is_complete());
    }

    #[test]
    fn test_sspi_handler_state_validation() {
        let mock = MockSecurityContext::multi_round(b"token1".to_vec(), b"token2".to_vec());
        let mut handler =
            SspiAuthHandler::with_context(Box::new(mock), "MSSQLSvc/server:1433".to_string());

        // Should fail if challenge is processed before initial token
        let result = handler.process_challenge(b"challenge");
        assert!(result.is_err());
    }

    #[test]
    fn test_sspi_handler_getters() {
        let mock = MockSecurityContext::single_round(vec![]);
        let handler =
            SspiAuthHandler::with_context(Box::new(mock), "MSSQLSvc/server:1433".to_string());

        assert_eq!(handler.target_spn(), "MSSQLSvc/server:1433");
        assert!(!handler.package_name().is_empty());
    }
}
