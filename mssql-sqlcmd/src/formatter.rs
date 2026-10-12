// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Output formatters.
//!
//! A formatter receives what native sqlcmd would otherwise print as text —
//! result sets, row counts, server messages — and renders it in another format.

pub mod json;
