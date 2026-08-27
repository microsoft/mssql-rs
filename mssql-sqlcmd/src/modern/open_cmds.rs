// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `sqlcmd open ads` — open the current context in Azure Data Studio.
//!
//! The reference stores the password in the OS credential store under a target
//! name Azure Data Studio recognises, then launches the application pointed at
//! the endpoint. Only the Windows half of that is implemented upstream:
//! go-sqlcmd's macOS build writes nothing (a documented encoding mismatch —
//! Azure Data Studio reads UTF-16 from the Keychain, the Go library writes
//! UTF-8) and its Linux build panics outright.
//!
//! This port launches the application on all three platforms, and hands the
//! password over only where that can be done correctly. Where it cannot, the
//! connection still opens and Azure Data Studio prompts for the password —
//! better than the reference's panic, and it never leaves a secret somewhere it
//! cannot be read back.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::Context;

/// Where Azure Data Studio installs itself, most-preferred first. Insiders
/// builds win, matching the reference.
fn search_locations() -> Vec<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();

    #[cfg(windows)]
    {
        let program_files =
            std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_string());
        vec![
            PathBuf::from(&home).join(
                r"AppData\Local\Programs\Azure Data Studio - Insiders\azuredatastudio-insiders.exe",
            ),
            PathBuf::from(&program_files)
                .join(r"Azure Data Studio - Insiders\azuredatastudio-insiders.exe"),
            PathBuf::from(&home)
                .join(r"AppData\Local\Programs\Azure Data Studio\azuredatastudio.exe"),
            PathBuf::from(&program_files).join(r"Azure Data Studio\azuredatastudio.exe"),
        ]
    }

    #[cfg(target_os = "macos")]
    {
        vec![
            PathBuf::from("/Applications/Azure Data Studio - Insiders.app"),
            PathBuf::from(&home).join("Downloads/Azure Data Studio - Insiders.app"),
            PathBuf::from("/Applications/Azure Data Studio.app"),
            PathBuf::from(&home).join("Downloads/Azure Data Studio.app"),
        ]
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // The reference panics here rather than looking. These are where the
        // .deb, .rpm and tarball builds put it.
        vec![
            PathBuf::from("/usr/share/azuredatastudio-insiders/bin/azuredatastudio-insiders"),
            PathBuf::from("/usr/bin/azuredatastudio-insiders"),
            PathBuf::from("/usr/share/azuredatastudio/bin/azuredatastudio"),
            PathBuf::from("/usr/bin/azuredatastudio"),
            PathBuf::from("/snap/bin/azuredatastudio"),
            PathBuf::from(&home).join(".local/bin/azuredatastudio"),
        ]
    }
}

fn locate() -> Option<PathBuf> {
    search_locations().into_iter().find(|p| p.exists())
}

/// What to tell someone who does not have it installed.
fn how_to_install() -> String {
    "Azure Data Studio is not installed.\n\
     Download it from https://aka.ms/azuredatastudio\n"
        .to_string()
}

/// Stores the password where Azure Data Studio will look for it.
///
/// The target name is the profile identity Azure Data Studio builds internally;
/// it has to match byte for byte or the application simply prompts instead.
#[cfg(windows)]
fn persist_credential(server: &str, username: &str, password: &str) -> Result<(), String> {
    let target = format!(
        "Microsoft.SqlTools|itemtype:Profile|id:providerName:MSSQL|applicationName:azdata\
         |authenticationType:SqlLogin|database:|server:{server}|user:{username}"
    );
    windows_credential::write(&target, username, password)
}

#[cfg(not(windows))]
fn persist_credential(_server: &str, _username: &str, _password: &str) -> Result<(), String> {
    // Azure Data Studio reads UTF-16 out of the macOS Keychain and libsecret on
    // Linux. Writing something it cannot decode would be worse than writing
    // nothing: it would look stored and then fail to unlock.
    Err("password hand-off is only implemented on Windows".to_string())
}

/// Opens the current context in Azure Data Studio.
pub fn ads(context: Context) -> Result<String, String> {
    let Some((current, endpoint, user)) = context.config.current() else {
        return Err(crate::modern::config_cmds::no_context());
    };

    let Some(executable) = locate() else {
        return Err(how_to_install());
    };

    // A context backed by a container is no use to Azure Data Studio unless the
    // container is actually up.
    if let Some(container) = &endpoint.container
        && !super::container::Runtime::detect()?.is_running(&container.id)
    {
        return Err("Container is not running\nTo start the container: sqlcmd start".to_string());
    }

    let server = format!("{},{}", endpoint.address, endpoint.port);
    let mut args = vec!["-r".to_string(), format!("--server={server}")];
    let mut notes = String::new();

    match user {
        Some(user) if user.authentication_type == "basic" => {
            let password = crate::modern::config_cmds::base64::decode_text(&user.password);
            // Quotes inside a user name would otherwise end the argument early.
            let username = user.username.replace('"', "\\\"");
            args.push(format!("--user={username}"));
            if let Err(reason) = persist_credential(&server, &user.username, &password) {
                notes.push_str(&format!(
                    "Azure Data Studio will prompt for the password: {reason}.\n"
                ));
            }
        }
        // No stored user means integrated auth, which only Windows can do.
        _ if cfg!(windows) => args.push("--integrated".to_string()),
        _ => {}
    }

    Command::new(&executable)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not launch {}: {e}", executable.display()))?;

    Ok(format!(
        "{notes}Opening \"{}\" in Azure Data Studio\n",
        current.name
    ))
}

/// The Windows Credential Manager, via the two `advapi32` entry points needed
/// to replace a single generic credential.
#[cfg(windows)]
mod windows_credential {
    use std::ffi::c_void;

    const CRED_TYPE_GENERIC: u32 = 1;
    /// Visible to this logon session only, which is what Azure Data Studio
    /// writes for a profile password.
    const CRED_PERSIST_SESSION: u32 = 1;

    #[repr(C)]
    struct Credential {
        flags: u32,
        cred_type: u32,
        target_name: *const u16,
        comment: *const u16,
        last_written: u64,
        credential_blob_size: u32,
        credential_blob: *const u8,
        persist: u32,
        attribute_count: u32,
        attributes: *const c_void,
        target_alias: *const u16,
        user_name: *const u16,
    }

    unsafe extern "system" {
        fn CredWriteW(credential: *const Credential, flags: u32) -> i32;
        fn CredDeleteW(target: *const u16, cred_type: u32, flags: u32) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn write(target: &str, username: &str, password: &str) -> Result<(), String> {
        let target_w = wide(target);
        let username_w = wide(username);
        // Azure Data Studio expects the blob as UTF-16, without a terminator.
        let blob: Vec<u8> = password
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();

        let blob_size = u32::try_from(blob.len())
            .map_err(|_| "password is too long for the credential store".to_string())?;

        // Replace rather than merge: a stale entry for the same target would
        // otherwise keep an old password alive.
        unsafe { CredDeleteW(target_w.as_ptr(), CRED_TYPE_GENERIC, 0) };

        let credential = Credential {
            flags: 0,
            cred_type: CRED_TYPE_GENERIC,
            target_name: target_w.as_ptr(),
            comment: std::ptr::null(),
            last_written: 0,
            credential_blob_size: blob_size,
            credential_blob: blob.as_ptr(),
            persist: CRED_PERSIST_SESSION,
            attribute_count: 0,
            attributes: std::ptr::null(),
            target_alias: std::ptr::null(),
            user_name: username_w.as_ptr(),
        };

        // SAFETY: every pointer above outlives the call, and the blob size is
        // the true length of `blob`.
        let ok = unsafe { CredWriteW(&credential, 0) };
        if ok == 0 {
            return Err(format!(
                "the credential store refused the password (error {})",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insiders_builds_are_preferred() {
        let locations = search_locations();
        let first_insiders = locations
            .iter()
            .position(|p| p.to_string_lossy().to_lowercase().contains("insiders"));
        let first_stable = locations
            .iter()
            .position(|p| !p.to_string_lossy().to_lowercase().contains("insiders"));
        assert!(
            first_insiders < first_stable,
            "an Insiders install should be found first: {locations:?}"
        );
    }

    #[test]
    fn every_platform_has_somewhere_to_look() {
        // The reference panics on Linux rather than searching; this port must
        // always have candidates.
        assert!(!search_locations().is_empty());
    }

    #[test]
    fn the_install_hint_names_the_download() {
        assert!(how_to_install().contains("aka.ms/azuredatastudio"));
    }
}
