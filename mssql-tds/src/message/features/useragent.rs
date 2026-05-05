#![allow(dead_code)]
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;
use std::sync::OnceLock;

use crate::connection::client_context::ClientContext;
use crate::core::TdsResult;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use crate::message::login::{Feature, FeatureExtension};

const UNKNOWN_VAL: &str = "Unknown";
const FORMAT_VERSION: &str = "1";
const DEFAULT_DRIVER_NAME: &str = "MS-TDS";

// Field length limits following MS driver standards
const MAX_ARCH_LEN: usize = 10;
const MAX_OS_TYPE_LEN: usize = 10;
const MAX_OS_DETAILS_LEN: usize = 44;
const MAX_DRIVER_NAME_LEN: usize = 12;
const MAX_DRIVER_VER_LEN: usize = 24;
const MAX_RUNTIME_LEN: usize = 44;

// Static cache for expensive OS environment detections to avoid overhead on every connection.
// This structure holds data that is strictly bound to the OS process and will never change.
static SYSTEM_ENV_CACHE: OnceLock<SystemEnvironmentInfo> = OnceLock::new();

#[derive(Debug, Clone)]
struct SystemEnvironmentInfo {
    architecture: String,
    os_type: String,
    os_details: String,
    fallback_runtime: String,
}

impl SystemEnvironmentInfo {
    /// Detects and caches exactly once the OS environment properties per process lifetime.
    fn detect() -> Self {
        SystemEnvironmentInfo {
            architecture: sanitize_field(std::env::consts::ARCH, MAX_ARCH_LEN),
            os_type: sanitize_field(get_os_type(), MAX_OS_TYPE_LEN),
            os_details: sanitize_field(&get_os_details(), MAX_OS_DETAILS_LEN),
            fallback_runtime: sanitize_field(UNKNOWN_VAL, MAX_RUNTIME_LEN),
        }
    }
}

/// Helper function to map a string OS name to the spec-required OS Type
fn get_os_type_from_name(os_name: &str) -> &'static str {
    match os_name {
        "windows" => "Windows",
        "linux" => "Linux",
        "macos" => "macOS",
        "freebsd" => "FreeBSD",
        "android" => "Android",
        // The specification strictly limits OS types to Windows, Linux, macOS, FreeBSD, Android, and Unknown.
        // Any unsupported OS, such as iOS, must explicitly fallback to "Unknown".
        _ => UNKNOWN_VAL,
    }
}

/// Helper function to detect the OS Type string according to requested values
fn get_os_type() -> &'static str {
    get_os_type_from_name(std::env::consts::OS)
}

#[cfg(target_os = "windows")]
fn get_windows_version() -> String {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;

    // SAFETY: `info` is a valid, properly sized OSVERSIONINFOW. RtlGetVersion writes
    // into it and returns NTSTATUS (0 == STATUS_SUCCESS).
    let status = unsafe { RtlGetVersion(&mut info) };
    if status.is_ok() {
        return format!(
            "Windows {}.{}.{}",
            info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
        );
    }
    UNKNOWN_VAL.to_string()
}

/// Helper function to read the OS specific details (name, distribution, version)
fn get_os_details() -> String {
    #[cfg(target_os = "windows")]
    {
        get_windows_version()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut info: libc::utsname = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is properly initialized with zero and represents the structure `uname` expects to fill.
        if unsafe { libc::uname(&mut info) } == 0 {
            // SAFETY: sysname and release are null-terminated byte arrays populated by the kernel.
            let sysname =
                unsafe { std::ffi::CStr::from_ptr(info.sysname.as_ptr()) }.to_string_lossy();
            let release =
                unsafe { std::ffi::CStr::from_ptr(info.release.as_ptr()) }.to_string_lossy();
            format!("{} {}", sysname, release)
        } else {
            UNKNOWN_VAL.to_string()
        }
    }
}

/// The feature extension carrying the exact user-agent string for login.
#[derive(Debug, Clone)]
pub struct UserAgentFeature {
    payload: String,
}

impl UserAgentFeature {
    /// Builds a sanitized user agent payload exactly dynamically merging
    /// cached process environment info + dynamic FFI context info.
    pub fn new(context: &ClientContext) -> Self {
        let env_info = SYSTEM_ENV_CACHE.get_or_init(SystemEnvironmentInfo::detect);

        let driver_name = {
            let name = &context.user_agent.library_name;
            // Fallback to default if an FFI caller explicitly set an empty string
            sanitize_field(
                if name.is_empty() {
                    DEFAULT_DRIVER_NAME
                } else {
                    name
                },
                MAX_DRIVER_NAME_LEN,
            )
        };

        let driver_version = {
            let ver = &context.user_agent.driver_version;
            let base_ver = context.driver_version.to_string();
            // Fallback to default if an FFI caller explicitly set an empty string
            sanitize_field(
                if ver.is_empty() { &base_ver } else { ver },
                MAX_DRIVER_VER_LEN,
            )
        };

        let runtime_details = {
            let v = &context.user_agent.runtime;
            // Fallback to default if an FFI caller explicitly set an empty string or "Unknown"
            if v.is_empty() || v == UNKNOWN_VAL {
                env_info.fallback_runtime.clone()
            } else {
                sanitize_field(v, MAX_RUNTIME_LEN)
            }
        };

        // Format is strictly: {Format Version}|{Driver Name}|{Driver Version}|{Architecture}|{OS Type}|{OS Details}|{Runtime Identifier}
        let payload = format!(
            "{}|{}|{}|{}|{}|{}|{}",
            FORMAT_VERSION,
            driver_name,
            driver_version,
            env_info.architecture,
            env_info.os_type,
            env_info.os_details,
            runtime_details
        );

        // We mathematically guarantee the maximum length of this assembled string is
        // 2 (format version) + 12 + 24 + 10 + 10 + 44 + 44 + 6 (delimiters) = 152 chars.
        // Because 152 <= 255 (the SQL Server limit) and `sanitize_field` strips out all '|'
        // from the inputs, we are guaranteed to never truncate delimiters or exceed limits.
        UserAgentFeature { payload }
    }
}

#[async_trait]
impl Feature for UserAgentFeature {
    fn feature_identifier(&self) -> FeatureExtension {
        FeatureExtension::UserAgent
    }

    fn is_requested(&self) -> bool {
        true
    }

    fn data_length(&self) -> i32 {
        // Each UTF-16 character is 2 bytes, so multiply the u16 count by 2 to get the total byte length
        let utf16_len = self.payload.encode_utf16().count() * 2;
        // 1 byte for feature identifier, 4 bytes for length, utf16_len bytes for payload
        (size_of::<u8>() + size_of::<i32>() + utf16_len) as i32
    }

    async fn serialize(&self, packet_writer: &mut PacketWriter) -> TdsResult<()> {
        // Each UTF-16 character is 2 bytes, so multiply the u16 count by 2 to get the total byte length
        let utf16_len = self.payload.encode_utf16().count() * 2;
        packet_writer
            .write_byte_async(self.feature_identifier().as_u8())
            .await?;
        packet_writer.write_i32_async(utf16_len as i32).await?;
        packet_writer
            .write_string_unicode_async(&self.payload)
            .await?;
        Ok(())
    }

    fn deserialize(&mut self, _data: &[u8]) -> TdsResult<()> {
        Ok(())
    }

    fn is_acknowledged(&self) -> bool {
        false
    }

    fn set_acknowledged(&mut self, _acknowledged: bool) {}

    fn clone_box(&self) -> Box<dyn Feature> {
        Box::new(self.clone())
    }
}

/// Sanitizes a single field to match strict spec requirements.
/// Replaces characters matching `[a-zA-Z0-9 \.\+_-]` with themselves and filters out other things (especially pipes `|`).
/// Finally truncates to max_len and falls back to "Unknown" if the field is empty.
fn sanitize_field(val: &str, max_len: usize) -> String {
    let mut sanitized = String::with_capacity(val.len().min(max_len));

    for ch in val.chars() {
        if sanitized.len() >= max_len {
            break;
        }
        if ch.is_ascii_alphanumeric()
            || ch == ' '
            || ch == '.'
            || ch == '+'
            || ch == '_'
            || ch == '-'
        {
            sanitized.push(ch);
        }
    }

    let trimmed = sanitized.trim();

    if trimmed.is_empty() {
        return UNKNOWN_VAL.to_string();
    }

    if trimmed.len() > max_len {
        trimmed[..max_len].to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_field_valid_chars() {
        // Field should allow alphanumeric, space, period, plus, underscore, hyphen
        assert_eq!(
            sanitize_field("Rust 1.76.0+build_1-rc", 50),
            "Rust 1.76.0+build_1-rc"
        );
    }

    #[test]
    fn test_sanitize_field_removes_invalid_chars() {
        // Field should actively remove pipes, angles, commas, etc.
        assert_eq!(
            sanitize_field("Ubuntu|Linux>22.04,x86", 50),
            "UbuntuLinux22.04x86"
        );
    }

    #[test]
    fn test_sanitize_field_truncation() {
        // Field should cleanly truncate to max length without panicking
        let long_str = "VeryLongStringMoreThan12Characters";
        assert_eq!(sanitize_field(long_str, 12), "VeryLongStri");
        assert_eq!(sanitize_field(long_str, 12).chars().count(), 12);
    }

    #[test]
    fn test_sanitize_field_whitespace_trimming() {
        // Field should trim leading/trailing whitespace
        assert_eq!(sanitize_field("  Space Trimmed  ", 50), "Space Trimmed");
        assert_eq!(sanitize_field("   ", 50), "Unknown"); // Empty after trim
    }

    #[test]
    fn test_sanitize_field_fallback() {
        // Field should gracefully fallback to "Unknown" when entirely empty or invalidated
        assert_eq!(sanitize_field("", 50), "Unknown");
        assert_eq!(sanitize_field("|||<>|||", 50), "Unknown");
    }

    #[test]
    fn test_os_type_mapping() {
        assert_eq!(get_os_type_from_name("windows"), "Windows");
        assert_eq!(get_os_type_from_name("linux"), "Linux");
        assert_eq!(get_os_type_from_name("macos"), "macOS");
        assert_eq!(get_os_type_from_name("freebsd"), "FreeBSD");
        assert_eq!(get_os_type_from_name("android"), "Android");
        // iOS is intentionally mapped to Unknown as per strict spec constraints
        assert_eq!(get_os_type_from_name("ios"), "Unknown");
        assert_eq!(get_os_type_from_name("solaris"), "Unknown");
    }

    #[test]
    fn test_user_agent_builder_defaults() {
        let context = ClientContext::with_data_source("tcp:test");
        let feature = UserAgentFeature::new(&context);

        let parts: Vec<&str> = feature.payload.split('|').collect();
        assert_eq!(
            parts.len(),
            7,
            "User payload must contain exactly 7 pipe-delimited fields"
        );

        assert_eq!(parts[0], "1");
        assert_eq!(parts[1], "MS-TDS");

        // Assert that at least some known environment values appeared.
        assert!(!parts[3].is_empty()); // Arch
        assert!(!parts[4].is_empty()); // OS Type
        assert!(!parts[6].is_empty()); // Runtime
    }

    #[test]
    fn test_user_agent_builder_custom_ffi() {
        let mut context = ClientContext::with_data_source("tcp:test");
        context.library_name = "mssql-python".to_string();
        context.user_agent.set_library_name("MS-PYTHON".to_string());
        context.user_agent.set_runtime("CPython 3.12.3".to_string());

        let feature = UserAgentFeature::new(&context);
        let parts: Vec<&str> = feature.payload.split('|').collect();

        assert_eq!(parts[1], "MS-PYTHON"); // Driver Name overrides library_name
        assert_eq!(parts[6], "CPython 3.12.3"); // Dynamic FFI runtime mapped
    }

    #[test]
    fn test_system_environment_info_detection() {
        // Validates that probing the host metrics (like `uname` running or FFI syscalls)
        // won't panic and gracefully produces strings internally.
        let env_info = SystemEnvironmentInfo::detect();

        assert!(
            !env_info.architecture.is_empty(),
            "Architecture should be populated"
        );
        assert!(!env_info.os_type.is_empty(), "OS type should be populated");
        assert!(
            !env_info.os_details.is_empty(),
            "OS details should be populated"
        );
        assert_eq!(
            env_info.fallback_runtime, "Unknown",
            "Fallback runtime should default to Unknown"
        );

        println!("Environment APIs detected on this test run:");
        println!("  Architecture: {}", env_info.architecture);
        println!("  OS Type: {}", env_info.os_type);
        println!("  OS Details: {}", env_info.os_details);
        println!("  Fallback Runtime: {}", env_info.fallback_runtime);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_get_windows_version_no_panic() {
        // Ensure that using the windows crate's OSVERSIONINFOW
        // doesn't crash the test runner on native Windows.
        let version = get_windows_version();
        assert!(!version.is_empty());
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn test_get_os_details_execution() {
        // Ensure FFI execution (`uname`) correctly falls back or yields a string
        let os_details = get_os_details();
        assert!(!os_details.is_empty());
        assert_ne!(os_details, UNKNOWN_VAL);
    }

    #[test]
    fn test_useragent_feature_methods() {
        let context = ClientContext::with_data_source("tcp:test");
        let mut feature = UserAgentFeature::new(&context);

        // Should acknowledge by default based on boolean flag
        assert!(!feature.is_acknowledged());

        // deserialize is a no-op for UserAgent
        let buf = bytes::BytesMut::new();
        assert!(feature.deserialize(&buf).is_ok());
    }
}
