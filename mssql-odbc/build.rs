// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    match target_os.as_str() {
        "linux" => {
            // Embed soname: mssql-odbc.so
            println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,mssql-odbc.so");
        }
        "macos" => {
            // Embed install name: mssql-odbc.dylib
            println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,mssql-odbc.dylib");
        }
        _ => {}
    }
}
