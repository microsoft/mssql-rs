// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// Entry point for e2e tests that skip *wholesale* when the driver path they
// need is not configured — currently `dll_unload_stress_test` and
// `trace_rotation_test`, both of which load the driver directly with
// LoadLibrary and therefore require `MSSQL_ODBC_DLL`.
//
// Why this exists: `GTEST_SKIP()` marks a case skipped but the binary still
// exits 0. ctest records one result per *binary*, so without a distinct exit
// code it writes `status='run'` and never a `<skipped>` element. The parity
// report is built from that JUnit, so a binary that ran on only one leg scored
// PASS/PASS — reading as mutual confirmation when nothing was compared at all.
//
// Returning ctest's conventional skip code (77, matched by SKIP_RETURN_CODE in
// CMakeLists.txt) makes the harness say "skipped (not compared)" instead.
// Only a run in which *every* selected case skipped counts: a partial skip
// still reflects real coverage and must not be reported as no coverage.

#include <gtest/gtest.h>

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);

    const int rc = RUN_ALL_TESTS();
    if (rc != 0) {
        return rc;
    }

    const ::testing::UnitTest& unit = *::testing::UnitTest::GetInstance();
    const int selected = unit.test_to_run_count();
    if (selected > 0 && unit.skipped_test_count() == selected) {
        return 77;
    }
    return 0;
}
