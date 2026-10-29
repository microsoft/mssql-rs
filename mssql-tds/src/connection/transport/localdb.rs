// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows LocalDB API bindings and connection logic.
//!
//! This module provides FFI bindings to the Windows LocalDB API (sqluserinstance.dll)
//! and implements the logic to resolve LocalDB instance names to named pipe paths.
//!
//! LocalDB API Reference:
//! https://learn.microsoft.com/en-us/sql/relational-databases/express-localdb-instance-apis/sql-server-express-localdb-reference-instance-apis

use crate::connection::blocking::off_runtime_thread;
use crate::core::TdsResult;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use tracing::{debug, info};
use windows::Win32::Foundation::{ERROR_SUCCESS, FILETIME, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{PCWSTR, PWSTR};

/// Maximum size for the LocalDB instance pipe name buffer (in characters).
/// Based on SQL Server LocalDB API documentation.
const LOCALDB_MAX_SQLCONNECTION_BUFFER_SIZE: usize = 260;

/// LocalDB API version 11.0 (SQL Server 2012)
#[allow(dead_code)] // Defined for completeness alongside the LocalDB API bindings
const LOCALDB_VERSION_11_0: u32 = 0x0B000000;

/// Maximum length of a LocalDB instance name (in characters, without the terminator).
const MAX_LOCALDB_INSTANCE_NAME_LENGTH: usize = 128;

/// Maximum length of a string SID (in characters, without the terminator).
const MAX_STRING_SID_LENGTH: usize = 186;

/// LocalDB instance information, as `LocalDBInstanceInfo` in `msodbcsql.h`.
/// `LocalDBGetInstanceInfo` accepts only the size of a version of this struct
/// it knows, so the layout must match the header field for field.
#[repr(C)]
#[derive(Debug)]
#[allow(dead_code)] // Fields mirror the C layout; only some are read.
pub(crate) struct LocalDBInstanceInfo {
    /// cbLocalDBInstanceInfoSize: size of this struct
    cb_size: u32,
    /// wszInstanceName
    instance_name: [u16; MAX_LOCALDB_INSTANCE_NAME_LENGTH + 1],
    /// bExists: the instance files exist on disk
    exists: i32,
    /// bConfigurationCorrupted
    configuration_corrupted: i32,
    /// bIsRunning: the instance is running at the moment
    is_running: i32,
    /// dwMajor, dwMinor, dwBuild, dwRevision: the LocalDB version of the instance
    major: u32,
    minor: u32,
    build: u32,
    revision: u32,
    /// ftLastStartDateUTC
    last_start_utc: FILETIME,
    /// wszConnection: the TDS named pipe of the instance, when it is running
    connection: [u16; LOCALDB_MAX_SQLCONNECTION_BUFFER_SIZE],
    /// bIsShared
    is_shared: i32,
    /// wszSharedInstanceName
    shared_instance_name: [u16; MAX_LOCALDB_INSTANCE_NAME_LENGTH + 1],
    /// wszOwnerSID
    owner_sid: [u16; MAX_STRING_SID_LENGTH + 1],
    /// bIsAutomatic
    is_automatic: i32,
}

// sizeof(LocalDBInstanceInfo) in msodbcsql.h (pack 8, no member wider than 4 bytes).
const _: () = assert!(std::mem::size_of::<LocalDBInstanceInfo>() == 1460);

impl Default for LocalDBInstanceInfo {
    fn default() -> Self {
        Self {
            cb_size: std::mem::size_of::<LocalDBInstanceInfo>() as u32,
            instance_name: [0; MAX_LOCALDB_INSTANCE_NAME_LENGTH + 1],
            exists: 0,
            configuration_corrupted: 0,
            is_running: 0,
            major: 0,
            minor: 0,
            build: 0,
            revision: 0,
            last_start_utc: FILETIME::default(),
            connection: [0; LOCALDB_MAX_SQLCONNECTION_BUFFER_SIZE],
            is_shared: 0,
            shared_instance_name: [0; MAX_LOCALDB_INSTANCE_NAME_LENGTH + 1],
            owner_sid: [0; MAX_STRING_SID_LENGTH + 1],
            is_automatic: 0,
        }
    }
}

// The LocalDB API functions are __cdecl in msodbcsql.h; "C" differs from
// "system" (stdcall) on 32-bit x86 only.

/// Type alias for LocalDBStartInstance function pointer
#[allow(non_snake_case)]
type LocalDBStartInstanceFn = unsafe extern "C" fn(
    pwsz_instance: PCWSTR,
    dw_flags: u32,
    wszSqlConnection: PWSTR,
    lpcchSqlConnection: *mut u32,
) -> i32;

/// Type alias for LocalDBGetInstanceInfo function pointer
#[allow(non_snake_case)]
type LocalDBGetInstanceInfoFn = unsafe extern "C" fn(
    pwszInstanceName: PCWSTR,
    pInstanceInfo: *mut LocalDBInstanceInfo,
    dwInstanceInfoSize: u32,
) -> i32;

/// LocalDB API wrapper that manages the DLL handle and function pointers
pub struct LocalDBApi {
    #[allow(dead_code)]
    // Retained to prevent the DLL from being unloaded while function pointers are live
    dll_handle: HMODULE,
    start_instance_fn: LocalDBStartInstanceFn,
    get_instance_info_fn: LocalDBGetInstanceInfoFn,
}

impl LocalDBApi {
    /// Query Windows Registry to find the path to sqluserinstance.dll
    ///
    /// This replicates the ODBC approach: enumerate all installed LocalDB versions
    /// under HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Microsoft SQL Server Local DB\Installed Versions,
    /// find the latest version, and read its InstanceAPIPath value.
    ///
    /// Returns the full path to SqlUserInstance.dll for the latest installed version.
    fn get_dll_path_from_registry() -> TdsResult<String> {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        use windows::Win32::System::Registry::*;

        unsafe {
            const REG_PATH: &str =
                r"SOFTWARE\Microsoft\Microsoft SQL Server Local DB\Installed Versions";

            // Open the registry key
            let key_name = to_wide_string(REG_PATH);
            let mut hkey = HKEY::default();

            let result = RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(key_name.as_ptr()),
                0,
                KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE,
                &mut hkey,
            );

            if result.is_err() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "LocalDB registry key not found. LocalDB may not be installed.",
                )
                .into());
            }

            // Enumerate subkeys to find all versions
            let mut latest_version = None;
            let mut latest_major: u16 = 0;
            let mut latest_minor: u16 = 0;
            let mut index = 0;

            loop {
                let mut name_buffer = vec![0u16; 256];
                let mut name_len = name_buffer.len() as u32;

                let result = RegEnumKeyExW(
                    hkey,
                    index,
                    windows::core::PWSTR(name_buffer.as_mut_ptr()),
                    &mut name_len,
                    None,
                    windows::core::PWSTR::null(),
                    None,
                    None,
                );

                if result.is_err() {
                    break; // No more subkeys
                }

                // Convert to String
                let version_str = OsString::from_wide(&name_buffer[..name_len as usize])
                    .to_string_lossy()
                    .into_owned();

                // Parse version (format: "15.0" or "16.0")
                if let Some((major_str, minor_str)) = version_str.split_once('.')
                    && let (Ok(major), Ok(minor)) =
                        (major_str.parse::<u16>(), minor_str.parse::<u16>())
                    && (major > latest_major || (major == latest_major && minor > latest_minor))
                {
                    latest_major = major;
                    latest_minor = minor;
                    latest_version = Some(version_str);
                }

                index += 1;
            }

            let version = latest_version.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "No valid LocalDB versions found in registry",
                )
            })?;

            // Open the version-specific key
            let version_key_name = to_wide_string(&version);
            let mut version_hkey = HKEY::default();

            let result = RegOpenKeyExW(
                hkey,
                PCWSTR(version_key_name.as_ptr()),
                0,
                KEY_QUERY_VALUE,
                &mut version_hkey,
            );

            let _ = RegCloseKey(hkey);

            if result.is_err() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("Failed to open LocalDB version key: {version}"),
                )
                .into());
            }

            // Read InstanceAPIPath value
            let value_name = to_wide_string("InstanceAPIPath");
            let mut buffer = vec![0u16; 512];
            let mut buffer_size = (buffer.len() * 2) as u32;

            let result = RegQueryValueExW(
                version_hkey,
                PCWSTR(value_name.as_ptr()),
                None,
                None, // Don't need the type
                Some(buffer.as_mut_ptr() as *mut u8),
                Some(&mut buffer_size),
            );

            let _ = RegCloseKey(version_hkey);

            if result.is_err() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "InstanceAPIPath registry value not found",
                )
                .into());
            }

            // Convert to String and trim null terminators
            let path = OsString::from_wide(&buffer[..(buffer_size as usize / 2)])
                .to_string_lossy()
                .trim_end_matches('\0')
                .trim()
                .to_string();

            if path.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "InstanceAPIPath registry value is empty",
                )
                .into());
            }

            Ok(path)
        }
    }

    /// Load the LocalDB API DLL and function pointers
    ///
    /// Attempts to load sqluserinstance.dll and resolve the required function pointers.
    /// This follows the same approach as ODBC: query the Windows Registry to find the
    /// path to the latest installed LocalDB version's DLL.
    ///
    /// Registry location: HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Microsoft SQL Server Local DB\Installed Versions
    ///
    /// Returns an error if the DLL cannot be loaded or if required functions are not found.
    pub fn load() -> TdsResult<Self> {
        unsafe {
            // Get the DLL path from the registry (same as ODBC does - no fallback)
            let dll_path = Self::get_dll_path_from_registry()?;

            // Load the DLL from the registry path
            let dll_name = to_wide_string(&dll_path);
            let dll_handle = LoadLibraryW(PCWSTR(dll_name.as_ptr())).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("Failed to load sqluserinstance.dll from '{dll_path}'. Error: {e}"),
                )
            })?;

            debug!("Loaded sqluserinstance.dll successfully");

            // Get function pointers
            let start_fn_name = b"LocalDBStartInstance\0";
            let start_fn_ptr =
                GetProcAddress(dll_handle, windows::core::PCSTR(start_fn_name.as_ptr()))
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "LocalDBStartInstance function not found in sqluserinstance.dll",
                        )
                    })?;

            let info_fn_name = b"LocalDBGetInstanceInfo\0";
            let info_fn_ptr =
                GetProcAddress(dll_handle, windows::core::PCSTR(info_fn_name.as_ptr()))
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "LocalDBGetInstanceInfo function not found in sqluserinstance.dll",
                        )
                    })?;

            #[allow(clippy::missing_transmute_annotations)]
            Ok(Self {
                dll_handle,
                start_instance_fn: std::mem::transmute(start_fn_ptr),
                get_instance_info_fn: std::mem::transmute(info_fn_ptr),
            })
        }
    }

    /// Start a LocalDB instance and return the named pipe path
    ///
    /// This function will start the instance if it's not already running and return
    /// the named pipe path that can be used to connect to it.
    ///
    /// # Arguments
    /// * `instance_name` - The name of the LocalDB instance (e.g., "MSSQLLocalDB", "v11.0")
    ///
    /// # Returns
    /// The named pipe path to connect to the instance (e.g., "np:\\.\pipe\LOCALDB#<hash>\tsql\query")
    pub fn start_instance(&self, instance_name: &str) -> TdsResult<String> {
        info!("Starting LocalDB instance: {}", instance_name);

        let instance_name_wide = to_wide_string(instance_name);
        let mut connection_buffer = [0u16; LOCALDB_MAX_SQLCONNECTION_BUFFER_SIZE];
        let mut buffer_size = LOCALDB_MAX_SQLCONNECTION_BUFFER_SIZE as u32;

        unsafe {
            let hr = (self.start_instance_fn)(
                PCWSTR(instance_name_wide.as_ptr()),
                0, // flags
                PWSTR(connection_buffer.as_mut_ptr()),
                &mut buffer_size,
            );

            if hr != ERROR_SUCCESS.0 as i32 {
                return Err(std::io::Error::other(
                    format!(
                        "LocalDBStartInstance failed for instance '{instance_name}' with error code: 0x{:08X}",
                        hr as u32
                    ),
                )
                .into());
            }

            let pipe_name = from_wide_string(&connection_buffer);
            info!("LocalDB instance started successfully: {}", pipe_name);
            Ok(pipe_name)
        }
    }

    /// Get information about a LocalDB instance
    ///
    /// # Arguments
    /// * `instance_name` - The name of the LocalDB instance
    ///
    /// # Returns
    /// LocalDBInstanceInfo structure: whether the instance is running and, if
    /// so, its named pipe
    pub fn get_instance_info(&self, instance_name: &str) -> TdsResult<LocalDBInstanceInfo> {
        debug!("Getting LocalDB instance info: {}", instance_name);

        let instance_name_wide = to_wide_string(instance_name);
        let mut instance_info = LocalDBInstanceInfo::default();

        unsafe {
            let hr = (self.get_instance_info_fn)(
                PCWSTR(instance_name_wide.as_ptr()),
                &mut instance_info,
                std::mem::size_of::<LocalDBInstanceInfo>() as u32,
            );

            if hr != ERROR_SUCCESS.0 as i32 {
                return Err(std::io::Error::other(
                    format!(
                        "LocalDBGetInstanceInfo failed for instance '{instance_name}' with error code: 0x{:08X}",
                        hr as u32
                    ),
                )
                .into());
            }

            debug!(
                "LocalDB instance info retrieved: running={}, connection={}",
                instance_info.is_running != 0,
                from_wide_string(&instance_info.connection)
            );

            Ok(instance_info)
        }
    }

    /// Resolve a LocalDB instance name to a named pipe path
    ///
    /// This is the main entry point for LocalDB resolution. It will:
    /// 1. Start the instance if needed
    /// 2. Return the named pipe path
    ///
    /// # Arguments
    /// * `instance_name` - The LocalDB instance name
    ///
    /// # Returns
    /// The named pipe path to connect to
    pub fn resolve_instance(&self, instance_name: &str) -> TdsResult<String> {
        // Start the instance (will return existing pipe if already running)
        let pipe_name = strip_np_prefix(self.start_instance(instance_name)?);

        info!(
            "LocalDB instance '{}' resolved to pipe: {}",
            instance_name, pipe_name
        );

        Ok(pipe_name)
    }
}

impl Drop for LocalDBApi {
    fn drop(&mut self) {
        // Note: In modern Windows APIs, HMODULE is automatically managed
        // We don't need to explicitly call FreeLibrary as the handle will be
        // cleaned up when the process exits
        debug!("LocalDBApi dropped");
    }
}

/// Convert a Rust string to a null-terminated wide string (UTF-16)
fn to_wide_string(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Convert a null-terminated wide string buffer to a Rust String
fn from_wide_string(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// The LocalDB API gives the pipe as "np:\\.\pipe\LOCALDB#<hash>\tsql\query";
/// the transport wants the raw pipe path, without "np:".
fn strip_np_prefix(pipe_name: String) -> String {
    pipe_name
        .strip_prefix("np:")
        .map(|s| s.to_string())
        .unwrap_or(pipe_name)
}

/// The pipe of an instance `info` reports running, without its "np:" prefix,
/// or `None` if it is not running or reports no pipe, and so has to be started.
fn running_pipe(info: &LocalDBInstanceInfo) -> Option<String> {
    if info.is_running == 0 {
        return None;
    }
    let pipe_name = strip_np_prefix(from_wide_string(&info.connection));
    (!pipe_name.is_empty()).then_some(pipe_name)
}

/// The pipe of `instance_name` if it is already running, asked of
/// LocalDBGetInstanceInfo, which only reads the instance's state. `None` if
/// the instance is not running, or its state could not be read.
fn running_instance_pipe(instance_name: &str) -> TdsResult<Option<String>> {
    let api = LocalDBApi::load()?;
    match api.get_instance_info(instance_name) {
        Ok(info) => Ok(running_pipe(&info)),
        Err(error) => {
            debug!("LocalDB instance '{instance_name}' state unavailable: {error}");
            Ok(None)
        }
    }
}

/// Resolve a LocalDB instance name to a named pipe path
///
/// This is the public API function that can be called from the transport layer.
///
/// An instance that is already running resolves on the current thread:
/// LocalDBGetInstanceInfo only reads its state and pipe, so a connection to a
/// running instance takes no blocking-pool thread. Otherwise the instance has
/// to be started, and LocalDBStartInstance, which can take a while or stall,
/// runs on the blocking pool, so the login timeout and cancellation still
/// apply. It also reports the error for an instance that does not exist.
///
/// # Arguments
/// * `instance_name` - The LocalDB instance name (e.g., "MSSQLLocalDB", "v11.0")
///
/// # Returns
/// The named pipe path to connect to the instance
pub async fn resolve_localdb_instance(instance_name: &str) -> TdsResult<String> {
    info!("Resolving LocalDB instance: {}", instance_name);

    if let Some(pipe_name) = running_instance_pipe(instance_name)? {
        info!(
            "LocalDB instance '{}' is running; resolved to pipe: {}",
            instance_name, pipe_name
        );
        return Ok(pipe_name);
    }

    info!(
        "LocalDB instance '{}' is not running; starting it on a blocking thread",
        instance_name
    );
    // LocalDBApi holds the module handle, which is not Send, so the blocking
    // thread loads it again; the DLL is already loaded, and this is small next
    // to starting an instance.
    let instance_name = instance_name.to_string();
    off_runtime_thread(move || {
        let api = LocalDBApi::load()?;
        api.resolve_instance(&instance_name)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_load_localdb_api() {
        let result = LocalDBApi::load();
        // This will fail if LocalDB is not installed, which is expected in CI
        if result.is_ok() {
            println!("LocalDB API loaded successfully");
        } else {
            println!("LocalDB API not available (expected in environments without LocalDB)");
        }
    }

    #[test]
    fn test_to_wide_string() {
        let s = "Hello";
        let wide = to_wide_string(s);
        assert_eq!(wide[0], b'H' as u16);
        assert_eq!(wide[1], b'e' as u16);
        assert_eq!(wide[2], b'l' as u16);
        assert_eq!(wide[3], b'l' as u16);
        assert_eq!(wide[4], b'o' as u16);
        assert_eq!(wide[5], 0); // null terminator
    }

    #[test]
    fn test_from_wide_string() {
        let buffer: Vec<u16> = vec![b'T' as u16, b'e' as u16, b's' as u16, b't' as u16, 0];
        let s = from_wide_string(&buffer);
        assert_eq!(s, "Test");
    }

    #[test]
    fn test_from_wide_string_with_extra_nulls() {
        let buffer: Vec<u16> = vec![b'H' as u16, b'i' as u16, 0, 0, 0];
        let s = from_wide_string(&buffer);
        assert_eq!(s, "Hi");
    }

    #[test]
    fn test_resolve_mssqllocaldb() {
        // This test will only work on machines with LocalDB installed
        if let Ok(api) = LocalDBApi::load() {
            match api.resolve_instance("MSSQLLocalDB") {
                Ok(pipe_name) => {
                    println!("Resolved pipe: {pipe_name}");
                    assert!(pipe_name.starts_with(r"\\.\pipe\"));
                }
                Err(e) => {
                    panic!("Failed to resolve MSSQLLocalDB (expected to be installed): {e}");
                }
            }
        }
    }

    fn instance_info(is_running: bool, connection: &str) -> LocalDBInstanceInfo {
        let mut info = LocalDBInstanceInfo {
            is_running: is_running as i32,
            ..Default::default()
        };
        let wide: Vec<u16> = connection.encode_utf16().collect();
        info.connection[..wide.len()].copy_from_slice(&wide);
        info
    }

    #[test]
    fn running_pipe_of_running_instance_drops_np_prefix() {
        let info = instance_info(true, r"np:\\.\pipe\LOCALDB#1A2B3C4D\tsql\query");
        assert_eq!(
            running_pipe(&info).as_deref(),
            Some(r"\\.\pipe\LOCALDB#1A2B3C4D\tsql\query")
        );
    }

    #[test]
    fn running_pipe_keeps_pipe_without_np_prefix() {
        let info = instance_info(true, r"\\.\pipe\LOCALDB#1A2B3C4D\tsql\query");
        assert_eq!(
            running_pipe(&info).as_deref(),
            Some(r"\\.\pipe\LOCALDB#1A2B3C4D\tsql\query")
        );
    }

    #[test]
    fn running_pipe_none_for_running_instance_without_pipe() {
        assert_eq!(running_pipe(&instance_info(true, "")), None);
        assert_eq!(running_pipe(&instance_info(true, "np:")), None);
    }

    #[test]
    fn running_pipe_none_for_stopped_instance() {
        let info = instance_info(false, r"np:\\.\pipe\LOCALDB#1A2B3C4D\tsql\query");
        assert_eq!(running_pipe(&info), None);
    }

    /// A current-thread runtime that counts the threads it starts, which are
    /// its blocking-pool threads.
    fn counting_runtime() -> (tokio::runtime::Runtime, Arc<AtomicUsize>) {
        let started = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&started);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .on_thread_start(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .build()
            .expect("runtime");
        (runtime, started)
    }

    #[test]
    fn test_running_instance_resolves_without_blocking_thread() {
        // Only on machines with LocalDB installed; starting the instance
        // first makes it the running case.
        let Ok(api) = LocalDBApi::load() else {
            return;
        };
        let started_pipe = api
            .resolve_instance("MSSQLLocalDB")
            .expect("MSSQLLocalDB should start where LocalDB is installed");

        let info = api
            .get_instance_info("MSSQLLocalDB")
            .expect("LocalDBGetInstanceInfo should accept this struct's size");
        assert_eq!(running_pipe(&info), Some(started_pipe.clone()));

        let (runtime, started) = counting_runtime();
        let pipe = runtime
            .block_on(resolve_localdb_instance("MSSQLLocalDB"))
            .expect("resolve running MSSQLLocalDB");
        assert_eq!(pipe, started_pipe);
        assert_eq!(started.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_unknown_instance_fails_as_starting_it_does() {
        let Ok(api) = LocalDBApi::load() else {
            return;
        };
        let name = format!("MssqlRsNoSuchInstance{}", std::process::id());
        // LocalDBGetInstanceInfo succeeds for an unknown name, reporting it
        // neither existing nor running, so it takes the start path.
        let info = api.get_instance_info(&name);
        if let Ok(info) = &info {
            assert_eq!(info.exists, 0);
        }
        assert_eq!(info.ok().and_then(|info| running_pipe(&info)), None);
        let expected = api
            .resolve_instance(&name)
            .expect_err("unknown instance should not start")
            .to_string();

        let (runtime, started) = counting_runtime();
        let error = runtime
            .block_on(resolve_localdb_instance(&name))
            .expect_err("unknown instance should not resolve");
        assert_eq!(error.to_string(), expected);
        assert!(started.load(Ordering::SeqCst) > 0);
    }
}
