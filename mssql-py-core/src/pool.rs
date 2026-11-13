// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Internal pool lifecycle and SQL checkout. Safe return and AQE integration remain gated.

pub(crate) mod config;
pub(crate) mod connection;
pub(crate) mod control;
pub(crate) mod error;
mod lifecycle;
pub(crate) mod manager;
pub(crate) mod session;

#[cfg(test)]
mod fit;
