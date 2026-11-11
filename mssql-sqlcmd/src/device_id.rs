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
    if text.len() != 36 {
        return None;
    }
    for (index, byte) in text.bytes().enumerate() {
        let ok = match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        };
        if !ok {
            return None;
        }
    }
    Some(text.to_ascii_lowercase())
}

/// The ID in a file, as on macOS and Linux.
pub struct FileStore {
    pub path: std::path::PathBuf,
}

impl Store for FileStore {
    fn read(&self) -> io::Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
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
        let staged = parent.join(format!(
            ".deviceid.{}.{}.tmp",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&staged, id)?;
        let linked = std::fs::hard_link(&staged, &self.path);
        let _ = std::fs::remove_file(&staged);
        match linked {
            Ok(()) => Ok(id.to_owned()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => self.read()?.ok_or(error),
            Err(error) => Err(error),
        }
    }
}

#[cfg(not(windows))]
fn default_store() -> Option<Box<dyn Store>> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    let home = std::path::PathBuf::from(home);
    let base = if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else {
        match std::env::var_os("XDG_CACHE_HOME").filter(|dir| !dir.is_empty()) {
            Some(dir) => std::path::PathBuf::from(dir),
            None => home.join(".cache"),
        }
    };
    Some(Box::new(FileStore {
        path: base
            .join("Microsoft")
            .join("DeveloperTools")
            .join("deviceid"),
    }))
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
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, HANDLE,
        WAIT_ABANDONED, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WOW64_64KEY, KEY_WRITE, REG_OPTION_NON_VOLATILE,
        REG_SZ, RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn key_path() -> Vec<u16> {
        wide("SOFTWARE\\Microsoft\\DeveloperTools")
    }

    fn os_error(code: u32) -> io::Error {
        io::Error::from_raw_os_error(code as i32)
    }

    /// A named mutex that serializes first runs of sqlcmd on this user's session.
    /// Taking it is best effort: if it cannot be taken the caller goes on
    /// without it, and the read-back after writing still reports what is stored.
    struct FirstRunLock(HANDLE);

    impl FirstRunLock {
        fn acquire() -> Option<Self> {
            let name = wide("Local\\Microsoft.Sqlcmd.DeviceIdFirstRun");
            // SAFETY: `name` is NUL-terminated; null security attributes.
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            if handle.is_null() {
                return None;
            }
            // SAFETY: `handle` is a mutex handle owned by this function.
            let waited = unsafe { WaitForSingleObject(handle, 2000) };
            if waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED {
                Some(Self(handle))
            } else {
                // SAFETY: `handle` was created above and is closed once.
                unsafe { CloseHandle(handle) };
                None
            }
        }
    }

    impl Drop for FirstRunLock {
        fn drop(&mut self) {
            // SAFETY: the mutex is owned by this thread and the handle is open.
            unsafe {
                ReleaseMutex(self.0);
                CloseHandle(self.0);
            }
        }
    }

    impl Store for RegistryStore {
        fn read(&self) -> io::Result<Option<String>> {
            let path = key_path();
            let name = wide("deviceid");
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
            let result = (|| {
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
            })();
            // SAFETY: `key` was opened above and is closed once.
            unsafe { RegCloseKey(key) };
            result
        }

        /// Under the first-run lock: reuse a value that is there, else store `id`,
        /// and report what is stored. A tool that stores the shared ID without the
        /// lock can still overwrite between the write and the read-back; the
        /// read-back narrows that to the one value that survives.
        fn create_if_absent(&self, id: &str) -> io::Result<String> {
            let _lock = FirstRunLock::acquire();
            if let Some(existing) = self.read()? {
                return Ok(existing);
            }
            self.set_value(id)?;
            Ok(self.read()?.unwrap_or_else(|| id.to_owned()))
        }
    }

    impl RegistryStore {
        fn set_value(&self, id: &str) -> io::Result<()> {
            let path = key_path();
            let name = wide("deviceid");
            let value = wide(id);
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
            // SAFETY: `value` is NUL-terminated and its byte length is passed.
            let status = unsafe {
                RegSetValueExW(
                    key,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    value.as_ptr().cast(),
                    (value.len() * 2) as u32,
                )
            };
            // SAFETY: `key` was opened above and is closed once.
            unsafe { RegCloseKey(key) };
            if status != ERROR_SUCCESS {
                return Err(os_error(status));
            }
            Ok(())
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
}
