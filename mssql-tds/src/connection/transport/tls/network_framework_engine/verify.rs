// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Server certificate verification for the Network.framework engine.
//!
//! A verify block replaces Network.framework's own trust evaluation, which can
//! neither accept an arbitrary certificate nor hand it back for pinning. The
//! block evaluates the chain with Security.framework under Apple's TLS server
//! policy, as Secure Transport did under native-tls, so keychain roots,
//! administrator-installed CAs and trust settings keep deciding which servers
//! are trusted.

use std::sync::{Mutex, PoisonError};

use core_foundation::base::TCFType;
use security_framework::policy::SecPolicy;
use security_framework::secure_transport::SslProtocolSide;
use security_framework::trust::SecTrust;

use super::ffi::{Object, sec_trust_copy_ref};
use crate::connection::transport::tls::TlsValidationConfig;

#[derive(Debug, PartialEq, Eq)]
enum Trust {
    /// `TrustServerCertificate` and certificate pinning: any chain is accepted.
    AcceptAny,
    System {
        check_hostname: bool,
    },
}

#[derive(Default)]
struct Outcome {
    leaf: Option<Vec<u8>>,
    failure: Option<String>,
}

pub(super) struct Verifier {
    trust: Trust,
    host_name: String,
    capture_leaf: bool,
    outcome: Mutex<Outcome>,
}

impl Verifier {
    /// `capture_leaf` keeps the server's certificate for pinning.
    pub(super) fn new(
        validation: &TlsValidationConfig,
        host_name: &str,
        capture_leaf: bool,
    ) -> Self {
        Self {
            trust: if validation.accept_invalid_certs {
                Trust::AcceptAny
            } else {
                Trust::System {
                    check_hostname: !validation.accept_invalid_hostnames,
                }
            },
            host_name: host_name.trim_end_matches('.').to_owned(),
            capture_leaf,
            outcome: Mutex::default(),
        }
    }

    /// Decides whether to trust the peer. Called on a global queue from inside
    /// Network.framework's verify block, because evaluating a chain can block
    /// on the network.
    ///
    /// `trust` is the block's `sec_trust_t` argument.
    pub(super) fn verify(&self, trust: Object) -> bool {
        // SAFETY: `trust` is live for the duration of the verify block. The copy
        // is owned (+1), which `wrap_under_create_rule` takes over.
        let trust = unsafe {
            let trust_ref = sec_trust_copy_ref(trust);
            if trust_ref.is_null() {
                self.record_failure("the TLS stack provided no certificate chain".into());
                return false;
            }
            SecTrust::wrap_under_create_rule(trust_ref)
        };
        self.verify_trust(trust)
    }

    fn verify_trust(&self, mut trust: SecTrust) -> bool {
        if self.capture_leaf {
            self.lock().leaf = leaf_der(&trust);
        }
        let Trust::System { check_hostname } = self.trust else {
            return true;
        };
        let host = check_hostname.then_some(self.host_name.as_str());
        match evaluate(&mut trust, host) {
            Ok(()) => true,
            Err(reason) => {
                self.record_failure(reason);
                false
            }
        }
    }

    /// The server's certificate in DER form, if [`Verifier::new`] was asked to keep it.
    pub(super) fn take_leaf(&self) -> Option<Vec<u8>> {
        self.lock().leaf.take()
    }

    /// Why the chain was rejected, if it was.
    pub(super) fn take_failure(&self) -> Option<String> {
        self.lock().failure.take()
    }

    fn record_failure(&self, reason: String) {
        self.lock().failure = Some(reason);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Outcome> {
        self.outcome.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// `SecTrust::chain` needs the crate's `macos-12` feature, which would raise the
// minimum OS version for everything else built on it.
#[allow(deprecated)]
fn leaf_der(trust: &SecTrust) -> Option<Vec<u8>> {
    trust.certificate_at_index(0).map(|leaf| leaf.to_der())
}

/// Evaluates the chain under Apple's TLS server policy. `hostname` of `None`
/// skips the name check. The roots come from `trust`, which is the system's
/// unless the caller configured others.
fn evaluate(trust: &mut SecTrust, hostname: Option<&str>) -> Result<(), String> {
    let policy = SecPolicy::create_ssl(SslProtocolSide::SERVER, hostname);
    trust.set_policy(&policy).map_err(|e| e.to_string())?;
    trust.evaluate_with_error().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use core_foundation::date::CFDate;
    use security_framework::certificate::SecCertificate;

    use super::*;

    // Self-signed EC P-256 certificate for localhost and 127.0.0.1, valid for
    // 397 days from 2026-10-29. Apple's policy turns down what the
    // repository's `valid_cert.pem` is: no subject alternative name, no
    // serverAuth usage and a validity beyond 825 days.
    //
    //   openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    //     -keyout /dev/null -days 397 -subj /CN=localhost \
    //     -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
    //     -addext extendedKeyUsage=serverAuth
    const CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----
MIIBsTCCAVagAwIBAgIUfJAZX8d64YPLjxd/okFbVuW9oHkwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MTAyOTIxMTAwOFoXDTI3MTEzMDIx
MTAwOFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0D
AQcDQgAEnya8poU3dNof6xSoEAuPnSLRcCSr1BgwFl6wfbfPMaYGcYWwisUwKnUm
Q8ZOkM+yN1njkTtVp0ViQLEOxEyFkaOBhTCBgjAdBgNVHQ4EFgQUfmdMqTbNsN0d
KgOCCjVYmW4M5V8wHwYDVR0jBBgwFoAUfmdMqTbNsN0dKgOCCjVYmW4M5V8wDwYD
VR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwEwYDVR0l
BAwwCgYIKwYBBQUHAwEwCgYIKoZIzj0EAwIDSQAwRgIhAJfT2iM7e9stKfC63ulj
XRJyen0LI5y/2yvuWnFY0TvfAiEA5tRw8UbkCpwa8l2OEivjXBZiO9y0Y3OcUc4z
sDAGXt4=
-----END CERTIFICATE-----
";

    /// Unix time inside the certificate's validity, so the tests outlive it.
    const VERIFIED_AT: f64 = 1_810_458_608.0;
    /// Seconds from the Unix epoch (1970) to Core Foundation's (2001).
    const UNIX_TO_CORE_FOUNDATION: f64 = 978_307_200.0;

    fn certificate() -> SecCertificate {
        let der = native_tls::Certificate::from_pem(CERTIFICATE.as_bytes())
            .unwrap()
            .to_der()
            .unwrap();
        SecCertificate::from_der(&der).unwrap()
    }

    /// The certificate as a chain of one, evaluated as of [`VERIFIED_AT`]. With
    /// `trusted` it is its own root. Otherwise there are no roots at all, so
    /// the outcome does not depend on the machine's keychain.
    fn chain(trusted: bool) -> SecTrust {
        let certificate = certificate();
        let mut trust = SecTrust::create_with_certificates(
            std::slice::from_ref(&certificate),
            &[SecPolicy::create_x509()],
        )
        .unwrap();
        trust
            .set_trust_verify_date(&CFDate::new(VERIFIED_AT - UNIX_TO_CORE_FOUNDATION))
            .unwrap();
        let roots = if trusted {
            vec![certificate]
        } else {
            Vec::new()
        };
        trust.set_anchor_certificates(&roots).unwrap();
        trust.set_trust_anchor_certificates_only(true).unwrap();
        trust
    }

    fn validation(
        accept_invalid_certs: bool,
        accept_invalid_hostnames: bool,
    ) -> TlsValidationConfig {
        TlsValidationConfig {
            accept_invalid_certs,
            accept_invalid_hostnames,
            use_alpn: true,
        }
    }

    #[test]
    fn the_validation_settings_choose_how_much_to_trust() {
        let trust = |validation| Verifier::new(&validation, "db", false).trust;

        assert_eq!(trust(validation(true, false)), Trust::AcceptAny);
        assert_eq!(trust(validation(true, true)), Trust::AcceptAny);
        assert_eq!(
            trust(validation(false, false)),
            Trust::System {
                check_hostname: true
            }
        );
        assert_eq!(
            trust(validation(false, true)),
            Trust::System {
                check_hostname: false
            }
        );
    }

    #[test]
    fn the_expected_name_has_no_trailing_dot() {
        let verifier = Verifier::new(&validation(false, false), "db.example.com.", false);

        assert_eq!(verifier.host_name, "db.example.com");
    }

    #[test]
    fn accepting_any_chain_keeps_the_leaf_for_pinning() {
        let verifier = Verifier::new(&validation(true, true), "db", true);

        assert!(verifier.verify_trust(chain(false)));

        assert_eq!(verifier.take_leaf().unwrap(), certificate().to_der());
        assert!(verifier.take_leaf().is_none(), "taken once");
        assert!(verifier.take_failure().is_none());
    }

    #[test]
    fn the_leaf_is_not_kept_unless_asked_for() {
        let verifier = Verifier::new(&validation(true, true), "db", false);

        assert!(verifier.verify_trust(chain(false)));

        assert!(verifier.take_leaf().is_none());
    }

    #[test]
    fn a_trusted_chain_is_accepted_for_the_names_it_certifies() {
        for host in ["localhost", "localhost.", "127.0.0.1"] {
            let verifier = Verifier::new(&validation(false, false), host, false);

            assert!(verifier.verify_trust(chain(true)), "{host}");

            assert!(verifier.take_failure().is_none(), "{host}");
        }
    }

    #[test]
    fn a_trusted_chain_is_rejected_for_other_names() {
        for host in ["elsewhere.example.com", "127.0.0.2"] {
            let verifier = Verifier::new(&validation(false, false), host, false);

            assert!(!verifier.verify_trust(chain(true)), "{host}");

            assert!(!verifier.take_failure().unwrap().is_empty(), "{host}");
        }
    }

    #[test]
    fn an_untrusted_chain_is_rejected_with_a_reason() {
        let verifier = Verifier::new(&validation(false, false), "localhost", false);

        assert!(!verifier.verify_trust(chain(false)));

        assert!(!verifier.take_failure().unwrap().is_empty());
        assert!(verifier.take_failure().is_none(), "taken once");
    }

    #[test]
    fn ignoring_the_name_does_not_ignore_the_chain() {
        let verifier = Verifier::new(&validation(false, true), "elsewhere.example.com", false);
        assert!(verifier.verify_trust(chain(true)));
        assert!(verifier.take_failure().is_none());

        let verifier = Verifier::new(&validation(false, true), "localhost", false);
        assert!(!verifier.verify_trust(chain(false)));
        assert!(!verifier.take_failure().unwrap().is_empty());
    }
}
