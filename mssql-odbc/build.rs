// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Single source of truth for the shipped driver filename: the SONAME, the
    // macOS install name, and `driver_name()` (via `env!`) all read this.
    let artifact = match target_os.as_str() {
        "windows" => "mssqlodbc.dll",
        "macos" => "mssqlodbc.dylib",
        _ => "mssqlodbc.so",
    };
    println!("cargo:rustc-env=MSSQL_ODBC_ARTIFACT={artifact}");

    match target_os.as_str() {
        "linux" => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,{artifact}");
        }
        "macos" => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,{artifact}");
        }
        _ => {}
    }
}
