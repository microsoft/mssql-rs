// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Mutex;

use super::{HandleType, HasObjectType};
use crate::error::{DiagRecord, HasDiagnostics};

/// The four automatically-allocated implicit descriptors owned by every
/// statement: application/implementation row/parameter descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DescKind {
    AppRow,
    AppParam,
    ImpRow,
    ImpParam,
}

/// Descriptor handle.
///
/// Currently a minimal placeholder: the Windows Driver Manager requires a
/// statement's implicit descriptors to exist as valid handles (it fetches them
/// via `SQLGetStmtAttr`), but field-level access is not yet implemented.
#[derive(Debug)]
pub(crate) struct DescHandle {
    pub(crate) object_type: HandleType,
    #[allow(dead_code)]
    pub(crate) kind: DescKind,
    /// Reserved for future field-level descriptor access; unused while the
    /// Driver Manager only requires the handles to exist.
    #[allow(dead_code)]
    pub(crate) inner: Mutex<DescState>,
}

#[derive(Debug)]
pub(crate) struct DescState {
    pub(crate) diag_records: Vec<DiagRecord>,
}

impl DescHandle {
    pub(crate) fn new(kind: DescKind) -> Self {
        Self {
            object_type: HandleType::Desc,
            kind,
            inner: Mutex::new(DescState {
                diag_records: Vec::new(),
            }),
        }
    }
}

impl HasObjectType for DescHandle {
    fn object_type_mut(&mut self) -> &mut HandleType {
        &mut self.object_type
    }
}

impl HasDiagnostics for DescState {
    fn diag_records(&self) -> &[DiagRecord] {
        &self.diag_records
    }
    fn diag_records_mut(&mut self) -> &mut Vec<DiagRecord> {
        &mut self.diag_records
    }
}
