// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Internal pool lifecycle, SQL checkout and safe return. AQE integration remains gated.

pub(crate) mod auth;
pub(crate) mod config;
pub(crate) mod connection;
pub(crate) mod control;
pub(crate) mod error;
mod lifecycle;
pub(crate) mod manager;
pub(crate) mod session;

#[cfg(test)]
mod fit;
