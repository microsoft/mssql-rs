// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Ensure exactly one TLS backend is enabled at compile time.
#[cfg(all(feature = "native-tls-backend", feature = "rustls-backend"))]
compile_error!(
    "Features `native-tls-backend` and `rustls-backend` are mutually exclusive. \
     Enable exactly one TLS backend."
);

#[cfg(not(any(feature = "native-tls-backend", feature = "rustls-backend")))]
compile_error!(
    "No TLS backend enabled. Enable exactly one of `native-tls-backend` or `rustls-backend`."
);

pub(crate) mod buffers;
pub(crate) mod certificate_validator;
pub(crate) mod extractable_stream;
#[cfg(windows)]
pub(crate) mod localdb;
#[cfg(windows)]
pub(crate) mod named_pipes;
/// Network transport creation and TLS negotiation.
pub mod network_transport;
/// Parallel TCP connect for MultiSubnetFailover.
pub mod parallel_connect;
/// SSL/TLS stream handling (native-tls backend).
#[cfg(feature = "native-tls-backend")]
pub mod ssl_handler;
/// SSL/TLS stream handling (rustls backend).
#[cfg(feature = "rustls-backend")]
pub mod ssl_handler_rustls;
/// High-level TDS transport abstraction.
pub mod tds_transport;
/// TLS-backend-agnostic TDS/TLS framing (TlsOverTdsStream).
pub(crate) mod tls_over_tds;
