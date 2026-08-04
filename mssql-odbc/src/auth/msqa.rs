// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bindings to `mssql-auth.dll` (the "MSQA" C ABI), the Windows authentication
//! library that fronts OneAuth.
//!
//! This is the same library, loaded the same way, that msodbcsql uses for
//! `Authentication=ActiveDirectoryInteractive`, so the sign-in experience and
//! the token cache behaviour match the C++ driver. Only the entry points the
//! interactive flow needs are bound; the password and integrated-auth entry
//! points are deliberately left out.
//!
//! # Why a dedicated thread
//!
//! OneAuth renders sign-in into a host window it creates via
//! `MSQAUICreateHostWindow`. That requires a single-threaded COM apartment and
//! a running message pump, neither of which a Tokio worker can provide. The
//! flow therefore runs on a thread of its own that initializes COM as an STA,
//! pumps messages until OneAuth signals completion, and then exits.
//!
//! # Reference
//!
//! msodbcsql `Sql/Common/DK/sni/src/SNI_FedAuth.cpp`: `SNISecMSQAInitialize`
//! (:231) loads the library, `MSQAAuthContextCache::getOrCreate` (:113) caches
//! contexts, `MSQAThread` (:417) pumps the UI, and `SNISecMSQAGetAccessToken`
//! (:553) sequences the whole acquisition.

use std::collections::HashMap;
use std::ffi::{CString, c_void};
use std::sync::{Arc, Mutex, OnceLock};

use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;
use mssql_tds::security::SecurityError;
use tracing::{debug, error, warn};
use windows::Win32::Foundation::{GetLastError, HMODULE, LPARAM, RECT, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExA,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetDesktopWindow, GetMessageW, MSG, PostThreadMessageW, TranslateMessage,
    WM_QUIT,
};
use windows::core::{GUID, PCSTR};

/// The library msodbcsql loads from System32 for Entra authentication.
const MSQA_LIBRARY: &str = "mssql-auth.dll";

/// `MSQAOPTION::UseWAM` — routes sign-in through the Web Account Manager broker
/// instead of OneAuth's own host window.
const MSQA_OPTION_USE_WAM: i32 = 1;

/// `MSQAOPTION::ForcePrompt` — forces an interactive prompt even when OneAuth
/// holds a usable cached account.
const MSQA_OPTION_FORCE_PROMPT: i32 = 2;

/// msodbcsql reads WAM from the `ADALuseWAM` DSN setting and defaults it off
/// (`sqlcconn.cpp:3417`), so the default experience is OneAuth's host window.
/// There is no ODBC connection-string keyword for it, so the default is all a
/// connection can currently select.
const USE_WAM: i32 = 0;

/// `MSQAAcquireToken` succeeded outright — OneAuth had a usable cached token
/// and no user interaction is needed.
const MSQA_SUCCESS: i32 = 0;

/// `MSQAAcquireToken` needs the caller to drive interaction before a token can
/// be produced (`S_FALSE`).
const MSQA_INTERACTION_REQUIRED: i32 = 1;

/// OneAuth packs its `Status` into the top 8 bits of the 64-bit error code
/// returned by `MSQAGetErrorDescription`; the rest is the underlying error.
const MSQA_STATUS_SHIFT: u32 = 56;

/// `Status::NetworkTemporarilyUnavailable`.
const STATUS_NETWORK_TEMPORARILY_UNAVAILABLE: u8 = 4;
/// `Status::ServerTemporarilyUnavailable`.
const STATUS_SERVER_TEMPORARILY_UNAVAILABLE: u8 = 5;
/// `Status::TransientError`.
const STATUS_TRANSIENT_ERROR: u8 = 15;

type HMsqaContext = *mut c_void;
type HMsqaRequest = *mut c_void;

type CompletionRoutine = unsafe extern "system" fn(request: HMsqaRequest, data: *mut c_void);

type PfnCreateAuthenticationContext = unsafe extern "system" fn(
    sts: PCSTR,
    client_id: PCSTR,
    redirect_uri: PCSTR,
    username: PCSTR,
) -> HMsqaContext;
type PfnSetOption = unsafe extern "system" fn(ctx: HMsqaContext, option: i32, value: i32);
type PfnAcquireToken = unsafe extern "system" fn(
    ctx: HMsqaContext,
    resource: PCSTR,
    correlation_id: *const GUID,
) -> HMsqaRequest;
type PfnGetRequestStatus = unsafe extern "system" fn(request: HMsqaRequest) -> i32;
type PfnGetAccessToken =
    unsafe extern "system" fn(request: HMsqaRequest, token: *mut u16, len: *mut u32) -> i32;
type PfnGetErrorDescription = unsafe extern "system" fn(
    request: HMsqaRequest,
    error: *mut u16,
    len: *mut u32,
    code: *mut i64,
) -> i32;
#[allow(clippy::too_many_arguments)]
type PfnUiCreateHostWindow = unsafe extern "system" fn(
    request: HMsqaRequest,
    callback: CompletionRoutine,
    callback_data: *mut c_void,
    parent: windows::Win32::Foundation::HWND,
    rect: *const RECT,
    window_name: *const u16,
    style: u32,
    ex_style: u32,
    menu_or_id: *mut c_void,
) -> i32;
type PfnDeleteRequest = unsafe extern "system" fn(request: HMsqaRequest);

/// The untyped code pointer `GetProcAddress` returns.
type Farproc = unsafe extern "system" fn() -> isize;

/// The resolved `mssql-auth.dll` entry points.
///
/// `MSQAReleaseAuthenticationContext` is intentionally absent: interactive
/// contexts are cached for the life of the process so OneAuth can reuse its
/// token cache, exactly as msodbcsql does (`SNI_FedAuth.cpp:777-782`), so there
/// is never a caller for it.
struct MsqaApi {
    create_context: PfnCreateAuthenticationContext,
    set_option: PfnSetOption,
    acquire_token: PfnAcquireToken,
    get_request_status: PfnGetRequestStatus,
    get_access_token: PfnGetAccessToken,
    get_error_description: PfnGetErrorDescription,
    ui_create_host_window: PfnUiCreateHostWindow,
    delete_request: PfnDeleteRequest,
}

// The entry points are plain code pointers into a module that is never
// unloaded, and OneAuth serializes its own state internally.
unsafe impl Send for MsqaApi {}
unsafe impl Sync for MsqaApi {}

/// A `HMSQACONTEXT` parked in the process-wide cache.
///
/// The handle is an opaque OneAuth pointer. Concurrent use is serialized by
/// [`CachedContext::acquire_lock`], and the handle outlives every borrow
/// because cached contexts are never released.
struct ContextHandle(HMsqaContext);

unsafe impl Send for ContextHandle {}
unsafe impl Sync for ContextHandle {}

/// A cached authentication context plus the lock that keeps concurrent
/// connections from opening two sign-in windows against it at once.
struct CachedContext {
    handle: ContextHandle,
    acquire_lock: Mutex<()>,
}

/// A `HMSQAREQUEST` being moved onto the UI thread.
///
/// Requests are used by one thread at a time: the caller hands ownership to the
/// UI thread and blocks on its join handle until it is done.
struct RequestHandle(HMsqaRequest);

unsafe impl Send for RequestHandle {}

/// Deletes the request handle on every exit path, mirroring the `Exit:` block
/// at `SNI_FedAuth.cpp:767-774`.
struct RequestGuard {
    api: &'static MsqaApi,
    request: HMsqaRequest,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        unsafe { (self.api.delete_request)(self.request) };
    }
}

/// Contexts cached by `(login hint, STS URL)`, matching msodbcsql's cache key.
type ContextCache = Mutex<HashMap<(String, String), Arc<CachedContext>>>;

static MSQA_API: OnceLock<Result<MsqaApi, String>> = OnceLock::new();
static CONTEXT_CACHE: OnceLock<ContextCache> = OnceLock::new();

/// Loads `mssql-auth.dll` and resolves the entry points, once per process.
///
/// The search is restricted to System32 (as msodbcsql does at
/// `SNI_FedAuth.cpp:249`) so a DLL dropped next to the application cannot
/// hijack authentication. The module is never freed: OneAuth spins up
/// background state that must outlive individual connections.
fn api() -> TdsResult<&'static MsqaApi> {
    let loaded = MSQA_API.get_or_init(|| {
        let name = CString::new(MSQA_LIBRARY).expect("library name has no interior NUL");
        let module = unsafe {
            LoadLibraryExA(
                PCSTR(name.as_ptr().cast()),
                None,
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        let module = match module {
            Ok(m) => m,
            Err(e) => {
                return Err(format!(
                    "{MSQA_LIBRARY} could not be loaded from the system directory ({e}). \
                     Entra interactive authentication requires the Microsoft SQL Server \
                     authentication library to be installed."
                ));
            }
        };
        resolve(module)
    });

    match loaded {
        Ok(api) => Ok(api),
        Err(message) => Err(Error::Security(SecurityError::LoadLibraryFailed(
            message.clone(),
        ))),
    }
}

/// Resolves every entry point the interactive flow needs, failing if any is
/// missing rather than discovering the gap mid-sign-in.
fn resolve(module: HMODULE) -> Result<MsqaApi, String> {
    /// Looks up one export and transmutes it to its typed signature.
    ///
    /// `GetProcAddress` hands back an untyped code pointer; the transmute is
    /// sound because the signatures are transcribed from `msqa_api.h` and the
    /// module stays loaded for the life of the process.
    macro_rules! entry {
        ($name:literal, $ty:ty) => {{
            let symbol = concat!($name, "\0");
            let address = unsafe { GetProcAddress(module, PCSTR(symbol.as_ptr())) }
                .ok_or_else(|| format!("{MSQA_LIBRARY} does not export {}", $name))?;
            unsafe { std::mem::transmute::<Farproc, $ty>(address) }
        }};
    }

    Ok(MsqaApi {
        create_context: entry!(
            "MSQACreateAuthenticationContext",
            PfnCreateAuthenticationContext
        ),
        set_option: entry!("MSQASetOption", PfnSetOption),
        acquire_token: entry!("MSQAAcquireToken", PfnAcquireToken),
        get_request_status: entry!("MSQAGetRequestStatus", PfnGetRequestStatus),
        get_access_token: entry!("MSQAGetAccessToken", PfnGetAccessToken),
        get_error_description: entry!("MSQAGetErrorDescription", PfnGetErrorDescription),
        ui_create_host_window: entry!("MSQAUICreateHostWindow", PfnUiCreateHostWindow),
        delete_request: entry!("MSQADeleteRequest", PfnDeleteRequest),
    })
}

/// Returns the cached context for `(login_hint, sts_url)`, creating it on first
/// use.
///
/// Keying on the account and authority — not the resource — is what lets a
/// second connection for the same user reuse OneAuth's token cache instead of
/// prompting again. `ForcePrompt` starts enabled so the first sign-in of a
/// process is always explicit; [`acquire_token`] clears it once a token has
/// been obtained. Mirrors `MSQAAuthContextCache::getOrCreate`.
fn get_or_create_context(
    api: &'static MsqaApi,
    sts_url: &str,
    login_hint: &str,
    client_id: &str,
    redirect_uri: &str,
) -> TdsResult<Arc<CachedContext>> {
    let key = (login_hint.to_string(), sts_url.to_string());
    let cache = CONTEXT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(existing) = cache.get(&key) {
        debug!("interactive: reusing cached OneAuth authentication context");
        return Ok(Arc::clone(existing));
    }

    let sts = to_c_string(sts_url, "STS URL")?;
    let client = to_c_string(client_id, "client id")?;
    let redirect = to_c_string(redirect_uri, "redirect URI")?;
    let user = to_c_string(login_hint, "user name")?;

    let handle = unsafe {
        (api.create_context)(
            PCSTR(sts.as_ptr().cast()),
            PCSTR(client.as_ptr().cast()),
            PCSTR(redirect.as_ptr().cast()),
            PCSTR(user.as_ptr().cast()),
        )
    };
    if handle.is_null() {
        let code = unsafe { GetLastError() }.0;
        return Err(Error::Security(SecurityError::InternalError(format!(
            "MSQACreateAuthenticationContext failed (Windows error {code})"
        ))));
    }

    unsafe {
        (api.set_option)(handle, MSQA_OPTION_FORCE_PROMPT, 1);
        (api.set_option)(handle, MSQA_OPTION_USE_WAM, USE_WAM);
    }

    let context = Arc::new(CachedContext {
        handle: ContextHandle(handle),
        acquire_lock: Mutex::new(()),
    });
    cache.insert(key, Arc::clone(&context));
    Ok(context)
}

/// Acquires an access token interactively. Blocking: run it on a thread that
/// may block.
///
/// `ui_thread_id` receives the id of the message-pump thread as soon as it
/// starts, so a caller whose login deadline expires can tear the sign-in window
/// down via [`cancel_ui`] instead of leaving it orphaned on screen.
pub(super) fn acquire_token(
    sts_url: &str,
    resource: &str,
    login_hint: &str,
    client_id: &str,
    redirect_uri: &str,
    window_title: &str,
    ui_thread_id: &Arc<UiThreadId>,
) -> TdsResult<String> {
    let api = api()?;
    let context = get_or_create_context(api, sts_url, login_hint, client_id, redirect_uri)?;
    let resource_c = to_c_string(resource, "resource")?;

    // A fresh correlation id per acquisition, so a failed sign-in can be
    // located in the tenant's Entra sign-in logs.
    let correlation_id = GUID::new().unwrap_or(GUID::zeroed());

    // Only one sign-in window per context at a time: without this, two
    // connections opening concurrently would each raise their own prompt.
    let _serialized = context
        .acquire_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let request = unsafe {
        (api.acquire_token)(
            context.handle.0,
            PCSTR(resource_c.as_ptr().cast()),
            &correlation_id,
        )
    };
    if request.is_null() {
        let code = unsafe { GetLastError() }.0;
        return Err(Error::Security(SecurityError::InternalError(format!(
            "MSQAAcquireToken failed (Windows error {code})"
        ))));
    }
    // Deletes the request when this function returns, on every path.
    let _request = RequestGuard { api, request };

    let status = unsafe { (api.get_request_status)(request) };
    let status = if status == MSQA_INTERACTION_REQUIRED {
        debug!("interactive: OneAuth requires sign-in, opening the host window");
        run_interactive_ui(api, request, window_title, ui_thread_id)
    } else {
        debug!(status, "interactive: OneAuth answered without a prompt");
        status
    };

    if status != MSQA_SUCCESS {
        return Err(describe_failure(api, request, status));
    }

    // The account is now signed in; let later connections reuse the cached
    // account instead of prompting again.
    unsafe { (api.set_option)(context.handle.0, MSQA_OPTION_FORCE_PROMPT, 0) };
    read_access_token(api, request)
}

/// Runs OneAuth's sign-in window to completion on a dedicated STA thread and
/// returns the resulting request status.
///
/// The window is created and pumped on the same thread because OneAuth posts
/// its completion callback to that thread's message queue; the callback turns
/// it into `WM_QUIT`, which ends the pump. Mirrors `MSQAThread`
/// (`SNI_FedAuth.cpp:417-511`).
fn run_interactive_ui(
    api: &'static MsqaApi,
    request: HMsqaRequest,
    window_title: &str,
    ui_thread_id: &Arc<UiThreadId>,
) -> i32 {
    let moved = RequestHandle(request);
    let title: Vec<u16> = window_title
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let ui_thread_id = Arc::clone(ui_thread_id);

    let worker = std::thread::Builder::new()
        .name("mssql-odbc-interactive-auth".to_string())
        .spawn(move || {
            let moved = moved;
            unsafe { pump_sign_in_window(api, moved.0, &title, &ui_thread_id) }
        });

    // The caller holds the request alive across this whole function, and it
    // owns the acquire lock, so reading the status back here is safe even when
    // the UI thread never started or died.
    let worker = match worker {
        Ok(worker) => worker,
        Err(e) => {
            error!(error = %e, "interactive: could not start the sign-in UI thread");
            return unsafe { (api.get_request_status)(request) };
        }
    };

    match worker.join() {
        Ok(status) => status,
        Err(_) => {
            error!("interactive: the sign-in UI thread panicked");
            unsafe { (api.get_request_status)(request) }
        }
    }
}

/// Initializes an STA, hands the request to OneAuth's window, and pumps
/// messages until the completion callback posts `WM_QUIT`.
///
/// # Safety
///
/// `request` must be a live `HMSQAREQUEST` that no other thread is using for
/// the duration of this call.
unsafe fn pump_sign_in_window(
    api: &'static MsqaApi,
    request: HMsqaRequest,
    title: &[u16],
    ui_thread_id: &UiThreadId,
) -> i32 {
    // OneAuth's window is a COM single-threaded apartment object.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if com.is_err() {
        error!(hresult = com.0, "interactive: CoInitializeEx failed");
        return com.0;
    }

    let thread_id = unsafe { GetCurrentThreadId() };
    ui_thread_id.set(thread_id);

    let created = unsafe {
        (api.ui_create_host_window)(
            request,
            ui_complete,
            thread_id as usize as *mut c_void,
            GetDesktopWindow(),
            std::ptr::null(),
            title.as_ptr(),
            0,
            0,
            std::ptr::null_mut(),
        )
    };

    if created == MSQA_SUCCESS {
        // The DSN test dialog can still hold the mouse; releasing it lets the
        // user interact with the sign-in window (`SNI_FedAuth.cpp:470`).
        let _ = unsafe { ReleaseCapture() };

        let mut message = MSG::default();
        // GetMessageW returns >0 for a message, 0 for WM_QUIT, and -1 on error.
        // `BOOL::as_bool()` is `!= 0`, so it would take the error for a message
        // and dispatch an uninitialized MSG, spinning here forever while the
        // caller blocks on this thread's join. msodbcsql tests `> 0`
        // (`SNI_FedAuth.cpp:474`).
        loop {
            let pumped = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
            if pumped < 0 {
                let code = unsafe { GetLastError() }.0;
                error!(
                    windows_error = code,
                    "interactive: GetMessageW failed, ending the message pump"
                );
                break;
            }
            if pumped == 0 {
                break;
            }
            let _ = unsafe { TranslateMessage(&message) };
            unsafe { DispatchMessageW(&message) };
        }
    } else {
        let code = unsafe { GetLastError() }.0;
        error!(
            hresult = created,
            windows_error = code,
            "interactive: MSQAUICreateHostWindow failed"
        );
    }

    ui_thread_id.clear();
    let status = unsafe { (api.get_request_status)(request) };
    unsafe { CoUninitialize() };
    status
}

/// OneAuth's completion callback. Ends the message pump on the thread whose id
/// was passed as `data` when the window was created.
///
/// # Safety
///
/// Called by OneAuth with the `callback_data` supplied to
/// `MSQAUICreateHostWindow`, which is always a thread id cast to a pointer.
unsafe extern "system" fn ui_complete(_request: HMsqaRequest, data: *mut c_void) {
    let thread_id = data as usize as u32;
    let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
}

/// Reads the acquired token with the two-pass length-then-buffer protocol
/// `MSQAGetAccessToken` uses.
fn read_access_token(api: &'static MsqaApi, request: HMsqaRequest) -> TdsResult<String> {
    let mut length: u32 = 0;
    unsafe { (api.get_access_token)(request, std::ptr::null_mut(), &mut length) };
    if length == 0 {
        return Err(Error::ProtocolError(
            "mssql-auth reported a successful sign-in but returned no access token".to_string(),
        ));
    }

    let mut buffer = vec![0u16; length as usize + 1];
    let status = unsafe { (api.get_access_token)(request, buffer.as_mut_ptr(), &mut length) };
    if status != MSQA_SUCCESS {
        return Err(Error::Security(SecurityError::InternalError(format!(
            "MSQAGetAccessToken failed (status {status})"
        ))));
    }

    buffer.truncate(length as usize);
    String::from_utf16(&buffer).map_err(|_| {
        Error::ProtocolError("mssql-auth returned a malformed access token".to_string())
    })
}

/// Turns a failed request into an error, preserving OneAuth's own description
/// and preserving the transient/permanent distinction the connection retry
/// logic depends on.
fn describe_failure(api: &'static MsqaApi, request: HMsqaRequest, status: i32) -> Error {
    let mut length: u32 = 0;
    let mut packed: i64 = 0;
    unsafe { (api.get_error_description)(request, std::ptr::null_mut(), &mut length, &mut packed) };

    let mut description = String::new();
    if length > 0 {
        let mut buffer = vec![0u16; length as usize + 1];
        let read = unsafe {
            (api.get_error_description)(request, buffer.as_mut_ptr(), &mut length, &mut packed)
        };
        if read == MSQA_SUCCESS {
            buffer.truncate(length as usize);
            description = String::from_utf16_lossy(&buffer);
        }
    }
    if description.is_empty() {
        description = format!("sign-in failed with status {status}");
    }

    // OneAuth packs its `Status` into the high byte and the underlying error
    // into the remainder (`SNI_FedAuth.cpp:726-759`).
    let one_auth_status = ((packed >> MSQA_STATUS_SHIFT) & 0xFF) as u8;

    if is_transient(one_auth_status) {
        warn!(
            status = one_auth_status,
            "interactive: transient failure acquiring a token"
        );
        // Transient faults stay `ConnectionError` so the provider may retry.
        return Error::ConnectionError(format!("Entra interactive sign-in failed: {description}"));
    }

    Error::Security(SecurityError::AuthenticationDenied(format!(
        "Entra interactive sign-in failed: {description}"
    )))
}

/// Mirrors `IsTransientError` (`SNI_FedAuth.cpp:301-306`).
fn is_transient(status: u8) -> bool {
    matches!(
        status,
        STATUS_NETWORK_TEMPORARILY_UNAVAILABLE
            | STATUS_SERVER_TEMPORARILY_UNAVAILABLE
            | STATUS_TRANSIENT_ERROR
    )
}

/// The id of the running sign-in message pump, or zero when none is running.
///
/// Zero is never a valid thread id, so it doubles as "nothing to cancel".
#[derive(Default)]
pub(super) struct UiThreadId(std::sync::atomic::AtomicU32);

impl UiThreadId {
    fn set(&self, id: u32) {
        self.0.store(id, std::sync::atomic::Ordering::SeqCst);
    }

    fn clear(&self) {
        self.0.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    fn get(&self) -> u32 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Closes a sign-in window that is still open, used when the caller's login
/// deadline expires.
///
/// The pump clears its id before it returns, so this cannot post to a thread id
/// that has already been recycled by another thread.
pub(super) fn cancel_ui(ui_thread_id: &UiThreadId) {
    let thread_id = ui_thread_id.get();
    if thread_id == 0 {
        return;
    }
    debug!("interactive: login deadline expired, closing the sign-in window");
    let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
}

/// Converts a connection-derived string into the narrow C string the MSQA ABI
/// takes, rejecting embedded NULs rather than silently truncating.
fn to_c_string(value: &str, what: &str) -> TdsResult<CString> {
    CString::new(value)
        .map_err(|_| Error::ConnectionError(format!("{what} contains an embedded NUL character")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_statuses_match_msodbcsql() {
        // SNI_FedAuth.cpp:301-306 treats exactly these three as retriable.
        assert!(is_transient(STATUS_NETWORK_TEMPORARILY_UNAVAILABLE));
        assert!(is_transient(STATUS_SERVER_TEMPORARILY_UNAVAILABLE));
        assert!(is_transient(STATUS_TRANSIENT_ERROR));
    }

    #[test]
    fn user_cancellation_is_not_transient() {
        // `Status::UserCanceled` (7) must not trigger a retry, or declining the
        // prompt would immediately raise a second sign-in window.
        assert!(!is_transient(7));
        // `Status::IncorrectConfiguration` (9) and `AuthorityUntrusted` (11)
        // recur identically on retry.
        assert!(!is_transient(9));
        assert!(!is_transient(11));
    }

    #[test]
    fn ui_thread_id_round_trips_and_clears() {
        let id = UiThreadId::default();
        assert_eq!(id.get(), 0, "no pump running yet");
        id.set(4242);
        assert_eq!(id.get(), 4242);
        id.clear();
        assert_eq!(id.get(), 0, "a cleared id must not be posted to");
    }

    #[test]
    fn cancel_ui_is_a_no_op_when_no_window_is_open() {
        // Guards against posting WM_QUIT to a recycled thread id.
        cancel_ui(&UiThreadId::default());
    }

    #[test]
    fn embedded_nul_is_rejected() {
        let err = to_c_string("contoso\0evil", "STS URL").unwrap_err();
        assert!(err.to_string().contains("STS URL"), "got: {err}");
    }

    #[test]
    fn packed_status_extraction_matches_one_auth_layout() {
        // OneAuth returns status in the top 8 bits, error in the rest.
        let packed: i64 = ((STATUS_TRANSIENT_ERROR as i64) << MSQA_STATUS_SHIFT) | 0x1234_5678;
        assert_eq!(((packed >> MSQA_STATUS_SHIFT) & 0xFF) as u8, 15);
    }
}
