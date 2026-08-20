// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Value-level conversion for both ODBC data directions.
//!
//! [`fetch_convert`] converts a fetched `ColumnValues` into a requested
//! `SQL_C_*` buffer; [`param_convert`] converts a bound application buffer into
//! the `SqlType` sent as an RPC parameter. Both sit on the direction-neutral
//! pieces here: the [`error`] outcome vocabulary and the exact [`numeric`]
//! value model.
//!
//! Only the value model is shared. Each direction keeps its own audited unsafe
//! pointer I/O — fetch writes caller buffers, parameters read them — and each
//! decides conversion legality at the moment its types become known: a
//! bind-time matrix in `crate::params` for parameters, [`error::ConvError`]
//! returned from inside the converters for fetch.

pub(crate) mod error;
pub(crate) mod fetch_convert;
pub(crate) mod numeric;
mod param_buffer;
pub(crate) mod param_convert;
