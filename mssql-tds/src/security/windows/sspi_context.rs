// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows SSPI context implementation.
//!
//! This module implements the `SecurityContext` trait using Windows SSPI.

use std::ptr;

use super::sspi_ffi::{
    CompleteAuthToken, CredHandle, CtxtHandle, DeleteSecurityContext, FreeCredentialsHandle,
    InitializeSecurityContextW, PVOID, SEC_E_NO_CREDENTIALS, SEC_E_OK, SECBUFFER_CHANNEL_BINDINGS,
    SECBUFFER_TOKEN, SECBUFFER_VERSION, SECURITY_STATUS, STANDARD_CONTEXT_REQ, SecBuffer,
    SecBufferDesc, TimeStamp, acquire_credentials, get_sspi_error_message, is_success_status,
    needs_complete, needs_continue, to_wide_string,
};
use crate::security::{
    IntegratedAuthConfig, SecurityContext, SecurityError, SecurityPackage, SspiAuthToken,
    spn::make_spn_canonicalized,
};

/// Default maximum token size if we can't query the package
const DEFAULT_MAX_TOKEN_SIZE: usize = 12288;

/// Wrapper for CredHandle to make it Send + Sync
///
/// # Safety
/// SSPI handles are thread-safe for different operations on Windows.
/// The same handle should not be used concurrently from multiple threads
/// for the same operation, but our usage pattern is single-threaded per context.
struct CredHandleWrapper(CredHandle);

// SAFETY: SSPI credential handles are safe to send between threads
unsafe impl Send for CredHandleWrapper {}
// SAFETY: SSPI credential handles can be referenced from multiple threads
unsafe impl Sync for CredHandleWrapper {}

/// Wrapper for CtxtHandle to make it Send + Sync
struct CtxtHandleWrapper(CtxtHandle);

// SAFETY: SSPI context handles are safe to send between threads
unsafe impl Send for CtxtHandleWrapper {}
// SAFETY: SSPI context handles can be referenced from multiple threads
unsafe impl Sync for CtxtHandleWrapper {}

/// Windows SSPI security context.
///
/// This struct manages the SSPI authentication state, including credential
/// and context handles. It automatically cleans up handles when dropped.
pub struct WindowsSspiContext {
    /// The SPN being used for authentication
    spn: String,

    /// The security package being used
    package: SecurityPackage,

    /// Whether authentication is complete
    is_complete: bool,

    /// Whether this is a loopback connection (may retry with empty SPN)
    is_loopback: bool,

    /// Whether we've already tried the empty SPN fallback
    tried_empty_spn: bool,

    /// Channel bindings for extended protection
    channel_bindings: Option<Vec<u8>>,

    /// Credential handle
    cred_handle: Option<CredHandleWrapper>,

    /// Security context handle
    ctx_handle: Option<CtxtHandleWrapper>,

    /// Maximum token size for output buffer allocation
    max_token_size: usize,
}

impl WindowsSspiContext {
    /// Creates a new Windows SSPI security context.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for integrated authentication
    /// * `server` - The server hostname
    /// * `port` - The server port
    ///
    /// # Returns
    ///
    /// A new SSPI context, or an error if initialization fails.
    pub fn new(
        config: &IntegratedAuthConfig,
        server: &str,
        port: u16,
    ) -> Result<Self, SecurityError> {
        // Determine SPN
        // For auto-generated SPNs, canonicalize the hostname via DNS lookup
        // to ensure it matches the SPN registered in Active Directory
        let spn = config
            .server_spn
            .clone()
            .unwrap_or_else(|| make_spn_canonicalized(server, None, port));

        let package_name = config.security_package.as_sspi_name();

        // Acquire credentials for the current user
        let (cred_handle, _expiry) = acquire_credentials(package_name).map_err(|status| {
            SecurityError::AcquireCredentialsFailed {
                code: status as u32,
                message: get_sspi_error_message(status),
            }
        })?;

        // Get max token size, use default if query fails
        let max_token_size = super::sspi_ffi::get_max_token_size(package_name)
            .unwrap_or(DEFAULT_MAX_TOKEN_SIZE as u32) as usize;

        Ok(Self {
            spn,
            package: config.security_package,
            is_complete: false,
            is_loopback: config.is_loopback,
            tried_empty_spn: false,
            channel_bindings: config.channel_bindings.clone(),
            cred_handle: Some(CredHandleWrapper(cred_handle)),
            ctx_handle: None,
            max_token_size,
        })
    }

    /// Checks if SSPI is available on this system.
    pub fn check_availability() -> Result<(), SecurityError> {
        // Try to query the Negotiate package info
        match super::sspi_ffi::get_max_token_size("Negotiate") {
            Ok(_) => Ok(()),
            Err(status) => Err(SecurityError::LoadLibraryFailed(format!(
                "Failed to query SSPI: {}",
                get_sspi_error_message(status)
            ))),
        }
    }

    /// Generates a token using InitializeSecurityContextW.
    fn generate_token_impl(
        &mut self,
        server_token: Option<&[u8]>,
        target_spn: &str,
    ) -> Result<(Vec<u8>, SECURITY_STATUS), SecurityError> {
        let cred_handle = self
            .cred_handle
            .as_ref()
            .ok_or_else(|| SecurityError::InternalError("No credential handle".to_string()))?;

        // Convert SPN to wide string
        let spn_wide = to_wide_string(target_spn);

        // Prepare input buffer (for server challenge)
        let mut input_buffer = SecBuffer::default();
        let mut input_buffer_desc = SecBufferDesc::default();

        // Prepare additional input buffers for channel bindings
        let mut channel_bindings_buffer = SecBuffer::default();
        let mut input_buffers: Vec<SecBuffer>;

        if let Some(server_data) = server_token {
            // Set up the token buffer
            input_buffer.cbBuffer = server_data.len() as u32;
            input_buffer.BufferType = SECBUFFER_TOKEN;
            input_buffer.pvBuffer = server_data.as_ptr() as PVOID;

            if let Some(ref cb) = self.channel_bindings {
                // Include channel bindings
                channel_bindings_buffer.cbBuffer = cb.len() as u32;
                channel_bindings_buffer.BufferType = SECBUFFER_CHANNEL_BINDINGS;
                channel_bindings_buffer.pvBuffer = cb.as_ptr() as PVOID;

                input_buffers = vec![input_buffer, channel_bindings_buffer];
                input_buffer_desc.cBuffers = 2;
            } else {
                input_buffers = vec![input_buffer];
                input_buffer_desc.cBuffers = 1;
            }
            input_buffer_desc.pBuffers = input_buffers.as_mut_ptr();
        } else if let Some(ref cb) = self.channel_bindings {
            // First call with channel bindings only
            channel_bindings_buffer.cbBuffer = cb.len() as u32;
            channel_bindings_buffer.BufferType = SECBUFFER_CHANNEL_BINDINGS;
            channel_bindings_buffer.pvBuffer = cb.as_ptr() as PVOID;

            input_buffers = vec![channel_bindings_buffer];
            input_buffer_desc.cBuffers = 1;
            input_buffer_desc.pBuffers = input_buffers.as_mut_ptr();
        }

        // Prepare output buffer
        let mut output_token_data = vec![0u8; self.max_token_size];
        let mut output_buffer = SecBuffer {
            cbBuffer: output_token_data.len() as u32,
            BufferType: SECBUFFER_TOKEN,
            pvBuffer: output_token_data.as_mut_ptr() as PVOID,
        };
        let mut output_buffer_desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 1,
            pBuffers: &mut output_buffer,
        };

        // Get pointers for context handle (input and output)
        let (ctx_in_ptr, mut new_ctx_handle) = match &self.ctx_handle {
            Some(ctx) => (&ctx.0 as *const CtxtHandle, ctx.0),
            None => (ptr::null(), CtxtHandle::default()),
        };

        let mut context_attr: u32 = 0;
        let mut expiry = TimeStamp::default();

        // Call InitializeSecurityContextW
        let status = unsafe {
            InitializeSecurityContextW(
                &cred_handle.0,
                if ctx_in_ptr.is_null() {
                    ptr::null()
                } else {
                    ctx_in_ptr
                },
                spn_wide.as_ptr(),
                STANDARD_CONTEXT_REQ,
                0, // Reserved1
                0, // SECURITY_NATIVE_DREP
                if server_token.is_some() || self.channel_bindings.is_some() {
                    &input_buffer_desc
                } else {
                    ptr::null()
                },
                0, // Reserved2
                &mut new_ctx_handle,
                &mut output_buffer_desc,
                &mut context_attr,
                &mut expiry,
            )
        };

        // Update context handle
        if new_ctx_handle.is_valid() {
            self.ctx_handle = Some(CtxtHandleWrapper(new_ctx_handle));
        }

        if !is_success_status(status) {
            return Err(SecurityError::InitContextFailed {
                code: status as u32,
                message: get_sspi_error_message(status),
            });
        }

        // Handle CompleteAuthToken if needed (for NTLM)
        if let Some(ctx) = self.ctx_handle.as_ref().filter(|_| needs_complete(status)) {
            let complete_status = unsafe { CompleteAuthToken(&ctx.0, &output_buffer_desc) };
            if complete_status != SEC_E_OK {
                return Err(SecurityError::InitContextFailed {
                    code: complete_status as u32,
                    message: format!(
                        "CompleteAuthToken failed: {}",
                        get_sspi_error_message(complete_status)
                    ),
                });
            }
        }

        // Extract output token
        let output_size = output_buffer.cbBuffer as usize;
        output_token_data.truncate(output_size);

        Ok((output_token_data, status))
    }
}

impl SecurityContext for WindowsSspiContext {
    fn package_name(&self) -> &str {
        self.package.as_sspi_name()
    }

    fn generate_token(
        &mut self,
        server_token: Option<&[u8]>,
    ) -> Result<SspiAuthToken, SecurityError> {
        // Try with the current SPN
        let result = self.generate_token_impl(server_token, &self.spn.clone());

        match result {
            Ok((token, status)) => {
                // Check if authentication is complete
                if !needs_continue(status) {
                    self.is_complete = true;
                }

                Ok(SspiAuthToken {
                    data: token,
                    is_complete: self.is_complete,
                })
            }
            // SEC_E_NO_CREDENTIALS is mapped to the dedicated NoCredentials variant
            // BEFORE the loopback retry arm. If the user has no usable credentials,
            // retrying with an empty SPN will fail the same way and would propagate
            // a generic InitContextFailed instead of the specific NoCredentials error
            // callers depend on. ODBC's loopback retry also skips this code for the
            // same reason — there is nothing to renegotiate without credentials.
            Err(SecurityError::InitContextFailed { code, .. })
                if code == SEC_E_NO_CREDENTIALS as u32 =>
            {
                Err(SecurityError::NoCredentials)
            }
            Err(ref e) if self.is_loopback && !self.tried_empty_spn => {
                // Loopback retry: matches ODBC behavior — on any SSPI failure for
                // a loopback connection (other than NoCredentials, handled above),
                // retry once with an empty SPN. This lets Negotiate fall back to
                // NTLM. Crucially, we keep the existing context handle so SSPI can
                // renegotiate within the same session.
                tracing::debug!(
                    spn = %self.spn,
                    error = %e,
                    has_ctx = self.ctx_handle.is_some(),
                    has_server_token = server_token.is_some(),
                    "Loopback SPN retry: retrying with empty SPN"
                );
                self.tried_empty_spn = true;
                self.spn = String::new();

                // Do NOT reset the context handle — SSPI needs the existing
                // Negotiate state to fall back to NTLM (matches ODBC's goto Retry).

                let (token, status) = self.generate_token_impl(server_token, "")?;
                if !needs_continue(status) {
                    self.is_complete = true;
                }

                Ok(SspiAuthToken {
                    data: token,
                    is_complete: self.is_complete,
                })
            }
            Err(e) => Err(e),
        }
    }

    fn is_complete(&self) -> bool {
        self.is_complete
    }

    fn spn(&self) -> &str {
        &self.spn
    }
}

impl Drop for WindowsSspiContext {
    fn drop(&mut self) {
        // Clean up security context handle
        if let Some(ref mut ctx) = self.ctx_handle {
            unsafe {
                DeleteSecurityContext(&mut ctx.0);
            }
        }

        // Clean up credential handle
        if let Some(ref mut cred) = self.cred_handle {
            unsafe {
                FreeCredentialsHandle(&mut cred.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sspi_package_name() {
        // Test the package name conversion logic
        assert_eq!(SecurityPackage::Negotiate.as_sspi_name(), "Negotiate");
        assert_eq!(SecurityPackage::Kerberos.as_sspi_name(), "Kerberos");
        assert_eq!(SecurityPackage::Ntlm.as_sspi_name(), "NTLM");
    }
}
