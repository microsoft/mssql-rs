// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDS protocol message types.
//!
//! Each sub-module corresponds to a TDS message kind: SQL batches, RPC
//! requests, login/prelogin handshakes, bulk-load streams, and transaction
//! management envelopes. Messages implement the [`messages::Request`] trait
//! for serialization into TDS packets.

pub(crate) mod batch;
/// Bulk load streaming writer.
pub mod bulk_load;
/// RPC parameter types and status flags.
pub mod parameters;

pub(crate) mod features;
pub(crate) mod headers;
pub(crate) mod rpc;

pub(crate) mod attention;
pub(crate) mod login;
pub mod login_options;
pub(crate) mod messages;
pub(crate) mod prelogin;
pub mod transaction_management;
