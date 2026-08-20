// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Statement parameter bindings and the bind-time conversion matrix.
//!
//! Value conversion itself lives in `crate::conversion`.

mod bound_param;
pub(crate) mod conversion_matrix;

pub(crate) use bound_param::BoundParam;
