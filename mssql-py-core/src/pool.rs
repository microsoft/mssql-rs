// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Internal pool contracts. No manager, pooled checkout or AQE API is enabled yet.

pub(crate) mod config;
pub(crate) mod connection;
pub(crate) mod control;
pub(crate) mod error;

#[cfg(test)]
mod fit;
