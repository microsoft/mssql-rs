// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use adbc_core::error::{Error, Status};
use mssql_tds::error::Error as TdsError;

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::with_message_and_status(message, Status::InvalidArguments)
}

pub(crate) fn state(message: impl Into<String>) -> Error {
    Error::with_message_and_status(message, Status::InvalidState)
}

pub(crate) fn unsupported(feature: impl std::fmt::Display) -> Error {
    Error::with_message_and_status(
        format!("{feature} is not supported"),
        Status::NotImplemented,
    )
}

pub(crate) fn data(message: impl Into<String>) -> Error {
    Error::with_message_and_status(message, Status::InvalidData)
}

pub(crate) fn tds(error: TdsError) -> Error {
    let status = match &error {
        TdsError::Io(_) | TdsError::ConnectionError(_) => Status::IO,
        TdsError::TimeoutError(_) => Status::Timeout,
        TdsError::OperationCancelledError(_) => Status::Cancelled,
        TdsError::UsageError(_) => Status::InvalidArguments,
        TdsError::TypeConversionError(_) => Status::InvalidData,
        TdsError::SqlServerError { diagnostics } => {
            match diagnostics.errors.first().map(|error| error.number) {
                Some(18456) => Status::Unauthenticated,
                Some(229 | 230 | 262) => Status::Unauthorized,
                Some(2601 | 2627 | 547 | 515) => Status::Integrity,
                Some(208 | 2812) => Status::NotFound,
                Some(2714) => Status::AlreadyExists,
                _ => Status::InvalidData,
            }
        }
        _ => Status::IO,
    };
    let mut result = Error::with_message_and_status(error.to_string(), status);
    if let TdsError::SqlServerError { diagnostics } = &error {
        if let Some(first) = diagnostics.errors.first() {
            result.vendor_code = i32::try_from(first.number).unwrap_or(i32::MAX);
        }
        result.details = Some(
            diagnostics
                .errors
                .iter()
                .enumerate()
                .map(|(index, error)| {
                    (
                        format!("mssql.error.{index}"),
                        error.to_string().into_bytes(),
                    )
                })
                .collect(),
        );
    }
    result
}
