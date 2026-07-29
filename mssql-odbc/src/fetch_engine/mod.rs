// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Column-wise fetch machinery shared by `SQLFetch` and `SQLGetData`.
//!
//! This module holds the internal engine that backs the msodbcsql-style
//! positioning model — it is deliberately kept out of [`crate::api`], which is
//! reserved for ODBC entry points:
//! - [`row_writer`] positions the TDS decoder on a row and captures a single
//!   requested column (pausing before the first column so `SQLFetch` reads no
//!   data).
//! - [`plp_stream`] streams a large PLP (`*(MAX)` / `xml`) value chunk-by-chunk
//!   across repeated `SQLGetData` calls, transcoding to the requested C type.

pub(crate) mod plp_stream;
pub(crate) mod row_writer;
