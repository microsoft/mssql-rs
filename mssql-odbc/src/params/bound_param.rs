// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;

use crate::api::odbc_types::{
    SQL_BIND_BY_COLUMN, SQL_C_DEFAULT, SQL_C_NUMERIC, SQL_PARAM_INPUT, SQL_PREC_NUMERIC, SqlLen,
    SqlPointer, SqlSmallInt, SqlULen,
};
use crate::api::set_desc_field::datetime_interval_code_for;
use crate::api::type_rules::{parameter_size_is_precision, resolve_default_c_type};
use crate::conversion::parameter_value_stride;
use crate::handles::OdbcVersion;
use crate::handles::desc::{DescRecord, DescState};

/// A bound parameter — the lightweight equivalent of msodbcsql's implicit
/// APD + IPD records (`cmdp.APD`), populated by `SQLBindParameter`.
///
/// ODBC binds parameters **by reference**: the application's value buffer and
/// its length/indicator buffer are read at `SQLExecute` time, not at bind time.
/// The raw pointers are stored here and dereferenced during execution. The
/// caller owns those buffers and must keep them valid (and unchanged in
/// location) until execution completes.
///
/// AB#47437: the APD/IPD descriptor records are the actual storage
/// `SQLBindParameter` and `SQLSetDescFieldW` share; this struct is no longer
/// stored continuously on `StmtState`, but reconstructed as a one-time
/// snapshot from those records immediately before each execute (see
/// [`Self::all_from_descriptor_states`]) — the shape `build_named_params` and
/// the data-at-execution lookups already expect.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct BoundParam {
    /// `SQL_PARAM_INPUT` / `SQL_PARAM_INPUT_OUTPUT` / `SQL_PARAM_OUTPUT`.
    pub(crate) input_output_type: SqlSmallInt,
    /// C data type of the application buffer (ODBC `ValueType`, `SQL_C_*`),
    /// with `SQL_C_DEFAULT` already resolved to a concrete type.
    pub(crate) c_type: SqlSmallInt,
    /// SQL data type of the column/expression (ODBC `ParameterType`, `SQL_*`).
    pub(crate) sql_type: SqlSmallInt,
    /// Column size (precision) as passed by the application.
    pub(crate) column_size: SqlULen,
    /// Decimal digits (scale) as passed by the application.
    pub(crate) decimal_digits: SqlSmallInt,
    pub(crate) app_precision: SqlSmallInt,
    pub(crate) app_scale: SqlSmallInt,
    /// Whether the application itself wrote the APD's `SQL_DESC_PRECISION`/
    /// `SQL_DESC_SCALE` (`SQLSetDescField`/`SQLSetDescRec`), rather than
    /// merely inheriting this driver's own default-fill. See
    /// `DescRecord::precision_scale_explicit`.
    pub(crate) precision_scale_explicit: bool,
    /// Pointer to the application's value buffer (read at execute time).
    pub(crate) parameter_value_ptr: *mut c_void,
    /// Length in bytes of the application value buffer.
    pub(crate) buffer_length: SqlLen,
    /// Pointer to the application's length/indicator buffer (read at execute
    /// time). May be null.
    pub(crate) strlen_or_ind_ptr: *mut SqlLen,
    /// Pointer to the application's octet-length/data-at-execution buffer,
    /// independent of `strlen_or_ind_ptr` per the ODBC "Deferred Fields" spec
    /// (`SQL_DESC_OCTET_LENGTH_PTR` carries the length or a DAE sentinel;
    /// `SQL_DESC_INDICATOR_PTR` carries only `SQL_NULL_DATA` status).
    /// `SQLBindParameter` writes the same pointer to both — see
    /// [`Self::write_to_records`] — but `SQLSetDescFieldW`/`SQLSetDescRec`
    /// can set them independently. Null means "assume NUL-terminated" for a
    /// character parameter.
    pub(crate) octet_length_ptr: *mut SqlLen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamArrayLayoutError {
    InvalidValueStride {
        c_type: SqlSmallInt,
        buffer_length: SqlLen,
    },
}

impl BoundParam {
    /// Returns the binding with `SQL_ATTR_PARAM_BIND_OFFSET_PTR` applied.
    ///
    /// ODBC adds the offset, in bytes, to the value pointer and the
    /// length/indicator pointer alike, which lets an application walk a
    /// binding across a buffer without rebinding it. A null pointer stays
    /// null — the offset addresses a buffer that was never supplied, and
    /// offsetting null would turn "no indicator" into a wild pointer.
    pub(crate) fn with_bind_offset(mut self, offset: isize) -> Self {
        if offset == 0 {
            return self;
        }
        if !self.parameter_value_ptr.is_null() {
            self.parameter_value_ptr = self.parameter_value_ptr.wrapping_byte_offset(offset);
        }
        if !self.strlen_or_ind_ptr.is_null() {
            self.strlen_or_ind_ptr = self.strlen_or_ind_ptr.wrapping_byte_offset(offset);
        }
        if !self.octet_length_ptr.is_null() {
            self.octet_length_ptr = self.octet_length_ptr.wrapping_byte_offset(offset);
        }
        self
    }

    /// Positions a binding on one parameter-set row.
    pub(crate) fn for_row(
        self,
        row: usize,
        bind_offset: isize,
        param_bind_type: SqlULen,
    ) -> Result<Self, ParamArrayLayoutError> {
        let mut positioned = self.with_bind_offset(bind_offset);
        if row == 0 {
            return Ok(positioned);
        }
        let (value_stride, indicator_stride) = if param_bind_type == SQL_BIND_BY_COLUMN {
            let Some(value_stride) = parameter_value_stride(self.c_type, self.buffer_length) else {
                return Err(ParamArrayLayoutError::InvalidValueStride {
                    c_type: self.c_type,
                    buffer_length: self.buffer_length,
                });
            };
            (value_stride, std::mem::size_of::<SqlLen>())
        } else {
            let row_stride = param_bind_type;
            (row_stride, row_stride)
        };
        let value_offset = row.wrapping_mul(value_stride);
        let indicator_offset = row.wrapping_mul(indicator_stride);
        if !positioned.parameter_value_ptr.is_null() {
            positioned.parameter_value_ptr = positioned
                .parameter_value_ptr
                .wrapping_byte_add(value_offset);
        }
        if !positioned.strlen_or_ind_ptr.is_null() {
            positioned.strlen_or_ind_ptr = positioned
                .strlen_or_ind_ptr
                .wrapping_byte_add(indicator_offset);
        }
        if !positioned.octet_length_ptr.is_null() {
            positioned.octet_length_ptr = positioned
                .octet_length_ptr
                .wrapping_byte_add(indicator_offset);
        }
        Ok(positioned)
    }

    /// Writes this binding into the matching APD and IPD records at the same
    /// ordinal — the descriptor storage `SQLBindParameter` and
    /// `SQLSetDescFieldW` share (AB#47437). Mirrors msodbcsql's
    /// `SQLBindParameter`, which unconditionally writes both `SetADRec` (APD)
    /// and `SetIPDRec` (IPD) on every call, so a rebind fully overwrites the
    /// IPD's `length`/`precision`/`scale` rather than leaving fields from a
    /// previous, differently-shaped binding behind.
    ///
    /// msodbcsql's `SetADRecBP` also re-applies `SetTypeDefaults`'s
    /// C-type-keyed defaults on every call (`sqlcdesc.cpp:2883`); for
    /// `SQL_C_NUMERIC` that default is `(SQL_PREC_NUMERIC, 0)`
    /// (`sqlcdesc.cpp:12344`, `case SQL_NUMERIC: pGenDescRec->cbPrecision =
    /// SQL_PREC_NUMERIC; pGenDescRec->ibScale = 0;` — `SQL_C_NUMERIC` and
    /// `SQL_NUMERIC` share the same integer value). This driver only tracks
    /// the (execution-critical, per `decimal_from_numeric`) precision/scale
    /// pair for that one C type, so the value reset below is scoped to it;
    /// every other C type's APD precision/scale stays at whatever
    /// `DescRecord::default_for` seeded (`0, 0`), which is harmless because
    /// nothing reads them. Without this reset, a `SQL_C_NUMERIC` bind that
    /// never calls `SQLSetDescFieldW` left `app_precision`/`app_scale` at
    /// `(0, 0)` instead of msodbcsql's real `(38, 0)` — silently forcing the
    /// slow (rescale) path even for the common "bind straight into a
    /// `NUMERIC(38,0)` column" case msodbcsql fast-paths.
    ///
    /// `precision_scale_explicit` is *not* part of that per-call value
    /// reset, though: per the ODBC spec, rebinding the same `ValueType`
    /// (`SQLBindParameter`'s `fCType`) retains other APD fields set by a
    /// prior bind or `SQLSetDescField` call, so it only clears when the C
    /// type is genuinely changing (verified against msodbcsql's parity-
    /// comparison harness —
    /// `NumericRebindDoesNotInheritAPreviousBindsStaleApdScale` regressed
    /// when this flag was cleared unconditionally, since a same-type
    /// rebind's freshly-defaulted `(38, 0)` values must still be treated as
    /// the deliberate, "app is in control" state a prior explicit call left
    /// behind, not as an unset default).
    ///
    /// `SQL_DESC_INDICATOR_PTR` and `SQL_DESC_OCTET_LENGTH_PTR`
    /// both receive the same pointer here — `SQLBindParameter`'s one
    /// `StrLen_or_IndPtr` argument feeds both descriptor fields at once,
    /// mirroring msodbcsql's `lpbindinfo->pIndValue = lpbindinfo->pcbValue =
    /// pcbValue` (`sqlcdesc.cpp`) — but they stay two independent fields on
    /// the record, since `SQLSetDescFieldW`/`SQLSetDescRec` can set them to
    /// different buffers.
    pub(crate) fn write_to_records(
        &self,
        apd_record: &mut DescRecord,
        ipd_record: &mut DescRecord,
    ) {
        // Per the ODBC spec's SQLBindParameter rebind rule: rebinding the
        // *same* ValueType retains other APD fields from a previous bind or
        // SQLSetDescField call; only a genuine ValueType change resets them
        // to type defaults. `precision_scale_explicit` therefore only clears
        // here when the C type is actually changing — a same-type rebind
        // (even a bare one) must not forget that the app once wrote
        // SQL_DESC_PRECISION/SCALE, or `decimal_from_numeric`'s fast-path
        // gate would wrongly treat it as never-explicit.
        let type_changing = apd_record.concise_type != self.c_type;
        apd_record.concise_type = self.c_type;
        apd_record.datetime_interval_code = datetime_interval_code_for(self.c_type);
        apd_record.data_ptr = self.parameter_value_ptr;
        apd_record.data_bound = true;
        apd_record.octet_length = self.buffer_length;
        apd_record.indicator_ptr = self.strlen_or_ind_ptr as SqlPointer;
        apd_record.octet_length_ptr = self.octet_length_ptr as SqlPointer;
        if type_changing {
            apd_record.precision_scale_explicit = false;
        }
        // The precision/scale *values* reset to SQL_C_NUMERIC's type default
        // on every bind regardless (msodbcsql's SetTypeDefaults), even a
        // same-type rebind — only the explicit-flag survives one.
        if self.c_type == SQL_C_NUMERIC {
            apd_record.precision = SQL_PREC_NUMERIC;
            apd_record.scale = 0;
        }

        ipd_record.parameter_type = self.input_output_type;
        ipd_record.concise_type = self.sql_type;
        ipd_record.datetime_interval_code = datetime_interval_code_for(self.sql_type);
        ipd_record.scale = self.decimal_digits;
        if parameter_size_is_precision(self.sql_type) {
            ipd_record.precision =
                SqlSmallInt::try_from(self.column_size).unwrap_or(SqlSmallInt::MAX);
            ipd_record.length = 0;
        } else if ipd_record.datetime_interval_code != 0 {
            // Per ODBC's "Decimal Digits" appendix ("All datetime types" ->
            // PRECISION): DecimalDigits (fractional-seconds precision) also
            // belongs in SQL_DESC_PRECISION for the datetime family, matching
            // `api::ird::ird_record_from_metadata`'s identical redirection
            // (`col_attribute::precision()`) for the equivalent result
            // column. This driver stores precision/scale as independent
            // fields (`get_desc_field.rs` reads each directly, no type-based
            // redirection), so leaving precision at 0 here would make
            // SQLGetDescField/SQLGetDescRecW disagree with the IRD for the
            // same logical type.
            ipd_record.precision = self.decimal_digits;
            ipd_record.length = self.column_size;
        } else {
            ipd_record.length = self.column_size;
            ipd_record.precision = 0;
        }
        // SQLBindParameter is an explicit application choice: describe_param.rs's
        // refine_ipd must never override it with the server's informational answer.
        ipd_record.explicitly_bound = true;
    }

    /// Reconstructs the binding an APD/IPD record pair represents, or `None`
    /// when no application value binding is active.
    ///
    /// `SQL_C_DEFAULT` alone cannot mean "unbound": it is also a valid value
    /// `SQLSetDescFieldW`/`SQLSetDescRec` can write to `SQL_DESC_CONCISE_TYPE`
    /// intentionally, asking the driver to resolve the C type from the
    /// paired IPD's SQL type at execute time, exactly as
    /// `sql_bind_parameter_safe` resolves it before ever writing to the APD.
    /// An active binding with `SQL_C_DEFAULT` is resolved here via
    /// [`resolve_default_c_type`].
    ///
    /// `data_bound` is separate from `data_ptr` because `SQLBindParameter`
    /// legitimately accepts a null `ParameterValuePtr` for DAE parameters.
    ///
    /// A missing IPD record (an APD-only binding set up through
    /// `SQLSetDescFieldW` without ever touching IPD) defaults to
    /// `SQL_PARAM_INPUT` with no SQL-type information; `build_named_params`
    /// already reports an unknown SQL type as its own diagnostic.
    pub(crate) fn from_records(
        apd_record: &DescRecord,
        ipd_record: Option<&DescRecord>,
        odbc_version: OdbcVersion,
    ) -> Option<Self> {
        if !apd_record.data_bound {
            return None;
        }
        let (input_output_type, sql_type, column_size, decimal_digits) = match ipd_record {
            Some(ipd) => (
                ipd.parameter_type,
                ipd.concise_type,
                if parameter_size_is_precision(ipd.concise_type) {
                    SqlULen::try_from(ipd.precision.max(0)).unwrap_or(0)
                } else {
                    ipd.length
                },
                ipd.scale,
            ),
            None => (SQL_PARAM_INPUT, 0, 0, 0),
        };
        let c_type = if apd_record.concise_type == SQL_C_DEFAULT {
            resolve_default_c_type(sql_type, odbc_version).unwrap_or(apd_record.concise_type)
        } else {
            apd_record.concise_type
        };
        Some(Self {
            input_output_type,
            c_type,
            sql_type,
            column_size,
            decimal_digits,
            app_precision: apd_record.precision,
            app_scale: apd_record.scale,
            precision_scale_explicit: apd_record.precision_scale_explicit,
            parameter_value_ptr: apd_record.data_ptr,
            buffer_length: apd_record.octet_length,
            strlen_or_ind_ptr: apd_record.indicator_ptr as *mut SqlLen,
            octet_length_ptr: apd_record.octet_length_ptr as *mut SqlLen,
        })
    }

    /// Every parameter position in `apd_state`, in ordinal order, paired with
    /// its IPD twin at the same position — `build_named_params`'s input,
    /// snapshotted fresh from the active APD/IPD immediately before an
    /// execute and never while the STMT lock is held (see
    /// ".github/instructions/mssql-odbc.instructions.md", "Locking rules": a
    /// STMT lock must never be held while acquiring a DESC lock). `None`
    /// slots are gaps: an ordinal never bound, or unbound since.
    pub(crate) fn all_from_descriptor_states(
        apd_state: &DescState,
        ipd_state: &DescState,
        odbc_version: OdbcVersion,
    ) -> Vec<Option<Self>> {
        apd_state
            .records
            .iter()
            .enumerate()
            .map(|(i, apd_record)| {
                Self::from_records(apd_record, ipd_state.records.get(i), odbc_version)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_BIND_BY_COLUMN, SQL_C_CHAR, SQL_C_SLONG, SQL_PARAM_INPUT, SQL_VARCHAR,
    };
    use crate::handles::desc::{DescHeader, DescKind};

    const ODBC_VERSION: OdbcVersion = OdbcVersion::Odbc3_80;

    fn param(value: *mut c_void, ind: *mut SqlLen) -> BoundParam {
        BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_CHAR,
            sql_type: SQL_VARCHAR,
            column_size: 8,
            decimal_digits: 0,
            app_precision: 0,
            app_scale: 0,
            precision_scale_explicit: false,
            parameter_value_ptr: value,
            buffer_length: 8,
            strlen_or_ind_ptr: ind,
            octet_length_ptr: ind,
        }
    }

    /// The offset moves both pointers by the same byte count, which is what
    /// lets an application step a whole binding across a buffer.
    #[test]
    fn bind_offset_shifts_value_and_indicator_together() {
        let mut buf = [0u8; 32];
        let mut ind: SqlLen = 4;
        let value = buf.as_mut_ptr().cast::<c_void>();
        let shifted = param(value, &raw mut ind).with_bind_offset(16);
        assert_eq!(shifted.parameter_value_ptr as usize, value as usize + 16);
        assert_eq!(
            shifted.strlen_or_ind_ptr as usize,
            (&raw mut ind) as usize + 16
        );
    }

    /// A negative offset is legal: ODBC does not constrain the direction.
    #[test]
    fn bind_offset_may_be_negative() {
        let mut buf = [0u8; 32];
        let value = unsafe { buf.as_mut_ptr().add(16) }.cast::<c_void>();
        let shifted = param(value, std::ptr::null_mut()).with_bind_offset(-16);
        assert_eq!(shifted.parameter_value_ptr as usize, value as usize - 16);
    }

    /// A null indicator means "no indicator supplied", so offsetting it would
    /// manufacture a wild pointer out of the absence of one.
    #[test]
    fn bind_offset_leaves_a_null_indicator_null() {
        let mut buf = [0u8; 32];
        let shifted = param(buf.as_mut_ptr().cast(), std::ptr::null_mut()).with_bind_offset(8);
        assert!(shifted.strlen_or_ind_ptr.is_null());
    }

    /// The overwhelmingly common case, and the one that must stay free.
    #[test]
    fn zero_offset_returns_the_binding_unchanged() {
        let mut buf = [0u8; 32];
        let mut ind: SqlLen = 4;
        let original = param(buf.as_mut_ptr().cast(), &raw mut ind);
        let shifted = original.with_bind_offset(0);
        assert_eq!(
            shifted.parameter_value_ptr as usize,
            original.parameter_value_ptr as usize
        );
        assert_eq!(
            shifted.strlen_or_ind_ptr as usize,
            original.strlen_or_ind_ptr as usize
        );
    }

    #[test]
    fn column_wise_rows_use_value_and_indicator_strides() {
        let mut values = [0i32; 3];
        let mut indicators = [0 as SqlLen; 3];
        let original = BoundParam {
            c_type: SQL_C_SLONG,
            parameter_value_ptr: values.as_mut_ptr().cast(),
            buffer_length: 128,
            strlen_or_ind_ptr: indicators.as_mut_ptr(),
            octet_length_ptr: indicators.as_mut_ptr(),
            ..param(std::ptr::null_mut(), std::ptr::null_mut())
        };

        let positioned = original
            .for_row(2, 0, SQL_BIND_BY_COLUMN)
            .expect("fixed-width column binding has a known stride");
        assert_eq!(
            positioned.parameter_value_ptr as usize,
            original.parameter_value_ptr as usize + 2 * size_of::<i32>()
        );
        assert_eq!(positioned.strlen_or_ind_ptr, unsafe {
            indicators.as_mut_ptr().add(2)
        });
    }

    #[test]
    fn row_wise_rows_use_the_structure_stride_for_every_pointer() {
        #[repr(C)]
        struct Row {
            value: i32,
            indicator: SqlLen,
        }

        let mut rows = [
            Row {
                value: 1,
                indicator: size_of::<i32>() as SqlLen,
            },
            Row {
                value: 2,
                indicator: size_of::<i32>() as SqlLen,
            },
        ];
        let original = BoundParam {
            c_type: SQL_C_SLONG,
            parameter_value_ptr: (&raw mut rows[0].value).cast(),
            buffer_length: size_of::<i32>() as SqlLen,
            strlen_or_ind_ptr: &raw mut rows[0].indicator,
            octet_length_ptr: &raw mut rows[0].indicator,
            ..param(std::ptr::null_mut(), std::ptr::null_mut())
        };

        let positioned = original
            .for_row(1, 0, size_of::<Row>() as SqlULen)
            .expect("row-wise binding uses the declared structure stride");
        assert_eq!(
            positioned.parameter_value_ptr,
            (&raw mut rows[1].value).cast()
        );
        assert_eq!(positioned.strlen_or_ind_ptr, &raw mut rows[1].indicator);
    }

    fn empty_state() -> DescState {
        DescState {
            diag_records: Vec::new(),
            header: DescHeader::default(),
            records: Vec::new(),
        }
    }

    /// `write_to_records` must split fields onto the correct side: C type and
    /// the value/indicator pointers on APD, SQL type/parameter direction/size
    /// on IPD — never the other way around.
    #[test]
    fn write_to_records_splits_apd_and_ipd_fields() {
        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 8;
        let bound = param(buf.as_mut_ptr().cast(), &raw mut ind);
        let mut apd_record = DescRecord::default_for(DescKind::AppParam);
        let mut ipd_record = DescRecord::default_for(DescKind::ImpParam);
        bound.write_to_records(&mut apd_record, &mut ipd_record);

        assert_eq!(apd_record.concise_type, SQL_C_CHAR);
        assert_eq!(apd_record.data_ptr, buf.as_mut_ptr().cast());
        assert_eq!(apd_record.octet_length, 8);
        assert_eq!(apd_record.indicator_ptr, (&raw mut ind).cast());
        assert_eq!(apd_record.octet_length_ptr, (&raw mut ind).cast());

        assert_eq!(ipd_record.parameter_type, SQL_PARAM_INPUT);
        assert_eq!(ipd_record.concise_type, SQL_VARCHAR);
        assert_eq!(ipd_record.length, 8);
        assert_eq!(ipd_record.precision, 0);
    }

    /// `SQLBindParameter(..., SQL_C_NUMERIC, ...)` with no follow-up
    /// `SQLSetDescFieldW` call must land the APD on msodbcsql's real
    /// `SetTypeDefaults` default — `(SQL_PREC_NUMERIC, 0)`, i.e. `(38, 0)` —
    /// not `(0, 0)`. Before this reset, a fresh bind into a `NUMERIC(38, 0)`
    /// column wrongly mismatched that default and took the slow (rescale)
    /// path where msodbcsql fast-paths, and misread a struct with a nonzero
    /// scale as scale `0` once there.
    #[test]
    fn write_to_records_resets_numeric_apd_precision_and_scale_to_msodbcsql_default() {
        let mut value = crate::api::odbc_types::SqlNumericStruct::default();
        let mut ind: SqlLen = std::mem::size_of_val(&value) as SqlLen;
        let bound = BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: crate::api::odbc_types::SQL_C_NUMERIC,
            sql_type: crate::api::odbc_types::SQL_DECIMAL,
            column_size: 38,
            decimal_digits: 0,
            app_precision: 0,
            app_scale: 0,
            precision_scale_explicit: false,
            parameter_value_ptr: (&raw mut value).cast(),
            buffer_length: ind,
            strlen_or_ind_ptr: &raw mut ind,
            octet_length_ptr: &raw mut ind,
        };
        // Seed the record as `default_for` would leave a genuinely unbound
        // one (`concise_type: SQL_C_DEFAULT`), but with stale explicit
        // precision/scale left behind — as if a *different* prior ValueType
        // had set them — to prove the value reset happens on every call and
        // the flag resets too, since the ValueType is changing here.
        let mut apd_record = DescRecord {
            precision: 7,
            scale: 2,
            precision_scale_explicit: true,
            ..DescRecord::default_for(DescKind::AppParam)
        };
        let mut ipd_record = DescRecord::default_for(DescKind::ImpParam);
        bound.write_to_records(&mut apd_record, &mut ipd_record);

        assert_eq!(apd_record.precision, SQL_PREC_NUMERIC);
        assert_eq!(apd_record.scale, 0);
        assert!(
            !apd_record.precision_scale_explicit,
            "a rebind that changes ValueType away from SQL_C_NUMERIC (the \
             seeded record's SQL_C_DEFAULT) must not inherit a prior bind's \
             explicit precision/scale flag"
        );
    }

    /// The ODBC-spec counterpart of the test above: rebinding the *same*
    /// `ValueType` (`SQL_C_NUMERIC` again) must retain `precision_scale_explicit`
    /// from a prior explicit `SQLSetDescField` call even though the
    /// precision/scale *values* still reset to `SetTypeDefaults`'
    /// `(SQL_PREC_NUMERIC, 0)` — verified against msodbcsql's parity harness
    /// (`NumericRebindDoesNotInheritAPreviousBindsStaleApdScale`): a bare
    /// second `SQLBindParameter(..., SQL_C_NUMERIC, ...)` after a first bind
    /// that wrote APD precision/scale explicitly must still use the APD's
    /// (now-defaulted) scale as the rescale source, not the second struct's
    /// own embedded scale — which only happens if this flag survives.
    #[test]
    fn write_to_records_keeps_explicit_flag_across_a_same_type_rebind() {
        let mut value = crate::api::odbc_types::SqlNumericStruct::default();
        let mut ind: SqlLen = std::mem::size_of_val(&value) as SqlLen;
        let bound = BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: crate::api::odbc_types::SQL_C_NUMERIC,
            sql_type: crate::api::odbc_types::SQL_DECIMAL,
            column_size: 10,
            decimal_digits: 2,
            app_precision: 0,
            app_scale: 0,
            precision_scale_explicit: false,
            parameter_value_ptr: (&raw mut value).cast(),
            buffer_length: ind,
            strlen_or_ind_ptr: &raw mut ind,
            octet_length_ptr: &raw mut ind,
        };
        // Seed the record as a prior SQL_C_NUMERIC bind that had SQLSetDescFieldW
        // called on it would leave it: same concise_type, explicit precision/scale.
        let mut apd_record = DescRecord {
            concise_type: crate::api::odbc_types::SQL_C_NUMERIC,
            precision: 10,
            scale: 2,
            precision_scale_explicit: true,
            ..DescRecord::default_for(DescKind::AppParam)
        };
        let mut ipd_record = DescRecord::default_for(DescKind::ImpParam);
        bound.write_to_records(&mut apd_record, &mut ipd_record);

        assert_eq!(apd_record.precision, SQL_PREC_NUMERIC);
        assert_eq!(apd_record.scale, 0);
        assert!(
            apd_record.precision_scale_explicit,
            "a same-ValueType SQL_C_NUMERIC rebind must retain a prior \
             explicit precision/scale flag, per the ODBC SQLBindParameter \
             rebind rule"
        );
    }

    /// Per ODBC's "Decimal Digits" appendix, `DecimalDigits` for the whole
    /// datetime family belongs in `SQL_DESC_PRECISION`, not (only)
    /// `SQL_DESC_SCALE` — matching `api::ird::ird_record_from_metadata`'s
    /// identical redirection for the equivalent result column. A
    /// `datetime2(7)` parameter (`SQL_TYPE_TIMESTAMP`, `DecimalDigits = 7`)
    /// must report `7` from `SQL_DESC_PRECISION`, not `0`.
    #[test]
    fn write_to_records_puts_datetime_decimal_digits_in_precision() {
        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 8;
        let bound = BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_CHAR,
            sql_type: crate::api::odbc_types::SQL_TYPE_TIMESTAMP,
            column_size: 27,
            decimal_digits: 7,
            app_precision: 0,
            app_scale: 0,
            precision_scale_explicit: false,
            parameter_value_ptr: buf.as_mut_ptr().cast(),
            buffer_length: 8,
            strlen_or_ind_ptr: &raw mut ind,
            octet_length_ptr: &raw mut ind,
        };
        let mut apd_record = DescRecord::default_for(DescKind::AppParam);
        let mut ipd_record = DescRecord::default_for(DescKind::ImpParam);
        bound.write_to_records(&mut apd_record, &mut ipd_record);

        assert_eq!(
            ipd_record.precision, 7,
            "fractional-seconds precision must land on SQL_DESC_PRECISION"
        );
        assert_eq!(ipd_record.scale, 7);
        assert_eq!(ipd_record.length, 27, "ColumnSize still lands on length");
    }

    /// A freshly-grown record (never bound, just a gap created by binding a
    /// higher ordinal first) has `SQL_C_DEFAULT` as its APD concise type —
    /// `DescRecord::default_for`'s own placeholder — paired with a null
    /// `data_ptr`, and must read as unbound.
    #[test]
    fn from_records_treats_default_c_type_as_unbound() {
        let apd_record = DescRecord::default_for(DescKind::AppParam);
        assert_eq!(apd_record.concise_type, SQL_C_DEFAULT);
        assert!(apd_record.data_ptr.is_null());
        assert!(BoundParam::from_records(&apd_record, None, ODBC_VERSION).is_none());
    }

    /// A data-at-execution parameter legitimately has a null
    /// `ParameterValuePtr` — `SQLBindParameter` returns the pointer value
    /// itself via `SQLParamData` to identify which parameter needs data
    /// next, and passing null is common when the app just wants the ordinal.
    /// Keying "unbound" off a null value pointer would misreport every such
    /// binding as unbound; `data_bound` preserves the distinction.
    #[test]
    fn from_records_treats_a_null_value_pointer_as_bound_when_a_real_c_type_is_set() {
        let apd_record = DescRecord {
            concise_type: SQL_C_CHAR,
            data_ptr: std::ptr::null_mut(),
            data_bound: true,
            ..DescRecord::default_for(DescKind::AppParam)
        };
        let ipd_record = DescRecord {
            concise_type: SQL_VARCHAR,
            parameter_type: SQL_PARAM_INPUT,
            ..DescRecord::default_for(DescKind::ImpParam)
        };
        let bound = BoundParam::from_records(&apd_record, Some(&ipd_record), ODBC_VERSION)
            .expect("DAE param is bound");
        assert!(bound.parameter_value_ptr.is_null());
        assert_eq!(bound.c_type, SQL_C_CHAR);
        assert_eq!(bound.sql_type, SQL_VARCHAR);
    }

    #[test]
    fn from_records_ignores_stale_dae_pointers_after_unbind() {
        let mut stale_indicator = crate::api::odbc_types::SQL_DATA_AT_EXEC;
        let apd_record = DescRecord {
            concise_type: crate::api::odbc_types::SQL_C_NUMERIC,
            indicator_ptr: (&raw mut stale_indicator).cast(),
            octet_length_ptr: (&raw mut stale_indicator).cast(),
            data_bound: false,
            ..DescRecord::default_for(DescKind::AppParam)
        };

        assert!(BoundParam::from_records(&apd_record, None, ODBC_VERSION).is_none());
    }

    #[test]
    fn from_records_keeps_numeric_source_and_target_metadata_distinct() {
        let mut value = crate::api::odbc_types::SqlNumericStruct::default();
        let apd_record = DescRecord {
            concise_type: crate::api::odbc_types::SQL_C_NUMERIC,
            precision: 7,
            scale: 2,
            data_ptr: (&raw mut value).cast(),
            data_bound: true,
            ..DescRecord::default_for(DescKind::AppParam)
        };
        let ipd_record = DescRecord {
            concise_type: crate::api::odbc_types::SQL_DECIMAL,
            parameter_type: SQL_PARAM_INPUT,
            precision: 12,
            scale: 4,
            ..DescRecord::default_for(DescKind::ImpParam)
        };

        let bound = BoundParam::from_records(&apd_record, Some(&ipd_record), ODBC_VERSION)
            .expect("numeric param is bound");
        assert_eq!((bound.app_precision, bound.app_scale), (7, 2));
        assert_eq!((bound.column_size, bound.decimal_digits), (12, 4));
    }

    /// `SQL_C_DEFAULT` is a valid value `SQLSetDescFieldW`/`SQLSetDescRec` can
    /// write to `SQL_DESC_CONCISE_TYPE` intentionally — it means "resolve the
    /// C type from the paired IPD's SQL type", exactly what
    /// `sql_bind_parameter_safe` itself does before ever writing to the APD.
    /// A non-null `data_ptr` alongside `SQL_C_DEFAULT` must therefore resolve
    /// to a real binding, not read as the growth-placeholder "unbound" case.
    #[test]
    fn from_records_resolves_sql_c_default_from_the_paired_ipd_when_a_data_pointer_is_set() {
        let mut buf = 0i32;
        let apd_record = DescRecord {
            concise_type: SQL_C_DEFAULT,
            data_ptr: &raw mut buf as SqlPointer,
            data_bound: true,
            ..DescRecord::default_for(DescKind::AppParam)
        };
        let ipd_record = DescRecord {
            concise_type: crate::api::odbc_types::SQL_INTEGER,
            parameter_type: SQL_PARAM_INPUT,
            ..DescRecord::default_for(DescKind::ImpParam)
        };
        let bound = BoundParam::from_records(&apd_record, Some(&ipd_record), ODBC_VERSION)
            .expect("SQL_C_DEFAULT with a data pointer is bound, not unbound");
        assert_eq!(bound.c_type, crate::api::odbc_types::SQL_C_SLONG);
    }

    /// `SQL_DESC_INDICATOR_PTR` and `SQL_DESC_OCTET_LENGTH_PTR` are
    /// independent fields (ODBC "Deferred Fields"): a descriptor-driven bind
    /// that sets them to different buffers must round-trip both, not
    /// silently collapse to one.
    #[test]
    fn from_records_preserves_independent_indicator_and_octet_length_pointers() {
        let mut indicator: SqlLen = 0;
        let mut octet_length: SqlLen = 3;
        let mut buf = [0u8; 8];
        let apd_record = DescRecord {
            concise_type: SQL_C_CHAR,
            data_ptr: buf.as_mut_ptr().cast(),
            indicator_ptr: (&raw mut indicator).cast(),
            octet_length_ptr: (&raw mut octet_length).cast(),
            data_bound: true,
            ..DescRecord::default_for(DescKind::AppParam)
        };
        let bound = BoundParam::from_records(&apd_record, None, ODBC_VERSION)
            .expect("a real data pointer is bound");
        assert_eq!(bound.strlen_or_ind_ptr, &raw mut indicator);
        assert_eq!(bound.octet_length_ptr, &raw mut octet_length);
    }

    /// `all_from_descriptor_states` pairs each APD record with its IPD twin
    /// at the same position, and reports a gap (never bound, or a position
    /// beyond the last real bind) as `None` without panicking on an IPD
    /// that's shorter than the APD.
    #[test]
    fn all_from_descriptor_states_pairs_apd_and_ipd_by_position() {
        let mut apd_state = empty_state();
        let mut ipd_state = empty_state();
        // Position 1: fully bound (APD + IPD both present).
        let mut apd_one = DescRecord::default_for(DescKind::AppParam);
        let mut ipd_one = DescRecord::default_for(DescKind::ImpParam);
        param(std::ptr::null_mut(), std::ptr::null_mut())
            .write_to_records(&mut apd_one, &mut ipd_one);
        apd_state.records.push(apd_one);
        ipd_state.records.push(ipd_one);
        // Position 2: never bound — a growth placeholder on the APD side,
        // with no IPD record allocated at all yet.
        apd_state
            .records
            .push(DescRecord::default_for(DescKind::AppParam));

        let params = BoundParam::all_from_descriptor_states(&apd_state, &ipd_state, ODBC_VERSION);
        assert_eq!(params.len(), 2);
        assert!(params[0].is_some());
        assert!(params[1].is_none());
    }
}
