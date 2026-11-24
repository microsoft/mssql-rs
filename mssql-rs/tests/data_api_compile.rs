// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

#[test]
fn tiberius_data_api_compatibility_fixtures_compile() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/pass/data_api_*.rs");
}
