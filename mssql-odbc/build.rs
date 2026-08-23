// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    match target_os.as_str() {
        "linux" => {
            // Embed soname: libmsodbcsql-18.4.so.1.1
            println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libmsodbcsql-18.4.so.1.1");
        }
        "macos" => {
            // Embed install name: libmsodbcsql.18.dylib
            println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,libmsodbcsql.18.dylib");
        }
        _ => {}
    }
}
