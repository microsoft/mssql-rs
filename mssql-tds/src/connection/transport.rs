// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub(crate) mod any_transport;
pub(crate) mod buffers;
pub(crate) mod certificate_validator;
pub(crate) mod extractable_stream;
#[cfg(windows)]
pub(crate) mod localdb;
#[cfg(windows)]
pub(crate) mod named_pipes;
/// Network transport creation and TLS negotiation.
pub mod network_transport;
pub mod parallel_connect;
/// SSL/TLS stream handling.
pub mod ssl_handler;
pub mod tds_transport;
pub(crate) mod tls;
#[cfg(windows)]
pub(crate) mod win_tls;
