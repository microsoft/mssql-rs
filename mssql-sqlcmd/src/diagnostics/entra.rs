// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Microsoft Entra authentication (`-G`) in `sqlcmd diagnose`.
//!
//! An access token that sqlcmd reads from a file (`-G -P <file>`, without
//! `-U`) is sent as sqlcmd sends it. sqlcmd diagnose does not acquire tokens
//! yet: with `ActiveDirectoryPassword`, which needs a credential provider for
//! one, the client goes as far as the server's request for a token
//! ([`TokenRequest`]), and the login is not evaluated. No token or credential
//! is ever reported.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use mssql_tds::connection::client_context::{EntraIdTokenFactory, TdsAuthenticationMethod};
use mssql_tds::core::TdsResult;
use mssql_tds::error::Error;

/// The largest access token file sqlcmd reads (`ACCESSTOKEN_MAX`).
pub const ACCESS_TOKEN_FILE_MAX: u64 = 1_048_576;

/// An access token, as sqlcmd reads it from the file `-P` names. Its `Debug`
/// output never shows it, and it cannot be copied: the one value moves to the
/// client.
pub struct AccessToken(String);

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccessToken(..)")
    }
}

impl AccessToken {
    /// The token, to hand to the client.
    pub fn into_inner(self) -> String {
        self.0
    }
}

/// Why the access token in the file `-P` names cannot be sent as sqlcmd
/// sends it.
#[derive(Debug)]
pub enum TokenFileError {
    /// There is no file at the path.
    NotFound(std::io::Error),
    /// The file could not be read.
    Unreadable(std::io::Error),
    /// The file is larger than sqlcmd reads ([`ACCESS_TOKEN_FILE_MAX`]).
    TooLarge(u64),
    /// The file is not UTF-16LE text (an odd number of bytes, or an unpaired
    /// surrogate), the form in which sqlcmd passes the token to the driver.
    NotUtf16,
    /// The file was not read within the time limit: a pipe no one writes to,
    /// or a share that does not answer.
    TimedOut,
}

impl TokenFileError {
    /// The error code connection input reports.
    pub fn code(&self) -> &'static str {
        match self {
            TokenFileError::NotFound(_) => "accessTokenFileNotFound",
            TokenFileError::Unreadable(_) => "accessTokenFileUnreadable",
            TokenFileError::TooLarge(_) => "accessTokenFileTooLarge",
            TokenFileError::NotUtf16 => "accessTokenFileNotUtf16",
            TokenFileError::TimedOut => "accessTokenFileTimedOut",
        }
    }
}

/// Reads the access token as sqlcmd does: the file's bytes, which sqlcmd
/// passes to the driver as they are and the driver sends as the token, so
/// they are UTF-16LE. The client sends the token it is given as UTF-16LE, so
/// it sends the same bytes.
pub fn read_access_token(path: &Path) -> Result<AccessToken, TokenFileError> {
    use std::io::Read;
    // Opened first, as sqlcmd opens it: what cannot be opened (a directory,
    // on Windows) cannot be read, whatever its size.
    let mut file = std::fs::File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => TokenFileError::NotFound(error),
        _ => TokenFileError::Unreadable(error),
    })?;
    let size = file.metadata().map_err(TokenFileError::Unreadable)?.len();
    if size > ACCESS_TOKEN_FILE_MAX {
        return Err(TokenFileError::TooLarge(size));
    }
    // At most one byte more than sqlcmd reads: a file that grew since is
    // still too large. The buffer takes the file's size at once; a pipe or a
    // device reports none.
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    file.by_ref()
        .take(ACCESS_TOKEN_FILE_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(TokenFileError::Unreadable)?;
    let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if read > ACCESS_TOKEN_FILE_MAX {
        return Err(TokenFileError::TooLarge(read));
    }
    if bytes.len() % 2 != 0 {
        return Err(TokenFileError::NotUtf16);
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    let mut token = String::with_capacity(bytes.len() / 2);
    for c in char::decode_utf16(units) {
        token.push(c.map_err(|_| TokenFileError::NotUtf16)?);
    }
    Ok(AccessToken(token))
}

/// Stands in for the credential provider of a method that needs one: it
/// records that the server asked for a token, which comes after pre-login,
/// TLS and the login request, and gets none, so the login stops there. The
/// authority and resource the server names are not looked at.
#[derive(Clone, Debug, Default)]
pub struct TokenRequest(Arc<AtomicBool>);

impl TokenRequest {
    /// Whether the server asked for a token.
    pub fn made(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// A request the server already made, for tests that replay an attempt.
    #[cfg(test)]
    pub(crate) fn already_made() -> Self {
        let request = Self::default();
        request.0.store(true, Ordering::SeqCst);
        request
    }
}

#[async_trait]
impl EntraIdTokenFactory for TokenRequest {
    async fn create_token(
        &self,
        _spn: String,
        _sts_url: String,
        _auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        self.0.store(true, Ordering::SeqCst);
        Err(Error::UsageError(
            "sqlcmd diagnose does not acquire Microsoft Entra tokens".to_string(),
        ))
    }
}

/// Token files for tests.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A new path in the temporary directory, for a file of `kind`.
    fn temp_path(kind: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "mssql-sqlcmd-{kind}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ))
    }

    /// A file in the temporary directory, removed when dropped.
    pub(crate) struct TempFile(pub(crate) PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A new temporary file holding `bytes`.
    pub(crate) fn file_with(bytes: &[u8]) -> TempFile {
        let path = temp_path("token");
        std::fs::write(&path, bytes).unwrap();
        TempFile(path)
    }

    /// An empty directory in the temporary directory, removed when dropped.
    pub(crate) struct TempDirectory(pub(crate) PathBuf);

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    /// A new empty temporary directory: a path sqlcmd cannot read a token
    /// from. Its path has no trailing separator, which Windows would report
    /// as not found.
    pub(crate) fn directory() -> TempDirectory {
        let path = temp_path("token-directory");
        std::fs::create_dir(&path).unwrap();
        TempDirectory(path)
    }

    /// A FIFO in the temporary directory that no one writes to: a reader's
    /// open waits for a writer. Dropped, it lets a reader still waiting go
    /// (to an empty read) and is removed.
    #[cfg(unix)]
    pub(crate) struct Fifo(pub(crate) PathBuf);

    #[cfg(unix)]
    impl Fifo {
        pub(crate) fn new() -> Self {
            use std::os::unix::ffi::OsStrExt;
            let path = temp_path("token-fifo");
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: `c_path` is a valid NUL-terminated path.
            let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
            assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());
            Fifo(path)
        }
    }

    #[cfg(unix)]
    impl Drop for Fifo {
        fn drop(&mut self) {
            use std::os::unix::fs::OpenOptionsExt;
            // Without blocking: with no reader waiting this just fails.
            drop(
                std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&self.0),
            );
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// `text` as UTF-16LE bytes, the form sqlcmd's token files take.
    pub(crate) fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{directory, file_with, utf16le};
    use super::*;

    #[test]
    fn a_utf16le_file_is_the_token_the_client_sends_back_as_its_bytes() {
        let file = file_with(&utf16le("eyJ0eXAi.eyJhdWQi.c2ln"));
        let token = read_access_token(&file.0).unwrap().into_inner();
        assert_eq!(token, "eyJ0eXAi.eyJhdWQi.c2ln");
        assert_eq!(utf16le(&token), std::fs::read(&file.0).unwrap());
    }

    #[test]
    fn a_token_never_shows_in_debug_output() {
        let file = file_with(&utf16le("secret-token-value"));
        let token = read_access_token(&file.0).unwrap();
        assert!(!format!("{token:?}").contains("secret"));
    }

    #[test]
    fn files_sqlcmd_cannot_send_are_told_apart() {
        let missing = std::env::temp_dir().join("mssql-sqlcmd-no-such-token-file");
        assert_eq!(
            read_access_token(&missing).unwrap_err().code(),
            "accessTokenFileNotFound"
        );
        let odd = file_with(b"abc");
        assert_eq!(
            read_access_token(&odd.0).unwrap_err().code(),
            "accessTokenFileNotUtf16"
        );
        // An unpaired surrogate is not text either.
        let surrogate = file_with(&[0x00, 0xD8, 0x41, 0x00]);
        assert_eq!(
            read_access_token(&surrogate.0).unwrap_err().code(),
            "accessTokenFileNotUtf16"
        );
        let max = usize::try_from(ACCESS_TOKEN_FILE_MAX).unwrap();
        let big = file_with(&vec![0x41; max + 2]);
        match read_access_token(&big.0) {
            Err(TokenFileError::TooLarge(size)) => assert_eq!(size, ACCESS_TOKEN_FILE_MAX + 2),
            other => panic!("{other:?}"),
        }
        // As large as sqlcmd reads is not too large.
        let largest = file_with(&vec![0x41; max]);
        assert!(read_access_token(&largest.0).is_ok());
        // A directory is not a file to read.
        let directory = directory();
        assert_eq!(
            read_access_token(&directory.0).unwrap_err().code(),
            "accessTokenFileUnreadable"
        );
    }

    /// A device or a pipe reports no size, so the size check passes it: what
    /// it gives is still read only up to one byte more than sqlcmd reads.
    #[cfg(unix)]
    #[test]
    fn a_file_with_no_size_is_still_read_only_up_to_the_limit() {
        match read_access_token(Path::new("/dev/zero")) {
            Err(TokenFileError::TooLarge(read)) => assert_eq!(read, ACCESS_TOKEN_FILE_MAX + 1),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_token_request_is_recorded_and_gets_no_token() {
        let request = TokenRequest::default();
        // The client holds its own clone of the factory.
        let registered = request.clone();
        assert!(!request.made());
        let factory: &dyn EntraIdTokenFactory = &registered;
        let result = factory
            .create_token(
                "https://database.windows.net/".to_string(),
                "https://login.microsoftonline.com/tenant/".to_string(),
                TdsAuthenticationMethod::ActiveDirectoryPassword,
            )
            .await;
        assert!(result.is_err());
        assert!(request.made());
    }
}
