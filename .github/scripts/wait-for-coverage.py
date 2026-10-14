# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Follow the PR's latest ADO build until its coverage artifact is available."""

import argparse
import json
from pathlib import Path
import random
import re
import time
from urllib.parse import urlencode
from urllib.request import urlopen


BUILD_API = "https://dev.azure.com/sqlclientdrivers/public/_apis/build"


def get_json(url: str) -> dict:
    with urlopen(url, timeout=60) as response:
        return json.load(response)


def latest_build(pr_number: str) -> dict | None:
    query = urlencode({
        "definitions": "2267",
        "branchName": f"refs/pull/{pr_number}/merge",
        "queryOrder": "queueTimeDescending",
        "$top": "10",
        "api-version": "7.1",
    })
    builds = get_json(f"{BUILD_API}/builds?{query}")["value"]
    for build in builds:
        trigger = build.get("triggerInfo", {})
        if str(trigger.get("pr.number")) != pr_number:
            continue
        if type(build["id"]) is not int or build["id"] <= 0:
            raise ValueError("Invalid Azure DevOps build ID")
        sha = trigger.get("pr.sourceSha")
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{40}", sha):
            raise ValueError("Invalid Azure DevOps PR source SHA")
        return build
    return None


def canceled(build: dict) -> bool:
    return build["status"] == "cancelling" or build.get("result") == "canceled"


def wait_for_artifact(pr_number: str, timeout: float = 75 * 60) -> tuple[int, str]:
    if not re.fullmatch(r"[1-9][0-9]*", pr_number):
        raise ValueError("Invalid PR number")
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        build = latest_build(pr_number)
        delay = 10
        if build is None:
            print(f"Waiting for an ADO build for PR #{pr_number}...", flush=True)
        elif canceled(build):
            print(f"Build {build['id']} was canceled; waiting for its replacement...", flush=True)
        else:
            build_id = build["id"]
            if build.get("result") == "failed":
                raise RuntimeError(f"Azure DevOps build {build_id} failed; cannot report coverage")
            print(f"Checking coverage artifact for build {build_id} ({build['status']})...", flush=True)
            artifacts = get_json(f"{BUILD_API}/builds/{build_id}/artifacts?api-version=7.1")["value"]
            matches = [item for item in artifacts if item["name"] == "CoberturaCoverage"]
            if len(matches) > 1:
                raise ValueError("Multiple CoberturaCoverage artifacts found")
            if matches:
                # A push can replace the build while the artifacts API is in flight.
                current = latest_build(pr_number)
                if (current is not None and current["id"] == build_id
                        and current["triggerInfo"]["pr.sourceSha"] == build["triggerInfo"]["pr.sourceSha"]
                        and not canceled(current) and current.get("result") != "failed"):
                    return build_id, build["triggerInfo"]["pr.sourceSha"]
                continue
            delay = random.randint(300, 600)
        remaining = deadline - time.monotonic()
        if remaining > 0:
            time.sleep(min(delay, remaining))
    raise TimeoutError(f"No coverage artifact for PR #{pr_number}'s latest build within {timeout:g} seconds")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pr-number", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    build_id, head_sha = wait_for_artifact(args.pr_number)
    with args.output.open("a", encoding="utf-8", newline="\n") as output:
        output.write(f"build_id={build_id}\nhead_sha={head_sha}\n")


if __name__ == "__main__":
    main()
