// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::env;
use std::path::PathBuf;

mod i18n_build {
    include!("i18n/build_i18n.rs");
}

fn main() {
    if let Err(error) = run() {
        panic!("i18n catalog generation failed: {error}");
    }
}

fn run() -> Result<(), String> {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?);
    i18n_build::emit_rerun_if_changed(&manifest_dir)?;
    let catalogs = i18n_build::load_catalogs(&manifest_dir)?;
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    i18n_build::write_generated_catalogs(&catalogs, &out_dir.join("i18n_catalog.rs"))
}
