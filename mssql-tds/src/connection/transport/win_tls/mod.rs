// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Direct Schannel TLS implementation (Windows-only).
//!
//! This module is the in-tree replacement for the Windows path of the
//! `native-tls` / `schannel` crate stack. It exists because the upstream
//! `schannel` crate's `validate()` unconditionally calls
//! `CertGetCertificateChain` + `CertVerifyCertificateChainPolicy`, even when
//! the caller would like to skip chain validation entirely (the case ODBC
//! satisfies for `TrustServerCertificate=Yes`). On cold-CAPI2 machines that
//! chain build can stall ~15s, which manifested as Adam Machanic's bulkcopy
//! timeout regression — see
//! `.github/prompts/plan-decoupleTlsBackendSchannelOdbcParity.prompt.md`.
//!
//! Layering:
//! ```text
//!   AsyncRead/AsyncWrite               (stream)
//!         │
//!         ▼
//!   sync Schannel handshake + records  (handshake + record_layer)
//!         │
//!         ▼
//!   raw SSPI FFI + cred cache          (sspi + cred)
//! ```
//!
//! The whole stack — FFI plumbing, credential cache, handshake, record
//! layer, async wrapper, validation, and the engine impl — lands together
//! and is wired into [`super::super::tls::default_engine`] as the default
//! Windows TLS engine.

pub(crate) mod alpn;
pub(crate) mod bindings;
pub(crate) mod cred;
#[cfg(feature = "tls-schannel-direct")]
pub(crate) mod engine;
pub(crate) mod errors;
pub(crate) mod handshake;
pub(crate) mod record_layer;
pub(crate) mod sspi;
pub(crate) mod stream;
pub(crate) mod validate;
