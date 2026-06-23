// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
/// SSL/TLS stream handling.
pub mod ssl_handler;
/// High-level TDS transport abstraction.
pub mod tds_transport;
/// TLS engine abstraction (native-tls today, Schannel-direct in a later PR).
pub(crate) mod tls;
/// Direct Schannel TLS implementation (Windows-only). Not wired into the
/// dispatcher until the final PR in the Schannel-direct stack.
#[cfg(windows)]
pub(crate) mod win_tls;
