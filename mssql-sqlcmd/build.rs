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
    // Ids removed from en-US stay in translated catalogs until the next
    // localization drop; they are skipped, and named here.
    for removed in &catalogs.removed_ids {
        println!("cargo:warning=localized message no longer in en-US, skipped: {removed}");
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    i18n_build::write_generated_catalogs(&catalogs.by_locale, &out_dir.join("i18n_catalog.rs"))
}
