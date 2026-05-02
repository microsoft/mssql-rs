// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock security context for testing.
//!
//! This module provides a mock implementation of `SecurityContext` that can be
//! used to test the TDS protocol integration without requiring actual SSPI or
//! GSSAPI infrastructure.

use crate::security::{SecurityContext, SecurityError, SspiAuthToken};

/// Mock security context for testing SSPI authentication flow.
///
/// This mock allows you to pre-define the tokens that will be returned
/// and simulate multi-round authentication without actual security infrastructure.
///
/// # Example
///
/// ```
/// use mssql_tds::security::mock::MockSecurityContext;
///
/// // Single-round authentication
/// let mut ctx = MockSecurityContext::single_round(vec![1, 2, 3, 4]);
/// let token = ctx
///     .generate_token(None)
///     .expect("single-round mock should yield a token");
/// assert_eq!(token.data, vec![1, 2, 3, 4]);
/// assert!(token.is_complete);
///
/// // Multi-round authentication
/// let mut ctx = MockSecurityContext::multi_round(vec![1, 2], vec![3, 4]);
/// let initial = ctx
///     .generate_token(None)
///     .expect("first mock round should yield a token");
/// assert!(!initial.is_complete);
/// let response = ctx
///     .generate_token(Some(&[5, 6]))
///     .expect("second mock round should yield a token");
/// assert!(response.is_complete);
/// ```
#[derive(Debug, Clone)]
pub struct MockSecurityContext {
    /// Tokens to return for each round
    tokens: Vec<Vec<u8>>,

    /// Expected challenge tokens from the server (for validation)
    expected_challenges: Vec<Vec<u8>>,

    /// Current round (0-indexed)
    current_round: usize,

    /// The round after which authentication is complete
    complete_after_round: usize,

    /// The SPN being used
    spn: String,

    /// The package name to report
    package_name: String,

    /// Optional error to return on a specific round
    error_on_round: Option<(usize, SecurityError)>,
}

impl MockSecurityContext {
    /// Creates a mock context for single-round authentication.
    ///
    /// The provided token is returned on the first call to `generate_token`,
    /// and authentication is immediately complete.
    pub fn single_round(token: Vec<u8>) -> Self {
        Self {
            tokens: vec![token],
            expected_challenges: vec![],
            current_round: 0,
            complete_after_round: 1,
            spn: "MSSQLSvc/mock:1433".to_string(),
            package_name: "Mock".to_string(),
            error_on_round: None,
        }
    }

    /// Creates a mock context for two-round authentication (like NTLM).
    ///
    /// - First call returns `initial_token` with `is_complete = false`
    /// - Second call (with server challenge) returns `response_token` with `is_complete = true`
    pub fn multi_round(initial_token: Vec<u8>, response_token: Vec<u8>) -> Self {
        Self {
            tokens: vec![initial_token, response_token],
            expected_challenges: vec![],
            current_round: 0,
            complete_after_round: 2,
            spn: "MSSQLSvc/mock:1433".to_string(),
            package_name: "Mock".to_string(),
            error_on_round: None,
        }
    }

    /// Creates a mock context with custom round configuration.
    ///
    /// # Arguments
    ///
    /// * `tokens` - Tokens to return for each round
    /// * `complete_after` - The round number after which authentication completes
    pub fn custom(tokens: Vec<Vec<u8>>, complete_after: usize) -> Self {
        Self {
            tokens,
            expected_challenges: vec![],
            current_round: 0,
            complete_after_round: complete_after,
            spn: "MSSQLSvc/mock:1433".to_string(),
            package_name: "Mock".to_string(),
            error_on_round: None,
        }
    }

    /// Sets the SPN for this mock context.
    pub fn with_spn(mut self, spn: String) -> Self {
        self.spn = spn;
        self
    }

    /// Sets the package name for this mock context.
    pub fn with_package_name(mut self, name: String) -> Self {
        self.package_name = name;
        self
    }

    /// Sets expected challenge tokens for validation.
    ///
    /// If set, the mock will verify that the server challenges match.
    pub fn with_expected_challenges(mut self, challenges: Vec<Vec<u8>>) -> Self {
        self.expected_challenges = challenges;
        self
    }

    /// Configures the mock to return an error on a specific round.
    pub fn with_error_on_round(mut self, round: usize, error: SecurityError) -> Self {
        self.error_on_round = Some((round, error));
        self
    }

    /// Returns the current round number.
    pub fn current_round(&self) -> usize {
        self.current_round
    }

    /// Returns the total number of rounds configured.
    pub fn total_rounds(&self) -> usize {
        self.tokens.len()
    }
}

impl SecurityContext for MockSecurityContext {
    fn package_name(&self) -> &str {
        &self.package_name
    }

    fn generate_token(
        &mut self,
        server_token: Option<&[u8]>,
    ) -> Result<SspiAuthToken, SecurityError> {
        // Check if we should return an error on this round
        if let Some((error_round, ref error)) = self.error_on_round
            && self.current_round == error_round
        {
            return Err(error.clone());
        }

        // Validate server challenge if expected
        if self.current_round > 0 && !self.expected_challenges.is_empty() {
            let expected_idx = self.current_round - 1;
            if expected_idx < self.expected_challenges.len() {
                let expected = &self.expected_challenges[expected_idx];
                if let Some(actual) = server_token
                    && actual != expected.as_slice()
                {
                    return Err(SecurityError::InvalidToken);
                }
            }
        }

        // Get the token for this round
        let token_data = self
            .tokens
            .get(self.current_round)
            .cloned()
            .unwrap_or_default();

        self.current_round += 1;

        let is_complete = self.current_round >= self.complete_after_round;

        Ok(SspiAuthToken {
            data: token_data,
            is_complete,
        })
    }

    fn is_complete(&self) -> bool {
        self.current_round >= self.complete_after_round
    }

    fn spn(&self) -> &str {
        &self.spn
    }
}

/// Creates a mock NTLM-style authentication sequence.
///
/// NTLM typically has 3 messages:
/// 1. NEGOTIATE (client → server)
/// 2. CHALLENGE (server → client)  
/// 3. AUTHENTICATE (client → server)
///
/// This creates a mock for the client side (messages 1 and 3).
pub fn create_ntlm_mock() -> MockSecurityContext {
    // Simplified NTLM message headers for testing
    let negotiate = b"NTLMSSP\x00\x01\x00\x00\x00".to_vec();
    let authenticate = b"NTLMSSP\x00\x03\x00\x00\x00".to_vec();

    MockSecurityContext::multi_round(negotiate, authenticate).with_package_name("NTLM".to_string())
}

/// Creates a mock Kerberos-style authentication sequence.
///
/// Kerberos typically completes in a single round for client authentication.
pub fn create_kerberos_mock() -> MockSecurityContext {
    // Simplified Kerberos AP-REQ header for testing
    let ap_req = vec![0x60, 0x82, 0x01, 0x00]; // ASN.1 Application tag

    MockSecurityContext::single_round(ap_req).with_package_name("Kerberos".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_round_auth() {
        let mut ctx = MockSecurityContext::single_round(vec![1, 2, 3, 4]);

        assert!(!ctx.is_complete());
        assert_eq!(ctx.current_round(), 0);

        let token = ctx
            .generate_token(None)
            .expect("single-round mock should yield a token");
        assert_eq!(token.data, vec![1, 2, 3, 4]);
        assert!(token.is_complete);
        assert!(ctx.is_complete());
        assert_eq!(ctx.current_round(), 1);
    }

    #[test]
    fn test_multi_round_auth() {
        let mut ctx = MockSecurityContext::multi_round(vec![1, 2], vec![3, 4]);

        // First round
        let initial = ctx
            .generate_token(None)
            .expect("first multi-round token should be generated");
        assert_eq!(initial.data, vec![1, 2]);
        assert!(!initial.is_complete);
        assert!(!ctx.is_complete());

        // Second round with server challenge
        let response = ctx
            .generate_token(Some(&[5, 6, 7]))
            .expect("second multi-round token should be generated");
        assert_eq!(response.data, vec![3, 4]);
        assert!(response.is_complete);
        assert!(ctx.is_complete());
    }

    #[test]
    fn test_custom_rounds() {
        let mut ctx = MockSecurityContext::custom(
            vec![vec![1], vec![2], vec![3]],
            3, // Complete after 3 rounds
        );

        // Three rounds
        let t1 = ctx
            .generate_token(None)
            .expect("first custom-round token should be generated");
        assert!(!t1.is_complete);

        let t2 = ctx
            .generate_token(Some(&[]))
            .expect("second custom-round token should be generated");
        assert!(!t2.is_complete);

        let t3 = ctx
            .generate_token(Some(&[]))
            .expect("third custom-round token should be generated");
        assert!(t3.is_complete);
    }

    #[test]
    fn test_error_on_round() {
        let mut ctx = MockSecurityContext::single_round(vec![1])
            .with_error_on_round(0, SecurityError::NoCredentials);

        let result = ctx.generate_token(None);
        assert!(result.is_err());
        assert!(matches!(result, Err(SecurityError::NoCredentials)));
    }

    #[test]
    fn test_spn_and_package_name() {
        let ctx = MockSecurityContext::single_round(vec![])
            .with_spn("MSSQLSvc/test:5000".to_string())
            .with_package_name("TestPackage".to_string());

        assert_eq!(ctx.spn(), "MSSQLSvc/test:5000");
        assert_eq!(ctx.package_name(), "TestPackage");
    }

    #[test]
    fn test_ntlm_mock() {
        let mut ctx = create_ntlm_mock();

        assert_eq!(ctx.package_name(), "NTLM");

        let negotiate = ctx
            .generate_token(None)
            .expect("NTLM mock should generate a negotiate token");
        assert!(negotiate.data.starts_with(b"NTLMSSP"));
        assert!(!negotiate.is_complete);

        let authenticate = ctx
            .generate_token(Some(&[0xAA, 0xBB]))
            .expect("NTLM mock should generate an authenticate token");
        assert!(authenticate.data.starts_with(b"NTLMSSP"));
        assert!(authenticate.is_complete);
    }

    #[test]
    fn test_kerberos_mock() {
        let mut ctx = create_kerberos_mock();

        assert_eq!(ctx.package_name(), "Kerberos");

        let token = ctx
            .generate_token(None)
            .expect("single-round mock should yield a token");
        assert!(token.is_complete);
    }

    #[test]
    fn test_expected_challenge_validation() {
        let mut ctx = MockSecurityContext::multi_round(vec![1], vec![2])
            .with_expected_challenges(vec![vec![0xAA, 0xBB]]);

        // First round - no challenge expected
        ctx.generate_token(None)
            .expect("first expected-challenge round should generate a token");

        // Second round - wrong challenge should fail
        let result = ctx.generate_token(Some(&[0xCC, 0xDD]));
        assert!(matches!(result, Err(SecurityError::InvalidToken)));
    }

    #[test]
    fn test_expected_challenge_success() {
        let mut ctx = MockSecurityContext::multi_round(vec![1], vec![2])
            .with_expected_challenges(vec![vec![0xAA, 0xBB]]);

        ctx.generate_token(None)
            .expect("first expected-challenge round should generate a token");

        // Correct challenge
        let result = ctx.generate_token(Some(&[0xAA, 0xBB]));
        assert!(result.is_ok());
    }
}
