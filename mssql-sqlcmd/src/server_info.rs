// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The database server properties usage telemetry may report after a
//! successful connection: engine edition, SQL Server version, server operating
//! system and server platform.
//!
//! The raw values are read from the server with one read-only query, defined
//! once in [`server_info_columns!`]: [`EXTRA_COLUMNS`] is what a query that
//! already selects the engine edition appends, and [`QUERY`] is the whole query
//! for native sqlcmd. They never leave this module as read: each is validated
//! against a fixed set or shape, and a value that is unavailable or does not
//! validate is omitted, so no server-controlled free text can reach an event.

/// The columns after `EngineEdition`, in the order [`RawServerInfo`] is filled
/// from a row: the version, the OS and the architecture of the SQL Server
/// build. The OS and architecture come from `@@VERSION` because `xp_msver` and
/// `sys.dm_os_host_info` are not available on Azure SQL Database. The build's
/// architecture is the parenthesized marker after the version number (`(X64)`,
/// `(Intel X86)`, `(ARM64)`); the `<X64>` after the OS name is the OS's, which
/// differs for a 32-bit instance under WOW64.
macro_rules! server_info_columns {
    // The `nvarchar(128)` below is `MAX_TEXT_UNITS`; a test keeps them equal.
    () => {
        concat!(
            "CONVERT(nvarchar(128), SERVERPROPERTY('ProductVersion')), ",
            "CASE WHEN @@VERSION LIKE '%on Linux%' THEN N'linux' ",
            "WHEN @@VERSION LIKE '%on Windows%' THEN N'windows' END, ",
            "CASE WHEN @@VERSION LIKE '%(X64)%' THEN N'x64' ",
            "WHEN @@VERSION LIKE '%(Intel X86)%' THEN N'x86' ",
            "WHEN @@VERSION LIKE '%(ARM64)%' THEN N'arm64' END"
        )
    };
}

/// The longest text a server column may return, in UTF-16 code units; the C ABI
/// rejects longer ones.
pub(crate) const MAX_TEXT_UNITS: usize = 128;

/// What a query that already selects the engine edition appends.
pub const EXTRA_COLUMNS: &str = server_info_columns!();

/// The whole query for native sqlcmd, as a C string, so that the NUL the C ABI
/// needs is part of the type and never sent to a server as SQL.
pub const QUERY: &std::ffi::CStr = match std::ffi::CStr::from_bytes_with_nul(
    concat!(
        "SELECT CONVERT(int, SERVERPROPERTY('EngineEdition')), ",
        server_info_columns!(),
        "\0"
    )
    .as_bytes(),
) {
    Ok(query) => query,
    Err(_) => panic!("QUERY must end in exactly one NUL"),
};

pub const KEY_ENGINE_EDITION: &str = "sqlcmdcli_engine_edition";
pub const KEY_SERVER_VERSION: &str = "sqlcmdcli_server_version";
pub const KEY_SERVER_OS: &str = "sqlcmdcli_server_os";
pub const KEY_SERVER_PLATFORM: &str = "sqlcmdcli_server_platform";

/// Values as the server returned them. `None` is SQL `NULL`; a negative
/// `engine_edition` is "not read".
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawServerInfo {
    pub engine_edition: i32,
    pub product_version: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
}

impl Default for RawServerInfo {
    fn default() -> Self {
        Self {
            engine_edition: -1,
            product_version: None,
            os: None,
            arch: None,
        }
    }
}

/// What may be reported. Every value is validated; `None` is omitted.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServerInfo {
    pub engine_edition: Option<i32>,
    pub server_version: Option<String>,
    pub server_os: Option<&'static str>,
    pub server_platform: Option<&'static str>,
}

impl ServerInfo {
    pub fn classify(raw: &RawServerInfo) -> Self {
        let os = raw.os.as_deref().and_then(os_label);
        let arch = raw.arch.as_deref().and_then(arch_label);
        Self {
            engine_edition: (0..=99)
                .contains(&raw.engine_edition)
                .then_some(raw.engine_edition),
            server_version: raw.product_version.as_deref().and_then(server_version),
            server_os: os,
            server_platform: platform(os, arch),
        }
    }

    /// The event properties, present ones only.
    pub fn properties(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(edition) = self.engine_edition {
            out.push((KEY_ENGINE_EDITION, edition.to_string()));
        }
        if let Some(version) = &self.server_version {
            out.push((KEY_SERVER_VERSION, version.clone()));
        }
        if let Some(os) = self.server_os {
            out.push((KEY_SERVER_OS, os.to_string()));
        }
        if let Some(platform) = self.server_platform {
            out.push((KEY_SERVER_PLATFORM, platform.to_string()));
        }
        out
    }
}

/// Whether `value` is one `key` may carry. The event filter applies this to
/// every property once more before anything is sent.
pub fn value_is_valid(key: &str, value: &str) -> bool {
    match key {
        KEY_ENGINE_EDITION => {
            !value.is_empty() && value.len() <= 2 && value.bytes().all(|b| b.is_ascii_digit())
        }
        KEY_SERVER_VERSION => server_version(value).as_deref() == Some(value),
        KEY_SERVER_OS => matches!(value, "windows" | "linux"),
        KEY_SERVER_PLATFORM => PLATFORMS.contains(&value),
        _ => false,
    }
}

const PLATFORMS: [&str; 5] = [
    "NT x64",
    "NT INTEL X86",
    "NT ARM64",
    "Linux x64",
    "Linux ARM64",
];

fn os_label(os: &str) -> Option<&'static str> {
    match os.trim() {
        "windows" => Some("windows"),
        "linux" => Some("linux"),
        _ => None,
    }
}

fn arch_label(arch: &str) -> Option<&'static str> {
    match arch.trim() {
        "x64" => Some("x64"),
        "x86" => Some("x86"),
        "arm64" => Some("arm64"),
        _ => None,
    }
}

/// The platform in the labels DacFx reports (`xp_msver` "Platform").
fn platform(os: Option<&'static str>, arch: Option<&'static str>) -> Option<&'static str> {
    match (os?, arch?) {
        ("windows", "x64") => Some("NT x64"),
        ("windows", "x86") => Some("NT INTEL X86"),
        ("windows", "arm64") => Some("NT ARM64"),
        ("linux", "x64") => Some("Linux x64"),
        ("linux", "arm64") => Some("Linux ARM64"),
        _ => None,
    }
}

/// `17.0.5015.4`: two to four dot-separated numbers.
fn server_version(version: &str) -> Option<String> {
    let version = version.trim();
    let parts: Vec<&str> = version.split('.').collect();
    let ok = (2..=4).contains(&parts.len())
        && parts.iter().all(|part| {
            !part.is_empty() && part.len() <= 5 && part.bytes().all(|b| b.is_ascii_digit())
        });
    ok.then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(engine: i32, version: &str, os: Option<&str>, arch: Option<&str>) -> RawServerInfo {
        RawServerInfo {
            engine_edition: engine,
            product_version: Some(version.to_string()),
            os: os.map(str::to_string),
            arch: arch.map(str::to_string),
        }
    }

    #[test]
    fn azure_sql_database_reports_edition_and_version_but_no_os_or_platform() {
        let info = ServerInfo::classify(&raw(5, "12.0.2000.8", None, None));
        assert_eq!(
            info.properties(),
            vec![
                (KEY_ENGINE_EDITION, "5".to_string()),
                (KEY_SERVER_VERSION, "12.0.2000.8".to_string()),
            ]
        );
    }

    #[test]
    fn box_sql_server_reports_the_platform_in_dacfx_labels() {
        let info = ServerInfo::classify(&raw(2, "17.0.5015.4", Some("windows"), Some("x64")));
        assert_eq!(
            info.properties(),
            vec![
                (KEY_ENGINE_EDITION, "2".to_string()),
                (KEY_SERVER_VERSION, "17.0.5015.4".to_string()),
                (KEY_SERVER_OS, "windows".to_string()),
                (KEY_SERVER_PLATFORM, "NT x64".to_string()),
            ]
        );
        for (os, arch, label) in [
            ("windows", "x86", "NT INTEL X86"),
            ("windows", "arm64", "NT ARM64"),
            ("linux", "x64", "Linux x64"),
            ("linux", "arm64", "Linux ARM64"),
        ] {
            let info = ServerInfo::classify(&raw(3, "16.0.4100.1", Some(os), Some(arch)));
            assert_eq!(info.server_platform, Some(label), "{os} {arch}");
        }
    }

    #[test]
    fn a_platform_needs_both_the_os_and_the_architecture() {
        let info = ServerInfo::classify(&raw(2, "17.0.5015.4", Some("windows"), None));
        assert_eq!(info.server_os, Some("windows"));
        assert_eq!(info.server_platform, None);
    }

    #[test]
    fn nothing_the_server_controls_passes_through_unvalidated() {
        let info =
            ServerInfo::classify(&raw(12345, "see C:\\private", Some("plan9"), Some("riscv")));
        assert_eq!(info, ServerInfo::default());
        assert!(info.properties().is_empty());
    }

    #[test]
    fn versions_must_have_the_expected_shape() {
        assert_eq!(
            server_version("17.0.5015.4").as_deref(),
            Some("17.0.5015.4")
        );
        assert_eq!(server_version("17"), None);
        assert_eq!(server_version("17.0.x"), None);
        assert_eq!(server_version("17.0.5015.4.1"), None);
    }

    #[test]
    fn the_query_and_the_appended_columns_are_one_definition() {
        let query = QUERY.to_str().expect("the query is UTF-8");
        assert!(query.starts_with("SELECT CONVERT(int, SERVERPROPERTY('EngineEdition')), "));
        assert!(query.ends_with(EXTRA_COLUMNS));
        // Four columns in all: edition, version, OS and architecture.
        assert_eq!(query.matches("SERVERPROPERTY(").count(), 2);
        assert_eq!(query.matches("CASE WHEN").count(), 2);
        assert!(query.contains(&format!("nvarchar({MAX_TEXT_UNITS})")));
    }

    /// The order is a contract with `raw_server_info_from_row` and with native
    /// sqlcmd's `MssqlSqlcmdServerInfo`: version, then OS, then architecture.
    #[test]
    fn the_columns_come_in_the_order_the_readers_expect() {
        let at = |needle: &str| EXTRA_COLUMNS.find(needle).expect(needle);
        assert!(at("'ProductVersion'") < at("'%on Linux%'"));
        assert!(at("'%on Linux%'") < at("'%(X64)%'"));
    }

    /// The architecture is the build's `(X64)`, not the OS's `<X64>`, so a 32-bit
    /// instance under WOW64 reads as x86.
    #[test]
    fn the_architecture_is_the_builds_not_the_operating_systems() {
        assert!(EXTRA_COLUMNS.contains("'%(X64)%'"));
        assert!(EXTRA_COLUMNS.contains("'%(Intel X86)%'"));
        assert!(EXTRA_COLUMNS.contains("'%(ARM64)%'"));
        assert!(!EXTRA_COLUMNS.contains("<X"));
    }

    #[test]
    fn the_filter_check_accepts_only_what_classification_produces() {
        assert!(value_is_valid(KEY_SERVER_PLATFORM, "NT x64"));
        assert!(!value_is_valid(KEY_SERVER_PLATFORM, "Plan 9"));
        assert!(value_is_valid(KEY_ENGINE_EDITION, "8"));
        assert!(!value_is_valid(KEY_ENGINE_EDITION, "800"));
        assert!(!value_is_valid("sqlcmdcli_product_level", "RTM"));
        assert!(!value_is_valid("sqlcmdcli_unknown", "x"));
    }
}
