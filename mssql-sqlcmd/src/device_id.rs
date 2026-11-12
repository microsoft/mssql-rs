// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The DevDeviceID for usage telemetry.
//!
//! A random lowercase UUID shared by Microsoft developer tools on the same
//! device, stored per user where `vscode-deviceid` stores it:
//!
//! - Windows: the registry value `deviceid` under
//!   `HKEY_CURRENT_USER\SOFTWARE\Microsoft\DeveloperTools` (64-bit view).
//! - macOS: `~/Library/Application Support/Microsoft/DeveloperTools/deviceid`.
//! - Linux: `$XDG_CACHE_HOME` (or `~/.cache`)`/Microsoft/DeveloperTools/deviceid`.
//!
//! sqlcmd reuses the shared ID and creates one only when none exists. It never
//! overwrites a stored value it cannot read or does not recognize, so it cannot
//! break correlation with the other tools. It is not derived from hardware or
//! a login. When the ID cannot be read, created or stored, there is none and
//! the event omits it.

use std::io;

/// Where the device ID is kept.
pub trait Store {
    /// The stored text, or `None` when nothing is stored yet.
    fn read(&self) -> io::Result<Option<String>>;

    /// Stores `id` only if nothing is stored, and returns what is stored
    /// afterwards: `id`, or the value another process stored first. A store must
    /// make this safe against concurrent first runs, so that all of them report
    /// the one ID that survives.
    fn create_if_absent(&self, id: &str) -> io::Result<String>;
}

/// The device ID of this user on this device, created if it does not exist.
pub fn get_or_create() -> Option<String> {
    let store = default_store()?;
    get_or_create_in(&*store, || uuid::Uuid::new_v4().hyphenated().to_string())
}

/// [`get_or_create`] with the store and the generator given.
pub fn get_or_create_in(store: &dyn Store, generate: impl FnOnce() -> String) -> Option<String> {
    match store.read() {
        // A stored value is used as it is, or not at all: it is not replaced.
        Ok(Some(text)) => normalize(&text),
        Ok(None) => {
            let id = normalize(&generate())?;
            // The winner of a concurrent first run, which may not be `id`.
            normalize(&store.create_if_absent(&id).ok()?)
        }
        Err(_) => None,
    }
}

/// `text` as a lowercase `8-4-4-4-12` UUID, or `None` when it is not one.
/// A leading byte order mark and surrounding whitespace are ignored.
pub fn normalize(text: &str) -> Option<String> {
    let text = text.trim_start_matches('\u{feff}').trim();
    is_hyphenated_guid(text).then(|| text.to_ascii_lowercase())
}

/// Whether `text`, as it is, is a `8-4-4-4-12` hexadecimal GUID.
pub(crate) fn is_hyphenated_guid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// The ID in a file, as on macOS and Linux.
pub struct FileStore {
    pub path: std::path::PathBuf,
}

/// The most the ID file is read for. An ID is 36 characters; a byte order mark
/// and a line ending fit well within this, and anything longer is not an ID.
const MAX_FILE_BYTES: u64 = 256;

impl Store for FileStore {
    /// Reads a regular file of at most [`MAX_FILE_BYTES`]. Anything else at the
    /// path (a directory, a FIFO or device that could block, a huge file) is an
    /// error: nothing is read from it and nothing replaces it.
    fn read(&self) -> io::Result<Option<String>> {
        use std::io::Read;
        let invalid = |reason| io::Error::new(io::ErrorKind::InvalidData, reason);
        // Checked before opening: opening a FIFO for reading waits for a writer.
        match std::fs::metadata(&self.path) {
            Ok(metadata) if !metadata.is_file() => return Err(invalid("not a regular file")),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid("too long to be a device ID"));
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| invalid("not text"))
    }

    /// The ID is written to a file of its own and then linked into place, which
    /// fails if the ID file exists. So a first run either installs a complete
    /// file or finds the one another run installed, and a reader never sees a
    /// partly written ID.
    fn create_if_absent(&self, id: &str) -> io::Result<String> {
        let parent = self
            .path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let staged = StagedFile(parent.join(format!(
            ".deviceid.{}.{}.tmp",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        )));
        std::fs::write(&staged.0, id)?;
        let linked = std::fs::hard_link(&staged.0, &self.path);
        // Gone before the winner's file is read below.
        drop(staged);
        match linked {
            Ok(()) => Ok(id.to_owned()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => self.read()?.ok_or(error),
            Err(error) => Err(error),
        }
    }
}

/// A staged ID file, removed however `create_if_absent` exits: a failed write
/// (a full disk) or link as well as a success.
struct StagedFile(std::path::PathBuf);

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Where the ID file is, for the given `HOME` and `XDG_CACHE_HOME` (macOS
/// ignores the latter). `XDG_CACHE_HOME` is used by itself when it is an absolute
/// path, as the XDG specification requires, so a service or container that sets
/// only it keeps working without `HOME`; `HOME` is needed for the `~/.cache`
/// fallback and on macOS.
#[cfg_attr(windows, allow(dead_code))]
fn file_store_path(
    home: Option<std::ffi::OsString>,
    xdg_cache_home: Option<std::ffi::OsString>,
    macos: bool,
) -> Option<std::path::PathBuf> {
    let home = home.filter(|home| !home.is_empty());
    let base = if macos {
        std::path::PathBuf::from(home?)
            .join("Library")
            .join("Application Support")
    } else {
        let xdg = xdg_cache_home
            .filter(|dir| !dir.is_empty())
            .map(std::path::PathBuf::from)
            .filter(|dir| dir.is_absolute());
        match xdg {
            Some(dir) => dir,
            None => std::path::PathBuf::from(home?).join(".cache"),
        }
    };
    Some(
        base.join("Microsoft")
            .join("DeveloperTools")
            .join("deviceid"),
    )
}

#[cfg(not(windows))]
fn default_store() -> Option<Box<dyn Store>> {
    let path = file_store_path(
        std::env::var_os("HOME"),
        std::env::var_os("XDG_CACHE_HOME"),
        cfg!(target_os = "macos"),
    )?;
    Some(Box::new(FileStore { path }))
}

#[cfg(windows)]
fn default_store() -> Option<Box<dyn Store>> {
    Some(Box::new(RegistryStore))
}

/// The ID in the registry, as on Windows.
#[cfg(windows)]
struct RegistryStore;

#[cfg(windows)]
mod registry {
    use super::{RegistryStore, Store};
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, HANDLE, LocalFree,
        WAIT_ABANDONED, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WOW64_64KEY, KEY_WRITE, REG_OPTION_NON_VOLATILE,
        REG_SZ, RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    use windows_sys::Win32::System::Threading::{
        CreateMutexW, GetCurrentProcess, OpenProcessToken, ReleaseMutex, WaitForSingleObject,
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn key_path() -> Vec<u16> {
        wide("SOFTWARE\\Microsoft\\DeveloperTools")
    }

    fn os_error(code: u32) -> io::Error {
        io::Error::from_raw_os_error(code as i32)
    }

    /// The current user's SID as text (`S-1-5-21-...`), or `None` when it cannot
    /// be read.
    fn user_sid_string() -> Option<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `token` is a valid out pointer for the current process's token.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return None;
        }
        // SAFETY: the call succeeded, so `token` is an open handle that nothing
        // else owns.
        let token_guard = unsafe { OwnedHandle::from_raw_handle(token) };
        let token = token_guard.as_raw_handle();
        let mut needed = 0u32;
        // SAFETY: a null buffer of length 0 asks for the size.
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 || needed > 4096 {
            return None;
        }
        // 8-byte aligned storage for the TOKEN_USER the call writes.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: `buffer` holds at least `needed` bytes.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return None;
        }
        // SAFETY: the call filled `buffer` with a TOKEN_USER, which is aligned.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` is a valid SID in `buffer`; `text` is a valid out pointer.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 || text.is_null() {
            return None;
        }
        let mut length = 0;
        // SAFETY: `text` is NUL-terminated, allocated by the call above.
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: `text` points to `length` valid UTF-16 units.
        let sid_text =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
        // SAFETY: the string was allocated by `ConvertSidToStringSidW` with
        // `LocalAlloc` and is freed once.
        unsafe { LocalFree(text.cast()) };
        Some(sid_text)
    }

    /// The first-run mutex's name. `Global\` makes it span the user's logon
    /// sessions, as the HKCU value does, and the SID keeps different users from
    /// contending for one name.
    fn first_run_lock_name() -> Option<String> {
        Some(format!(
            "Global\\Microsoft.Sqlcmd.DeviceIdFirstRun.{}",
            user_sid_string()?
        ))
    }

    /// A named mutex that serializes first runs of sqlcmd for this user across
    /// all their sessions. A first run that cannot take it, because of an error
    /// or the wait timing out, does not create the ID: it would race with the
    /// lock's owner, so the event goes without the property instead. (Another
    /// user could keep a first run from creating the ID by creating this
    /// SID-qualified name first; that only omits the property.) The mutex
    /// belongs to the thread that waited on it, so the guard is `!Send`, like
    /// `std::sync::MutexGuard`.
    struct FirstRunLock(OwnedHandle, std::marker::PhantomData<*const ()>);

    impl FirstRunLock {
        fn acquire() -> Option<Self> {
            Self::acquire_with(500)
        }

        fn acquire_with(wait_ms: u32) -> Option<Self> {
            let name = wide(&first_run_lock_name()?);
            // SAFETY: `name` is NUL-terminated; null security attributes.
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            if handle.is_null() {
                return None;
            }
            // SAFETY: `handle` is a new, open mutex handle that nothing else owns.
            let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
            // SAFETY: the handle is open for the duration of the call.
            let waited = unsafe { WaitForSingleObject(handle.as_raw_handle(), wait_ms) };
            // Built only on success: a failed wait just closes the handle, and
            // never runs the release below for a mutex this thread does not own.
            if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
                Some(Self(handle, std::marker::PhantomData))
            } else {
                None
            }
        }
    }

    impl Drop for FirstRunLock {
        fn drop(&mut self) {
            // SAFETY: the mutex is owned by this thread; the handle closes after.
            unsafe { ReleaseMutex(self.0.as_raw_handle()) };
        }
    }

    /// An open registry key, closed when it goes out of scope.
    struct OpenKey(HKEY);

    impl OpenKey {
        /// The key for reading, or `None` when it does not exist.
        fn open_read() -> io::Result<Option<Self>> {
            let path = key_path();
            let mut key: HKEY = std::ptr::null_mut();
            // SAFETY: `path` is NUL-terminated and `key` is a valid out pointer.
            let status = unsafe {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    path.as_ptr(),
                    0,
                    KEY_READ | KEY_WOW64_64KEY,
                    &mut key,
                )
            };
            if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
                return Ok(None);
            }
            if status != ERROR_SUCCESS {
                return Err(os_error(status));
            }
            Ok(Some(Self(key)))
        }

        /// The key for writing, created if it does not exist.
        fn create() -> io::Result<Self> {
            let path = key_path();
            let mut key: HKEY = std::ptr::null_mut();
            // SAFETY: `path` is NUL-terminated and `key` is a valid out pointer.
            let status = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    path.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_WRITE | KEY_READ | KEY_WOW64_64KEY,
                    std::ptr::null(),
                    &mut key,
                    std::ptr::null_mut(),
                )
            };
            if status != ERROR_SUCCESS {
                return Err(os_error(status));
            }
            Ok(Self(key))
        }
    }

    impl Drop for OpenKey {
        fn drop(&mut self) {
            // SAFETY: the key was opened by `RegOpenKeyExW` or `RegCreateKeyExW`
            // and is closed once, here.
            unsafe { RegCloseKey(self.0) };
        }
    }

    impl Store for RegistryStore {
        fn read(&self) -> io::Result<Option<String>> {
            let Some(key) = OpenKey::open_read()? else {
                return Ok(None);
            };
            let key = key.0;
            let name = wide("deviceid");
            let mut kind = 0u32;
            let mut size = 0u32;
            // SAFETY: `key` is open; a null data pointer asks for the size.
            let status = unsafe {
                RegQueryValueExW(
                    key,
                    name.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    std::ptr::null_mut(),
                    &mut size,
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if status != ERROR_SUCCESS {
                return Err(os_error(status));
            }
            if kind != REG_SZ || size == 0 || size > 4096 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected value",
                ));
            }
            let mut data = vec![0u16; (size as usize).div_ceil(2)];
            let mut capacity = (data.len() * 2) as u32;
            // SAFETY: `data` holds `capacity` bytes.
            let status = unsafe {
                RegQueryValueExW(
                    key,
                    name.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    data.as_mut_ptr().cast(),
                    &mut capacity,
                )
            };
            if status != ERROR_SUCCESS {
                return Err(os_error(status));
            }
            let units = (capacity as usize / 2).min(data.len());
            let end = data[..units]
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(units);
            Ok(Some(String::from_utf16_lossy(&data[..end])))
        }

        /// Under the first-run lock: reuse a value that is there, else store `id`,
        /// and report what is stored. If the lock cannot be taken this is an
        /// error, not an unlocked write. A tool that stores the shared ID without
        /// the lock can still overwrite between the write and the read-back; the
        /// read-back narrows that to the one value that survives.
        fn create_if_absent(&self, id: &str) -> io::Result<String> {
            let _lock = FirstRunLock::acquire().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the device ID first-run lock could not be taken",
                )
            })?;
            if let Some(existing) = self.read()? {
                return Ok(existing);
            }
            self.set_value(id)?;
            Ok(self.read()?.unwrap_or_else(|| id.to_owned()))
        }
    }

    impl RegistryStore {
        fn set_value(&self, id: &str) -> io::Result<()> {
            let key = OpenKey::create()?;
            let name = wide("deviceid");
            let value = wide(id);
            // SAFETY: `value` is NUL-terminated and its byte length is passed.
            let status = unsafe {
                RegSetValueExW(
                    key.0,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    value.as_ptr().cast(),
                    (value.len() * 2) as u32,
                )
            };
            if status != ERROR_SUCCESS {
                return Err(os_error(status));
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod lock_tests {
        use super::*;

        #[test]
        fn the_lock_name_spans_sessions_and_is_per_user() {
            let name = first_run_lock_name().expect("the process token has a user");
            assert!(name.starts_with("Global\\Microsoft.Sqlcmd.DeviceIdFirstRun.S-1-"));
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'\\' | b'.' | b'-'))
            );
        }

        #[test]
        fn a_second_first_run_cannot_take_the_lock_while_it_is_held() {
            let first = FirstRunLock::acquire_with(1000).expect("the lock is free");
            // The mutex is owned by the thread that took it, so another thread
            // stands in for another process.
            let waited = std::thread::spawn(|| FirstRunLock::acquire_with(150).is_some())
                .join()
                .expect("thread");
            assert!(!waited, "taken while held");
            drop(first);
            let again = std::thread::spawn(|| FirstRunLock::acquire_with(1000).is_some())
                .join()
                .expect("thread");
            assert!(again, "free again after release");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

    #[derive(Default)]
    struct Memory {
        value: RefCell<Option<String>>,
        fail_read: bool,
        fail_write: bool,
        writes: RefCell<u32>,
        /// Another process stores this just before ours would.
        racing_winner: RefCell<Option<String>>,
    }

    impl Store for Memory {
        fn read(&self) -> io::Result<Option<String>> {
            if self.fail_read {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
            }
            Ok(self.value.borrow().clone())
        }

        fn create_if_absent(&self, id: &str) -> io::Result<String> {
            if self.fail_write {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
            }
            if let Some(winner) = self.racing_winner.borrow_mut().take() {
                *self.value.borrow_mut() = Some(winner);
            }
            let mut value = self.value.borrow_mut();
            if let Some(existing) = value.as_ref() {
                return Ok(existing.clone());
            }
            *self.writes.borrow_mut() += 1;
            *value = Some(id.to_string());
            Ok(id.to_string())
        }
    }

    #[test]
    fn a_first_run_that_loses_a_race_reports_the_id_that_survives() {
        let store = Memory::default();
        *store.racing_winner.borrow_mut() = Some(ID.to_string());
        let other = "11111111-2222-4333-8444-555555555555";
        let id = get_or_create_in(&store, || other.to_string());
        assert_eq!(id.as_deref(), Some(ID));
        assert_eq!(store.value.borrow().as_deref(), Some(ID));
        assert_eq!(*store.writes.borrow(), 0);
    }

    #[test]
    fn a_racing_winner_that_is_not_an_id_means_no_id() {
        let store = Memory::default();
        *store.racing_winner.borrow_mut() = Some("garbage".to_string());
        assert_eq!(get_or_create_in(&store, || ID.to_string()), None);
        assert_eq!(store.value.borrow().as_deref(), Some("garbage"));
    }

    #[test]
    fn concurrent_first_runs_on_files_all_report_one_id() {
        let dir = std::env::temp_dir().join(format!("sqlcmd-deviceid-race-{}", std::process::id()));
        let path = dir
            .join("Microsoft")
            .join("DeveloperTools")
            .join("deviceid");
        let ids: Vec<String> = (0..16)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let store = FileStore { path };
                    get_or_create_in(&store, || uuid::Uuid::new_v4().hyphenated().to_string())
                        .expect("an id")
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect();
        assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
        let stored = std::fs::read_to_string(&path).expect("stored id");
        assert_eq!(normalize(&stored).as_deref(), Some(ids[0].as_str()));
        let leftovers = std::fs::read_dir(path.parent().expect("parent"))
            .expect("dir")
            .filter(|entry| {
                entry
                    .as_ref()
                    .is_ok_and(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            })
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_id_is_generated_and_stored() {
        let store = Memory::default();
        let id = get_or_create_in(&store, || ID.to_string());
        assert_eq!(id.as_deref(), Some(ID));
        assert_eq!(store.value.borrow().as_deref(), Some(ID));
    }

    #[test]
    fn the_shared_id_is_reused_not_replaced() {
        let store = Memory::default();
        *store.value.borrow_mut() = Some(ID.to_string());
        let id = get_or_create_in(&store, || panic!("must not generate"));
        assert_eq!(id.as_deref(), Some(ID));
        assert_eq!(*store.writes.borrow(), 0);
    }

    #[test]
    fn an_uppercase_or_bom_prefixed_id_is_normalized_but_not_rewritten() {
        let store = Memory::default();
        *store.value.borrow_mut() = Some(format!("\u{feff}{}\r\n", ID.to_ascii_uppercase()));
        let id = get_or_create_in(&store, || panic!("must not generate"));
        assert_eq!(id.as_deref(), Some(ID));
        assert_eq!(*store.writes.borrow(), 0);
    }

    #[test]
    fn an_unrecognized_stored_value_is_left_alone_and_there_is_no_id() {
        let store = Memory::default();
        *store.value.borrow_mut() = Some("not-a-uuid".to_string());
        let id = get_or_create_in(&store, || panic!("must not generate"));
        assert_eq!(id, None);
        assert_eq!(store.value.borrow().as_deref(), Some("not-a-uuid"));
    }

    #[test]
    fn a_read_failure_means_no_id_and_nothing_is_overwritten() {
        let store = Memory {
            fail_read: true,
            ..Memory::default()
        };
        assert_eq!(get_or_create_in(&store, || ID.to_string()), None);
        assert_eq!(*store.writes.borrow(), 0);
    }

    #[test]
    fn a_write_failure_means_no_id() {
        let store = Memory {
            fail_write: true,
            ..Memory::default()
        };
        assert_eq!(get_or_create_in(&store, || ID.to_string()), None);
    }

    #[test]
    fn only_lowercase_8_4_4_4_12_uuids_come_out() {
        assert_eq!(normalize(ID).as_deref(), Some(ID));
        for bad in [
            "",
            "0f8fad5bd9cb469fa16570867728950e",
            "0f8fad5b-d9cb-469f-a165-70867728950",
            "0f8fad5b-d9cb-469f-a165-70867728950g",
            "{0f8fad5b-d9cb-469f-a165-70867728950e}",
        ] {
            assert_eq!(normalize(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_generated_id_is_a_version_4_uuid_in_the_right_shape() {
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        assert_eq!(normalize(&id).as_deref(), Some(id.as_str()));
        assert_eq!(id.as_bytes()[14], b'4');
    }

    #[test]
    fn xdg_cache_home_alone_is_enough_on_linux() {
        use std::ffi::OsString;
        let abs = std::env::temp_dir().join("xdg-cache");
        let expected = abs
            .join("Microsoft")
            .join("DeveloperTools")
            .join("deviceid");
        // No HOME at all, or an empty one: a service or container that sets only
        // XDG_CACHE_HOME still has its ID.
        for home in [None, Some(OsString::new())] {
            assert_eq!(
                file_store_path(home, Some(abs.clone().into_os_string()), false),
                Some(expected.clone())
            );
        }
    }

    #[test]
    fn home_is_the_linux_fallback_and_the_macos_base() {
        use std::ffi::OsString;
        let home = std::env::temp_dir().join("a-home");
        let tail = std::path::Path::new("Microsoft")
            .join("DeveloperTools")
            .join("deviceid");
        // No XDG_CACHE_HOME, an empty one, or a relative one (the XDG
        // specification says to ignore it): ~/.cache.
        for xdg in [
            None,
            Some(OsString::new()),
            Some(OsString::from("relative/cache")),
        ] {
            assert_eq!(
                file_store_path(Some(home.clone().into_os_string()), xdg, false),
                Some(home.join(".cache").join(&tail))
            );
        }
        // macOS ignores XDG_CACHE_HOME.
        assert_eq!(
            file_store_path(
                Some(home.clone().into_os_string()),
                Some(OsString::from("/ignored")),
                true
            ),
            Some(home.join("Library").join("Application Support").join(&tail))
        );
    }

    #[test]
    fn without_a_usable_base_there_is_no_store() {
        use std::ffi::OsString;
        assert_eq!(file_store_path(None, None, false), None);
        assert_eq!(file_store_path(Some(OsString::new()), None, false), None);
        assert_eq!(
            file_store_path(
                Some(OsString::new()),
                Some(OsString::from("relative")),
                false
            ),
            None
        );
        assert_eq!(
            file_store_path(None, Some(OsString::from("/abs")), true),
            None
        );
    }

    #[test]
    fn the_file_store_creates_directories_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("sqlcmd-deviceid-test-{}", std::process::id()));
        let store = FileStore {
            path: dir
                .join("Microsoft")
                .join("DeveloperTools")
                .join("deviceid"),
        };
        assert_eq!(store.read().expect("missing file is not an error"), None);
        let id = get_or_create_in(&store, || ID.to_string());
        assert_eq!(id.as_deref(), Some(ID));
        assert_eq!(
            get_or_create_in(&store, || panic!("reuse")).as_deref(),
            Some(ID)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Only a small regular file is read: a huge file, a directory or a
    /// non-text file at the path is an error, and the ID is not replaced.
    #[test]
    fn the_file_store_reads_only_a_small_regular_text_file() {
        let dir = std::env::temp_dir().join(format!("sqlcmd-deviceid-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("deviceid");
        let store = FileStore { path: path.clone() };

        std::fs::write(&path, format!("\u{feff}{ID}\r\n")).expect("write");
        assert!(store.read().expect("a normal file").is_some());

        std::fs::write(&path, vec![b'a'; 1024 * 1024]).expect("write");
        assert!(store.read().is_err(), "a large file is not read");

        std::fs::write(&path, [0xff, 0xfe, 0xfd]).expect("write");
        assert!(store.read().is_err(), "not text");

        std::fs::remove_file(&path).expect("remove");
        std::fs::create_dir(&path).expect("a directory at the path");
        assert!(store.read().is_err(), "a directory is not an ID");
        assert_eq!(get_or_create_in(&store, || ID.to_string()), None);
        assert!(path.is_dir(), "nothing replaced it");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The staged file goes whenever the guard does, which is what a failed
    /// write (not reproducible here) relies on.
    #[test]
    fn a_staged_file_is_removed_when_its_guard_drops() {
        let path =
            std::env::temp_dir().join(format!("sqlcmd-deviceid-guard-{}", std::process::id()));
        std::fs::write(&path, ID).expect("write");
        drop(StagedFile(path.clone()));
        assert!(!path.exists());
    }

    /// A create that fails does not leave its staged file in the ID directory:
    /// here it fails at the link.
    #[test]
    fn a_failed_create_leaves_no_staged_file() {
        let dir =
            std::env::temp_dir().join(format!("sqlcmd-deviceid-stage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        // The ID path is a directory, so linking the staged file there fails.
        let path = dir.join("deviceid");
        std::fs::create_dir(&path).expect("directory at the ID path");
        let store = FileStore { path };
        assert!(store.create_if_absent(ID).is_err());
        let staged = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(staged, 0, "no staged file is left");
        let _ = std::fs::remove_dir_all(dir);
    }
}
