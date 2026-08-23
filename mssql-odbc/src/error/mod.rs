// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub(crate) mod diag;

pub(crate) use diag::DiagRecord;
pub(crate) use diag::HasDiagnostics;
pub(crate) use diag::free_errors;
pub(crate) use diag::post_sql_error;
