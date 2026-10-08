// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows LocalDB API bindings and connection logic.
//!
//! This module provides FFI bindings to the Windows LocalDB API (sqluserinstance.dll)
//! and implements the logic to resolve LocalDB instance names to named pipe paths.
//!
//! LocalDB API Reference:
//! https://learn.microsoft.com/en-us/sql/relational-databases/express-localdb-instance-apis/sql-server-express-localdb-reference-instance-apis

use crate::core::TdsResult;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use tracing::{debug, info};
use windows::Win32::Foundation::{ERROR_SUCCESS, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{PCWSTR, PWSTR};

/// Maximum size for the LocalDB instance pipe name buffer (in characters).
/// Based on SQL Server LocalDB API documentation.
const LOCALDB_MAX_SQLCONNECTION_BUFFER_SIZE: usize = 260;

/// LocalDB API version 11.0 (SQL Server 2012)
#[allow(dead_code)] // Defined for completeness alongside the LocalDB API bindings
const LOCALDB_VERSION_11_0: u32 = 0x0B000000;

/// LocalDB instance information structure
#[repr(C)]
#[derive(Debug)]
pub(crate) struct LocalDBInstanceInfo {
    /// Size of the LocalDBInstanceInfo struct
    cb_size: u32,
    /// Instance state
    instance_state: u32,
    /// Named pipe to use to communicate with the instance
    ws_connection: [u16; LOCALDB_MAX_SQLCONNECTION_BUFFER_SIZE],
}

impl Default for LocalDBInstanceInfo {
    fn default() -> Self {
        Self {
            cb_size: std::mem::size_of::<LocalDBInstanceInfo>() as u32,
            instance_state: 0,
            ws_connection: [0; LOCALDB_MAX_SQLCONNECTION_BUFFER_SIZE],
        }
    }
}

/// LocalDB instance states
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstanceState {
    /// Instance exists and is running
    Running = 1,
    /// Instance exists but is stopped
    Stopped = 2,
}

/// Type alias for LocalDBStartInstance function pointer
#[allow(non_snake_case)]
type LocalDBStartInstanceFn = unsafe extern "system" fn(
    pwsz_instance: PCWSTR,
    dw_flags: u32,
    wszSqlConnection: PWSTR,
    lpcchSqlConnection: *mut u32,
) -> i32;

/// Type alias for LocalDBGetInstanceInfo function pointer
#[allow(non_snake_case)]
type LocalDBGetInstanceInfoFn = unsafe extern "system" fn(
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
    /// LocalDBInstanceInfo structure containing instance state and connection string
    #[allow(dead_code)]
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
                "LocalDB instance info retrieved: state={}, connection={}",
                instance_info.instance_state,
                from_wide_string(&instance_info.ws_connection)
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
        let pipe_name = self.start_instance(instance_name)?;

        // The pipe name from LocalDB API uses "np:" prefix, but we need the raw pipe path
        // Format: "np:\\\\.\\.\\pipe\\LOCALDB#<hash>\\tsql\\query"
        // We need: "\\\\.\\.\\pipe\\LOCALDB#<hash>\\tsql\\query"
        let cleaned_pipe_name = pipe_name
            .strip_prefix("np:")
            .map(|s| s.to_string())
            .unwrap_or(pipe_name);

        info!(
            "LocalDB instance '{}' resolved to pipe: {}",
            instance_name, cleaned_pipe_name
        );

        Ok(cleaned_pipe_name)
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

/// Resolve a LocalDB instance name to a named pipe path
///
/// This is the public API function that can be called from the transport layer.
/// It loads the LocalDB API, resolves the instance, and returns the pipe path.
///
/// # Arguments
/// * `instance_name` - The LocalDB instance name (e.g., "MSSQLLocalDB", "v11.0")
///
/// # Returns
/// The named pipe path to connect to the instance
pub async fn resolve_localdb_instance(instance_name: &str) -> TdsResult<String> {
    resolve_localdb_pipe(instance_name)
}

/// Resolves a LocalDB instance name (for example `MSSQLLocalDB`) to the named
/// pipe path to connect to, starting the instance if it is stopped.
///
/// This blocks: it loads the LocalDB API and may wait for the instance to
/// start, so async callers should run it on a thread of its own.
pub fn resolve_localdb_pipe(instance_name: &str) -> TdsResult<String> {
    info!("Resolving LocalDB instance: {}", instance_name);

    // Load the LocalDB API
    let api = LocalDBApi::load()?;

    // Resolve the instance to a named pipe path
    let pipe_name = api.resolve_instance(instance_name)?;

    Ok(pipe_name)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
