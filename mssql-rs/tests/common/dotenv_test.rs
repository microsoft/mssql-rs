// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::env;
use std::path::PathBuf;
use std::process::Command;

pub fn in_child() -> bool {
    env::var_os("MSSQL_RS_DOTENV_TEST_CHILD").is_some()
}

pub fn run(test_name: &str, contents: &str, overrides: &[(&str, &str)]) {
    let dir = env::temp_dir().join(format!("mssql-rs-dotenv-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).expect("create isolated dotenv directory");
    let dir = TestDirectory(dir);
    std::fs::write(dir.0.join(".env"), contents).expect("write isolated dotenv file");

    let mut command = Command::new(env::current_exe().expect("locate test executable"));
    command
        .current_dir(&dir.0)
        .args(["--exact", test_name, "--nocapture"])
        .env("MSSQL_RS_DOTENV_TEST_CHILD", "1");
    for name in [
        "ACCESS_TOKEN",
        "SQL_PASSWORD",
        "DB_HOST",
        "DB_PORT",
        "DB_DATABASE",
        "DB_USERNAME",
        "TRUST_SERVER_CERTIFICATE",
        "CERT_HOST_NAME",
    ] {
        command.env_remove(name);
    }
    for (name, value) in overrides {
        command.env(name, value);
    }
    let output = command.output().expect("run isolated dotenv test");
    assert!(
        output.status.success(),
        "isolated test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct TestDirectory(PathBuf);

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(self.0.join(".env")) {
            eprintln!("could not remove test .env: {error}");
        }
        if let Err(error) = std::fs::remove_dir(&self.0) {
            eprintln!("could not remove test directory: {error}");
        }
    }
}
