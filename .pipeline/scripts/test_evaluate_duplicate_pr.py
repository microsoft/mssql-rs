# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""A pin-update commit must not reuse the preceding PR head's validation."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "evaluate_duplicate", Path(__file__).with_name("evaluate-duplicate-pr.py")
)
duplicate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(duplicate)


class DuplicateValidation(unittest.TestCase):
    def evaluate(self, prior_head, current_head):
        env = {
            "SYSTEM_ACCESSTOKEN": "test-token",
            "SYSTEM_COLLECTIONURI": "https://dev.azure.com/example/",
            "SYSTEM_TEAMPROJECT": "public",
            "SYSTEM_DEFINITIONID": "2267",
            "BUILD_BUILDID": "101",
            "BUILD_SOURCEBRANCH": "refs/pull/1/merge",
            "SYSTEM_PULLREQUEST_SOURCECOMMITID": current_head,
            "SYSTEM_PULLREQUEST_PULLREQUESTNUMBER": "1",
        }
        payload = {"value": [{
            "id": 100,
            "triggerInfo": {"pr.sourceCommitId": prior_head, "pr.number": "1"},
        }]}
        output = io.StringIO()
        with patch.dict(os.environ, env, clear=True), patch.object(
            duplicate.urllib.request, "urlopen",
            return_value=io.StringIO(json.dumps(payload)),
        ) as request, contextlib.redirect_stdout(output):
            duplicate.main()
        self.assertIn("resultFilter=succeeded", request.call_args.args[0].full_url)
        return output.getvalue()

    def test_changed_pin_commit_requires_fresh_validation(self):
        output = self.evaluate("a" * 40, "b" * 40)
        self.assertIn("skipDuplicate;isOutput=true]false", output)
        self.assertNotIn("skipDuplicate;isOutput=true]true", output)

    def test_same_head_can_reuse_successful_validation(self):
        self.assertIn(
            "skipDuplicate;isOutput=true]true",
            self.evaluate("a" * 40, "a" * 40),
        )


if __name__ == "__main__":
    unittest.main()
