// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_sqlcmd::i18n;

#[test]
fn set_locale_updates_the_shipping_locale_state() {
    // SAFETY: this integration test is a single-process check of the shipping
    // locale state. It sets the environment before the first locale() call.
    unsafe { std::env::set_var("SQLCMD_LANG", "1031") };
    assert_eq!(i18n::locale(), "de-DE");

    assert!(i18n::set_locale("1031"));
    assert_eq!(i18n::locale(), "de-DE");
    std::thread::spawn(|| assert_eq!(i18n::locale(), "de-DE"))
        .join()
        .expect("locale reader thread");

    assert!(!i18n::set_locale("not-a-locale"));
    assert_eq!(i18n::locale(), "en-US");
    std::thread::spawn(|| assert_eq!(i18n::locale(), "en-US"))
        .join()
        .expect("locale reader thread");
    // SAFETY: cleanup after this process-local integration test.
    unsafe { std::env::remove_var("SQLCMD_LANG") };
}
