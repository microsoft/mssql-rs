// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection-local CA validation without changing Schannel's channel binding.
//!
//! `native_tls::TlsStream::tls_server_end_point` returns a certificate digest,
//! not the `SECPKG_ATTR_UNIQUE_BINDINGS` token used by this engine. Keeping the
//! handshake here preserves the existing SSPI binding on custom-CA connections.

use std::{io, mem, ptr};

use windows_sys::Win32::Foundation;
use windows_sys::Win32::Security::{Authentication::Identity, Cryptography as crypto};

use super::errors::sec_status_to_io_error;
use super::handshake::SecCtx;

struct Cert(*const crypto::CERT_CONTEXT);

impl Drop for Cert {
    fn drop(&mut self) {
        // SAFETY: owns the reference returned by QueryContextAttributesW.
        unsafe { crypto::CertFreeCertificateContext(self.0) };
    }
}

struct Store(crypto::HCERTSTORE);

impl Drop for Store {
    fn drop(&mut self) {
        // SAFETY: owns the store opened by CertOpenStore.
        unsafe { crypto::CertCloseStore(self.0, 0) };
    }
}

struct Engine(crypto::HCERTCHAINENGINE);

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: owns the engine returned by CertCreateCertificateChainEngine.
        unsafe { crypto::CertFreeCertificateChainEngine(self.0) };
    }
}

struct Chain(*const crypto::CERT_CHAIN_CONTEXT);

impl Drop for Chain {
    fn drop(&mut self) {
        // SAFETY: owns the chain returned by CertGetCertificateChain.
        unsafe { crypto::CertFreeCertificateChain(self.0) };
    }
}

pub(super) fn validate(
    ctx: &SecCtx,
    roots: &[Vec<u8>],
    include_platform_roots: bool,
    host: &str,
) -> io::Result<()> {
    let mut cert: *mut crypto::CERT_CONTEXT = ptr::null_mut();
    // SAFETY: ctx is a completed handshake; the query initializes a certificate
    // reference which is released by Cert.
    let status = unsafe {
        Identity::QueryContextAttributesW(
            ctx.raw(),
            Identity::SECPKG_ATTR_REMOTE_CERT_CONTEXT,
            &mut cert as *mut _ as *mut _,
        )
    };
    if status != Foundation::SEC_E_OK {
        return Err(sec_status_to_io_error(status, "query remote certificate"));
    }
    if cert.is_null() {
        return Err(io::Error::other("Schannel returned no remote certificate"));
    }
    let cert = Cert(cert);
    validate_certificate(&cert, roots, include_platform_roots, host)
}

fn validate_certificate(
    cert: &Cert,
    roots: &[Vec<u8>],
    include_platform_roots: bool,
    host: &str,
) -> io::Result<()> {
    if host.encode_utf16().any(|c| c == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in TLS host name",
        ));
    }
    // SAFETY: these store providers accept null parameters and allocate
    // independent in-memory stores. No system certificate store is modified.
    let roots_store = unsafe {
        crypto::CertOpenStore(
            crypto::CERT_STORE_PROV_MEMORY,
            0,
            0,
            crypto::CERT_STORE_CREATE_NEW_FLAG,
            ptr::null(),
        )
    };
    if roots_store.is_null() {
        return Err(io::Error::last_os_error());
    }
    let roots_store = Store(roots_store);
    for root in roots {
        let len = u32::try_from(root.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "CA certificate too large"))?;
        // SAFETY: root is readable for len bytes; the store copies the DER.
        if unsafe {
            crypto::CertAddEncodedCertificateToStore(
                roots_store.0,
                crypto::X509_ASN_ENCODING,
                root.as_ptr(),
                len,
                crypto::CERT_STORE_ADD_REPLACE_EXISTING,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }

    // SAFETY: the collection provider accepts a null parameter.
    let additional =
        unsafe { crypto::CertOpenStore(crypto::CERT_STORE_PROV_COLLECTION, 0, 0, 0, ptr::null()) };
    if additional.is_null() {
        return Err(io::Error::last_os_error());
    }
    let additional = Store(additional);
    // SAFETY: cert owns a live CERT_CONTEXT. The collection retains references
    // to both stores, and no writes to the peer's certificate store are needed.
    for store in [roots_store.0, unsafe { (*cert.0).hCertStore }] {
        if !store.is_null()
            && unsafe { crypto::CertAddStoreToCollection(additional.0, store, 0, 0) } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }

    // With platform roots, keep platform trust semantics first, including
    // system disallowed roots. Only an untrusted-root error permits retrying
    // with the explicit CA roots; expired certificates, wrong names and invalid
    // signatures never bypass it.
    if include_platform_roots {
        let system_error = match verify_chain(0, cert, additional.0, host) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        if system_error.raw_os_error() != Some(Foundation::CERT_E_UNTRUSTEDROOT) {
            return Err(system_error);
        }
    }

    // SAFETY: the config is initialized with documented defaults; the root
    // store outlives the engine and the engine is freed by its RAII owner.
    let engine = unsafe {
        let mut config: crypto::CERT_CHAIN_ENGINE_CONFIG = mem::zeroed();
        config.cbSize = mem::size_of_val(&config) as u32;
        config.hExclusiveRoot = roots_store.0;
        let mut engine = 0;
        if crypto::CertCreateCertificateChainEngine(&config, &mut engine) == 0 {
            return Err(io::Error::last_os_error());
        }
        Engine(engine)
    };
    verify_chain(engine.0, cert, additional.0, host)
}

fn verify_chain(
    engine: crypto::HCERTCHAINENGINE,
    cert: &Cert,
    additional: crypto::HCERTSTORE,
    host: &str,
) -> io::Result<()> {
    let mut host: Vec<u16> = host.encode_utf16().chain(Some(0)).collect();
    // SAFETY: all input buffers and handles live through the synchronous calls.
    // The resulting chain is freed on every exit path by Chain.
    unsafe {
        let mut usage = [crypto::szOID_PKIX_KP_SERVER_AUTH as *mut u8];
        let mut params: crypto::CERT_CHAIN_PARA = mem::zeroed();
        params.cbSize = mem::size_of_val(&params) as u32;
        params.RequestedUsage.dwType = crypto::USAGE_MATCH_TYPE_AND;
        params.RequestedUsage.Usage.cUsageIdentifier = usage.len() as u32;
        params.RequestedUsage.Usage.rgpszUsageIdentifier = usage.as_mut_ptr();
        let mut chain = ptr::null_mut();
        let flags = crypto::CERT_CHAIN_CACHE_END_CERT
            | crypto::CERT_CHAIN_REVOCATION_CHECK_CACHE_ONLY
            | crypto::CERT_CHAIN_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT;
        if crypto::CertGetCertificateChain(
            engine,
            cert.0,
            ptr::null(),
            additional,
            &params,
            flags,
            ptr::null(),
            &mut chain,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let chain = Chain(chain);
        let mut ssl: crypto::HTTPSPolicyCallbackData = mem::zeroed();
        ssl.Anonymous.cbSize = mem::size_of_val(&ssl) as u32;
        ssl.dwAuthType = crypto::AUTHTYPE_SERVER;
        ssl.pwszServerName = host.as_mut_ptr();
        let mut policy: crypto::CERT_CHAIN_POLICY_PARA = mem::zeroed();
        policy.cbSize = mem::size_of_val(&policy) as u32;
        // Match native-tls: reject known revocation without requiring an online
        // revocation responder for private CAs.
        policy.dwFlags = crypto::CERT_CHAIN_POLICY_IGNORE_ALL_REV_UNKNOWN_FLAGS;
        policy.pvExtraPolicyPara = &mut ssl as *mut _ as *mut _;
        let mut status: crypto::CERT_CHAIN_POLICY_STATUS = mem::zeroed();
        status.cbSize = mem::size_of_val(&status) as u32;
        if crypto::CertVerifyCertificateChainPolicy(
            crypto::CERT_CHAIN_POLICY_SSL,
            chain.0,
            &policy,
            &mut status,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        if status.dwError != 0 {
            return Err(io::Error::from_raw_os_error(status.dwError as i32));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!("tests/test_certificates/{name}");
        let pem = std::fs::read(path)
            .expect("generate fixtures with scripts/generate_mock_tds_server_certs.ps1");
        native_tls::Certificate::from_pem(&pem)
            .unwrap()
            .to_der()
            .unwrap()
    }

    #[test]
    fn supplemental_roots_preserve_chain_and_hostname_validation() {
        let (leaf, root, unrelated) = (
            fixture("ca_signed_cert.pem"),
            fixture("ca_cert.pem"),
            fixture("unrelated_ca_cert.pem"),
        );
        // SAFETY: the DER is valid and remains readable for the call; Cert
        // releases the allocated context.
        let cert = unsafe {
            crypto::CertCreateCertificateContext(
                crypto::X509_ASN_ENCODING,
                leaf.as_ptr(),
                leaf.len() as u32,
            )
        };
        assert!(!cert.is_null());
        let cert = Cert(cert);
        for include_platform_roots in [true, false] {
            let check = |roots: &[Vec<u8>], host| {
                validate_certificate(&cert, roots, include_platform_roots, host)
            };
            assert!(check(std::slice::from_ref(&root), "localhost").is_ok());
            assert!(check(std::slice::from_ref(&root), "wrong.example").is_err());
            assert!(check(std::slice::from_ref(&unrelated), "localhost").is_err());
            assert!(check(&[], "localhost").is_err());
            assert!(check(std::slice::from_ref(&root), "localhost\0wrong.example").is_err());
        }
    }

    #[test]
    fn invalid_context_cannot_skip_custom_ca_validation() {
        assert!(validate(&SecCtx::for_test_only(), &[], true, "localhost").is_err());
    }
}
