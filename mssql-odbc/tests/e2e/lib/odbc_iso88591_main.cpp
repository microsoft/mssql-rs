// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include <gtest/gtest.h>

#include <clocale>
#include <initializer_list>

int main(int argc, char** argv) {
    std::setlocale(LC_CTYPE, "");
    for (const char* locale : {
             "en_US.ISO-8859-1",
             "en_US.ISO8859-1",
             "en_US.iso88591",
             "C.ISO-8859-1",
             "ISO-8859-1",
         }) {
        if (std::setlocale(LC_CTYPE, locale) != nullptr) {
            break;
        }
    }

    ::testing::InitGoogleTest(&argc, argv);
    const int rc = RUN_ALL_TESTS();
    if (rc != 0) {
        return rc;
    }

    const ::testing::UnitTest& unit = *::testing::UnitTest::GetInstance();
    const int selected = unit.test_to_run_count();
    return selected > 0 && unit.skipped_test_count() == selected ? 77 : 0;
}
