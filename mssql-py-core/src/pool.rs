// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Internal pool contracts and lifecycle. SQL reuse and AQE integration remain gated.

pub(crate) mod config;
pub(crate) mod connection;
pub(crate) mod control;
pub(crate) mod error;
mod lifecycle;
pub(crate) mod manager;

#[cfg(test)]
mod fit;
