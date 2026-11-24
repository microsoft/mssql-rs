# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Run the live Python lifecycle cases required on Windows and macOS CI."""

import pytest


TEST_FILES = (
    "test_async_platform_lifecycle.py",
    "test_async_connection.py",
    "test_async_cursor.py",
)

TEST_NAMES = (
    "test_cancelled_nextset_during_server_wait_resynchronizes_connection",
    "test_cancelled_close_waiter_during_server_wait_finishes_close",
    "test_close_drains_results_and_preserves_transaction_intent",
    "test_clean_context_exit_commits_after_last_cursor_is_dropped",
    "test_delayed_drain_error_prevents_commit_and_preserves_block_exception",
    "test_transaction_operation_drains_pending_cursor_results",
)


if __name__ == "__main__":
    raise SystemExit(
        pytest.main(
            [
                *(f"tests/{file}" for file in TEST_FILES),
                "-m",
                "integration",
                "-k",
                " or ".join(TEST_NAMES),
                "-q",
                "--tb=short",
                "--junitxml=pytest-platform-lifecycle-results.xml",
            ]
        )
    )
