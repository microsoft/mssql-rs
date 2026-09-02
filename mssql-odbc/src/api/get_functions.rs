// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetFunctions.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_API_ALL_FUNCTIONS, SQL_API_ALL_FUNCTIONS_SIZE, SQL_API_ODBC3_ALL_FUNCTIONS,
    SQL_API_SQLALLOCHANDLE, SQL_API_SQLBINDCOL, SQL_API_SQLBINDPARAMETER, SQL_API_SQLCANCEL,
    SQL_API_SQLCLOSECURSOR, SQL_API_SQLCOLATTRIBUTE, SQL_API_SQLCOLUMNS, SQL_API_SQLCONNECT,
    SQL_API_SQLDESCRIBECOL, SQL_API_SQLDESCRIBEPARAM, SQL_API_SQLDISCONNECT,
    SQL_API_SQLDRIVERCONNECT, SQL_API_SQLENDTRAN, SQL_API_SQLEXECDIRECT, SQL_API_SQLEXECUTE,
    SQL_API_SQLFETCH, SQL_API_SQLFETCHSCROLL, SQL_API_SQLFOREIGNKEYS, SQL_API_SQLFREEHANDLE,
    SQL_API_SQLFREESTMT, SQL_API_SQLGETCONNECTATTR, SQL_API_SQLGETDATA, SQL_API_SQLGETDESCFIELD,
    SQL_API_SQLGETDESCREC, SQL_API_SQLGETDIAGFIELD, SQL_API_SQLGETDIAGREC, SQL_API_SQLGETENVATTR,
    SQL_API_SQLGETFUNCTIONS, SQL_API_SQLGETINFO, SQL_API_SQLGETSTMTATTR, SQL_API_SQLGETTYPEINFO,
    SQL_API_SQLMORERESULTS, SQL_API_SQLNUMRESULTCOLS, SQL_API_SQLPARAMDATA, SQL_API_SQLPREPARE,
    SQL_API_SQLPRIMARYKEYS, SQL_API_SQLPROCEDURES, SQL_API_SQLPUTDATA, SQL_API_SQLROWCOUNT,
    SQL_API_SQLSETCONNECTATTR, SQL_API_SQLSETDESCFIELD, SQL_API_SQLSETDESCREC,
    SQL_API_SQLSETENVATTR, SQL_API_SQLSETSTMTATTR, SQL_API_SQLSPECIALCOLUMNS,
    SQL_API_SQLSTATISTICS, SQL_API_SQLTABLES, SQL_ERROR, SQL_FALSE, SQL_INVALID_HANDLE,
    SQL_SUCCESS, SQL_TRUE, SqlHandle, SqlReturn, SqlUSmallInt,
};
use crate::error::free_errors;
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

/// Returns function-support metadata for a connection handle.
///
/// # Safety
/// - `connection_handle` must be a valid DBC handle from `SQLAllocHandle`.
/// - `supported_ptr` must be writable for:
///   - one `SQLUSMALLINT` for normal function-id queries, or
///   - `SQL_API_ALL_FUNCTIONS_SIZE` words for `SQL_API_ALL_FUNCTIONS`, or
///   - `SQL_API_ODBC3_ALL_FUNCTIONS_SIZE` words for `SQL_API_ODBC3_ALL_FUNCTIONS`.
pub(crate) unsafe fn sql_get_functions(
    connection_handle: SqlHandle,
    function_id: SqlUSmallInt,
    supported_ptr: *mut SqlUSmallInt,
) -> SqlReturn {
    debug!(
        ?connection_handle,
        function_id,
        ?supported_ptr,
        "SQLGetFunctions called",
    );

    crate::ffi_entry!("SQLGetFunctions", unsafe {
        sql_get_functions_impl(connection_handle, function_id, supported_ptr)
    })
}

unsafe fn sql_get_functions_impl(
    connection_handle: SqlHandle,
    function_id: SqlUSmallInt,
    supported_ptr: *mut SqlUSmallInt,
) -> SqlReturn {
    if connection_handle.is_null() {
        error!("SQLGetFunctions: connection_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let dbc = unsafe { handle_from_raw::<DbcHandle>(connection_handle) };
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLGetFunctions: handle is not a DBC"
    );
    sql_get_functions_safe(dbc, function_id, supported_ptr)
}

fn sql_get_functions_safe(
    dbc: &DbcHandle,
    function_id: SqlUSmallInt,
    supported_ptr: *mut SqlUSmallInt,
) -> SqlReturn {
    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLGetFunctions: dbc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    // Matches msodbcsql (sqlcinfo.cpp): a null SupportedPtr is a benign no-op
    // returning SQL_SUCCESS rather than a diagnostic. The ODBC spec defines no
    // SQLSTATE for this case.
    if supported_ptr.is_null() {
        debug!("SQLGetFunctions: supported_ptr is null; returning SQL_SUCCESS (no-op)");
        return SQL_SUCCESS;
    }

    match function_id {
        SQL_API_ALL_FUNCTIONS => {
            // Legacy ODBC 2.x 100-word support array.
            let mut funcs = [0u16; SQL_API_ALL_FUNCTIONS_SIZE];
            for api in supported_function_ids() {
                let idx = *api as usize;
                if idx < funcs.len() {
                    funcs[idx] = SQL_TRUE;
                }
            }
            unsafe {
                std::ptr::copy_nonoverlapping(
                    funcs.as_ptr() as *const u8,
                    supported_ptr as *mut u8,
                    funcs.len() * std::mem::size_of::<u16>(),
                );
            }
            SQL_SUCCESS
        }
        SQL_API_ODBC3_ALL_FUNCTIONS => {
            let mut funcs = [0u16; crate::api::odbc_types::SQL_API_ODBC3_ALL_FUNCTIONS_SIZE];
            for api in supported_function_ids() {
                let idx = (*api as usize) >> 4;
                if idx < funcs.len() {
                    funcs[idx] |= 1u16 << ((*api as usize) & 0x000F);
                }
            }
            unsafe {
                std::ptr::copy_nonoverlapping(
                    funcs.as_ptr() as *const u8,
                    supported_ptr as *mut u8,
                    funcs.len() * std::mem::size_of::<u16>(),
                );
            }
            SQL_SUCCESS
        }
        _ => {
            let exists = if is_supported_function(function_id) {
                SQL_TRUE
            } else {
                SQL_FALSE
            };
            unsafe { std::ptr::write_unaligned(supported_ptr, exists) };
            SQL_SUCCESS
        }
    }
}

fn is_supported_function(function_id: SqlUSmallInt) -> bool {
    supported_function_ids().contains(&function_id)
}

fn supported_function_ids() -> &'static [SqlUSmallInt] {
    &[
        SQL_API_SQLCONNECT,
        SQL_API_SQLCANCEL,
        SQL_API_SQLDESCRIBECOL,
        SQL_API_SQLDESCRIBEPARAM,
        SQL_API_SQLDISCONNECT,
        SQL_API_SQLEXECDIRECT,
        SQL_API_SQLEXECUTE,
        SQL_API_SQLBINDCOL,
        SQL_API_SQLFETCH,
        SQL_API_SQLFETCHSCROLL,
        SQL_API_SQLFREESTMT,
        SQL_API_SQLNUMRESULTCOLS,
        SQL_API_SQLROWCOUNT,
        SQL_API_SQLDRIVERCONNECT,
        SQL_API_SQLGETDATA,
        SQL_API_SQLGETFUNCTIONS,
        SQL_API_SQLGETINFO,
        SQL_API_SQLGETTYPEINFO,
        SQL_API_SQLMORERESULTS,
        SQL_API_SQLALLOCHANDLE,
        SQL_API_SQLCLOSECURSOR,
        SQL_API_SQLFREEHANDLE,
        SQL_API_SQLGETCONNECTATTR,
        SQL_API_SQLGETDIAGFIELD,
        SQL_API_SQLGETDIAGREC,
        SQL_API_SQLGETENVATTR,
        SQL_API_SQLGETSTMTATTR,
        SQL_API_SQLSETCONNECTATTR,
        SQL_API_SQLSETSTMTATTR,
        SQL_API_SQLSETENVATTR,
        SQL_API_SQLPREPARE,
        SQL_API_SQLBINDPARAMETER,
        SQL_API_SQLPARAMDATA,
        SQL_API_SQLPUTDATA,
        SQL_API_SQLENDTRAN,
        SQL_API_SQLCOLATTRIBUTE,
        // Descriptor field/record access (AB#47297/AB#47435/AB#47437). Same
        // Windows DM trap as SQLGetTypeInfo/SQLColAttribute/the catalog
        // functions below: the SQLXxxW export exists and is implemented, but
        // the DM answers IM001 without calling the driver unless advertised
        // here.
        SQL_API_SQLGETDESCFIELD,
        SQL_API_SQLSETDESCFIELD,
        SQL_API_SQLGETDESCREC,
        SQL_API_SQLSETDESCREC,
        // Catalog functions (AB#46380). Same trap as SQLGetTypeInfo/SQLColAttribute
        // above: the Windows Driver Manager answers IM001 without ever calling the
        // driver unless each is advertised here, even though the SQLXxxW export
        // exists and is fully implemented.
        SQL_API_SQLTABLES,
        SQL_API_SQLCOLUMNS,
        SQL_API_SQLPRIMARYKEYS,
        SQL_API_SQLFOREIGNKEYS,
        SQL_API_SQLSPECIALCOLUMNS,
        SQL_API_SQLSTATISTICS,
        SQL_API_SQLPROCEDURES,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_API_ODBC3_ALL_FUNCTIONS_SIZE, SQL_NULL_HANDLE};
    use crate::test_support::TestHandles;

    // A function id the driver does not implement (not in `supported_function_ids`).
    const UNSUPPORTED_ID: SqlUSmallInt = 9999;

    #[test]
    fn null_handle_returns_invalid_handle() {
        let mut supported: SqlUSmallInt = SQL_TRUE;
        let ret = unsafe { sql_get_functions(SQL_NULL_HANDLE, SQL_API_SQLCONNECT, &mut supported) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn null_supported_ptr_is_noop_success() {
        let h = TestHandles::with_env_dbc();
        let ret = unsafe { sql_get_functions(h.dbc, SQL_API_SQLCONNECT, std::ptr::null_mut()) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn supported_function_reports_true() {
        let h = TestHandles::with_env_dbc();
        let mut supported: SqlUSmallInt = SQL_FALSE;
        let ret = unsafe { sql_get_functions(h.dbc, SQL_API_SQLEXECUTE, &mut supported) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(supported, SQL_TRUE);
    }

    // AB#46973: the Windows Driver Manager short-circuits SQLGetTypeInfo with
    // IM001 unless the driver advertises it here, even though SQLGetTypeInfoW is
    // exported and implemented.
    #[test]
    fn get_type_info_reports_true() {
        let h = TestHandles::with_env_dbc();
        let mut supported: SqlUSmallInt = SQL_FALSE;
        let ret = unsafe { sql_get_functions(h.dbc, SQL_API_SQLGETTYPEINFO, &mut supported) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(supported, SQL_TRUE);
    }

    #[test]
    fn describe_param_reports_true() {
        let h = TestHandles::with_env_dbc();
        let mut supported: SqlUSmallInt = SQL_FALSE;
        let ret = unsafe { sql_get_functions(h.dbc, SQL_API_SQLDESCRIBEPARAM, &mut supported) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(supported, SQL_TRUE);
    }

    // AB#46973 (scope follow-up): SQLSetStmtAttrW is exported and fully
    // implemented (shares set_stmt_attr.rs with the already-advertised
    // SQLGetStmtAttr), so the Windows DM must not short-circuit it with IM001.
    #[test]
    fn set_stmt_attr_reports_true() {
        let h = TestHandles::with_env_dbc();
        let mut supported: SqlUSmallInt = SQL_FALSE;
        let ret = unsafe { sql_get_functions(h.dbc, SQL_API_SQLSETSTMTATTR, &mut supported) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(supported, SQL_TRUE);
    }

    // The Windows DM refuses to dispatch SQLEndTran and SQLGetConnectAttrW with
    // IM001 unless both are advertised here, which makes every transaction
    // attribute unreadable from an application.
    #[test]
    fn transaction_functions_report_true() {
        let h = TestHandles::with_env_dbc();
        for id in [SQL_API_SQLENDTRAN, SQL_API_SQLGETCONNECTATTR] {
            let mut supported: SqlUSmallInt = SQL_FALSE;
            let ret = unsafe { sql_get_functions(h.dbc, id, &mut supported) };
            assert_eq!(ret, SQL_SUCCESS, "id {id}");
            assert_eq!(supported, SQL_TRUE, "id {id}");
        }
    }

    // Same trap as AB#46973: SQLColAttributeW is exported and implemented, but
    // the Windows Driver Manager answers IM001 without ever calling the driver
    // unless it is advertised here. unixODBC dispatches regardless, so this only
    // shows up on the Windows leg.
    #[test]
    fn col_attribute_reports_true() {
        let h = TestHandles::with_env_dbc();
        let mut supported: SqlUSmallInt = SQL_FALSE;
        let ret = unsafe { sql_get_functions(h.dbc, SQL_API_SQLCOLATTRIBUTE, &mut supported) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(supported, SQL_TRUE);
    }

    // AB#47297/AB#47435: SQLGetDescFieldW/SQLSetDescFieldW are exported and
    // fully implemented, but a client or Driver Manager that checks
    // SQLGetFunctions before calling (a common, spec-encouraged pattern
    // distinct from mssql-python's direct-symbol-resolution path) must not
    // see them reported as unsupported.
    #[test]
    fn descriptor_field_functions_report_true() {
        let h = TestHandles::with_env_dbc();
        for id in [
            SQL_API_SQLGETDESCFIELD,
            SQL_API_SQLSETDESCFIELD,
            SQL_API_SQLGETDESCREC,
            SQL_API_SQLSETDESCREC,
        ] {
            let mut supported: SqlUSmallInt = SQL_FALSE;
            let ret = unsafe { sql_get_functions(h.dbc, id, &mut supported) };
            assert_eq!(ret, SQL_SUCCESS, "id {id}");
            assert_eq!(supported, SQL_TRUE, "id {id}");
        }
    }

    // AB#46380: the seven catalog functions hit the exact same Windows DM trap
    // as SQLGetTypeInfo/SQLColAttribute above — each SQLXxxW export is
    // implemented, but IM001 short-circuits every call unless advertised here.
    #[test]
    fn catalog_functions_report_true() {
        let h = TestHandles::with_env_dbc();
        for id in [
            SQL_API_SQLTABLES,
            SQL_API_SQLCOLUMNS,
            SQL_API_SQLPRIMARYKEYS,
            SQL_API_SQLFOREIGNKEYS,
            SQL_API_SQLSPECIALCOLUMNS,
            SQL_API_SQLSTATISTICS,
            SQL_API_SQLPROCEDURES,
        ] {
            let mut supported: SqlUSmallInt = SQL_FALSE;
            let ret = unsafe { sql_get_functions(h.dbc, id, &mut supported) };
            assert_eq!(ret, SQL_SUCCESS, "id {id}");
            assert_eq!(supported, SQL_TRUE, "id {id}");
        }
    }

    /// The Windows Driver Manager will not dispatch an entry point it has not
    /// been told about, returning IM001 even though the export exists.
    #[test]
    fn fetch_scroll_reports_true() {
        let h = TestHandles::with_env_dbc();
        let mut supported: SqlUSmallInt = SQL_FALSE;
        let ret = unsafe { sql_get_functions(h.dbc, SQL_API_SQLFETCHSCROLL, &mut supported) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(supported, SQL_TRUE);
    }

    #[test]
    fn unsupported_function_reports_false() {
        let h = TestHandles::with_env_dbc();
        let mut supported: SqlUSmallInt = SQL_TRUE;
        let ret = unsafe { sql_get_functions(h.dbc, UNSUPPORTED_ID, &mut supported) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(supported, SQL_FALSE);
    }

    #[test]
    fn all_functions_fills_legacy_word_array() {
        let h = TestHandles::with_env_dbc();
        let mut funcs = [0u16; SQL_API_ALL_FUNCTIONS_SIZE];
        let ret = unsafe { sql_get_functions(h.dbc, SQL_API_ALL_FUNCTIONS, funcs.as_mut_ptr()) };
        assert_eq!(ret, SQL_SUCCESS);
        // SQLEXECUTE (12) is supported and fits the 0..100 legacy range.
        assert_eq!(funcs[SQL_API_SQLEXECUTE as usize], SQL_TRUE);
        // AB#46973: SQLGetTypeInfo (47) must also appear in the legacy array.
        assert_eq!(funcs[SQL_API_SQLGETTYPEINFO as usize], SQL_TRUE);
        // AB#46380: SQLTables (54) must also appear in the legacy array.
        assert_eq!(funcs[SQL_API_SQLTABLES as usize], SQL_TRUE);
        assert_eq!(funcs[SQL_API_SQLDESCRIBEPARAM as usize], SQL_TRUE);
        // Ids >= 100 (e.g. SQLALLOCHANDLE = 1001) never appear in this array.
        // An unoccupied slot stays zero.
        assert_eq!(funcs[2], SQL_FALSE);
    }

    #[test]
    fn odbc3_all_functions_sets_expected_bits() {
        let h = TestHandles::with_env_dbc();
        let mut funcs = [0u16; SQL_API_ODBC3_ALL_FUNCTIONS_SIZE];
        let ret =
            unsafe { sql_get_functions(h.dbc, SQL_API_ODBC3_ALL_FUNCTIONS, funcs.as_mut_ptr()) };
        assert_eq!(ret, SQL_SUCCESS);

        // SQL_FUNC_EXISTS bitmap: bit (id & 0xF) of word (id >> 4).
        let bit_set = |id: SqlUSmallInt| -> bool {
            let word = funcs[(id as usize) >> 4];
            word & (1u16 << ((id as usize) & 0x000F)) != 0
        };
        // A low id and a high id, both supported.
        assert!(bit_set(SQL_API_SQLCONNECT));
        assert!(bit_set(SQL_API_SQLALLOCHANDLE));
        // AB#46973: SQLGetTypeInfo (47) bit must be set in the ODBC3 bitmap.
        assert!(bit_set(SQL_API_SQLGETTYPEINFO));
        // AB#46380: SQLTables (54) bit must be set in the ODBC3 bitmap too.
        assert!(bit_set(SQL_API_SQLTABLES));
        assert!(bit_set(SQL_API_SQLDESCRIBEPARAM));
        // AB#46973 (scope follow-up): SQLSetStmtAttr (1020) bit must be set too.
        assert!(bit_set(SQL_API_SQLSETSTMTATTR));
        // AB#47297/AB#47435: SQLGetDescField (1008) / SQLSetDescField (1017)
        // bits must be set in the ODBC3 bitmap too.
        assert!(bit_set(SQL_API_SQLGETDESCFIELD));
        assert!(bit_set(SQL_API_SQLSETDESCFIELD));
        // AB#47437: SQLGetDescRec (1009) / SQLSetDescRec (1018) bits must be
        // set in the ODBC3 bitmap too.
        assert!(bit_set(SQL_API_SQLGETDESCREC));
        assert!(bit_set(SQL_API_SQLSETDESCREC));
        // An in-range unsupported id (2) keeps its bit clear.
        assert!(!bit_set(2));
    }
}
