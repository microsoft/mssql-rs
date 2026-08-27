// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    match target_os.as_str() {
        "linux" => {
            // Embed soname: libmssqlodbc.so
            println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libmssqlodbc.so");
        }
        "macos" => {
            // Embed install name: libmssqlodbc.dylib
            println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,libmssqlodbc.dylib");
        }
        _ => {}
    }
}
