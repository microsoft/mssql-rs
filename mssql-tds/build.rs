// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::env;
use std::error::Error;
use std::io;
use std::process::Command;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    if env::var("CARGO_CFG_TARGET_OS")? != "macos"
        || env::var_os("CARGO_FEATURE_TLS_NETWORK_FRAMEWORK").is_none()
    {
        return Ok(());
    }

    let deployment_target = match env::var("MACOSX_DEPLOYMENT_TARGET") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => {
            let output = Command::new(env::var_os("RUSTC").ok_or("RUSTC is not set")?)
                .args([
                    "--print",
                    "deployment-target",
                    "--target",
                    &env::var("TARGET")?,
                ])
                .output()?;
            if !output.status.success() {
                return Err(format!(
                    "cannot determine the macOS deployment target: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
                .into());
            }
            String::from_utf8(output.stdout)?
                .trim()
                .strip_prefix("MACOSX_DEPLOYMENT_TARGET=")
                .ok_or("rustc returned an unexpected deployment target")?
                .to_string()
        }
        Err(error) => return Err(error.into()),
    };

    let mut version = [0_u32; 3];
    let parts: Vec<_> = deployment_target.split('.').collect();
    if parts.is_empty() || parts.len() > version.len() {
        return Err(invalid_target(&deployment_target).into());
    }
    for (part, component) in parts.iter().zip(&mut version) {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid_target(&deployment_target).into());
        }
        *component = part.parse()?;
    }
    if version < [10, 15, 0] {
        return Err(format!(
            "tls-network-framework requires macOS 10.15 or later, but the deployment target \
             is {deployment_target}. Set MACOSX_DEPLOYMENT_TARGET=10.15 (or later) for the \
             entire build, or disable tls-network-framework with default-features = false."
        )
        .into());
    }
    Ok(())
}

fn invalid_target(value: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid MACOSX_DEPLOYMENT_TARGET: {value:?}"),
    )
}
