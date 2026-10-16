# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Follow the PR's latest ADO build until its coverage artifact is available."""

import argparse
from collections.abc import Iterator
import json
import os
from pathlib import Path
import random
import re
import subprocess
import time
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode


BUILD_API = "https://dev.azure.com/sqlclientdrivers/public/_apis/build"


class PollingDeadlineExceeded(TimeoutError):
    pass


class CoverageBuildChanged(Exception):
    pass


def remaining_time(deadline: float) -> float:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise PollingDeadlineExceeded("Coverage polling deadline exceeded")
    return remaining


def get_json(url: str, *, deadline: float, headers: dict | None = None) -> dict:
    timeout = min(60, remaining_time(deadline))
    for name, value in (headers or {}).items():
        if any(char in name or char in value for char in "\r\n"):
            raise ValueError("Invalid HTTP header")
    try:
        # curl bounds the whole transfer; subprocess also bounds startup and DNS.
        # Headers travel over stdin, keeping the job token out of process arguments.
        response = subprocess.run(
            [
                "curl", "--disable", "--silent", "--show-error", "--fail",
                "--proto", "=http,https", "--max-time", str(timeout),
                "--write-out", "\n%{http_code}", "--header", "@-", "--url", url,
            ],
            input="".join(f"{name}: {value}\n" for name, value in (headers or {}).items()),
            capture_output=True, text=True, encoding="utf-8", timeout=timeout,
        )
    except subprocess.TimeoutExpired as error:
        remaining_time(deadline)
        raise TimeoutError("Coverage API request timed out") from error
    remaining_time(deadline)
    if response.returncode == 28:
        raise TimeoutError(response.stderr.strip())
    if response.returncode not in (0, 22):
        raise URLError(f"curl failed ({response.returncode}): {response.stderr.strip()}")
    body, separator, status = response.stdout.rpartition("\n")
    if not separator or not re.fullmatch(r"[1-5][0-9]{2}", status):
        raise ValueError("Missing HTTP status in coverage API response")
    if response.returncode == 22 or not 200 <= int(status) < 300:
        raise HTTPError(url, int(status), response.stderr.strip(), {}, None)
    return json.loads(body)


def validate_sha(value: object) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", value):
        raise ValueError("Invalid PR source SHA")
    return value.lower()


def github_head(pr_number: str, *, deadline: float) -> str:
    pr = get_json(
        f"https://api.github.com/repos/microsoft/mssql-rs/pulls/{pr_number}",
        deadline=deadline,
        headers={
            "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    return validate_sha(pr["head"]["sha"])


def builds_for_pr(pr_number: str, *, deadline: float, successful: bool = False) -> Iterator[dict]:
    parameters = {
        "definitions": "2267",
        "branchName": f"refs/pull/{pr_number}/merge",
        "queryOrder": "queueTimeDescending",
        "$top": "10",
        "api-version": "7.1",
    }
    if successful:
        parameters.update({
            "statusFilter": "completed",
            "resultFilter": "succeeded",
            "queryOrder": "finishTimeDescending",
            "$top": "200",
        })
    query = urlencode(parameters)
    builds = get_json(f"{BUILD_API}/builds?{query}", deadline=deadline)["value"]
    for build in builds:
        remaining_time(deadline)
        trigger = build.get("triggerInfo", {})
        if str(trigger.get("pr.number")) != pr_number:
            continue
        if type(build["id"]) is not int or build["id"] <= 0:
            raise ValueError("Invalid Azure DevOps build ID")
        trigger["pr.sourceSha"] = validate_sha(
            trigger.get("pr.sourceSha")
            or trigger.get("pr.sourceCommitId")
            or trigger.get("pr.headSha")
        )
        yield build


def latest_build(pr_number: str, *, deadline: float) -> dict | None:
    return next(builds_for_pr(pr_number, deadline=deadline), None)


def canceled(build: dict) -> bool:
    return build["status"] == "cancelling" or build.get("result") == "canceled"


def has_coverage(build_id: int, *, deadline: float) -> bool:
    artifacts = get_json(f"{BUILD_API}/builds/{build_id}/artifacts?api-version=7.1", deadline=deadline)["value"]
    matches = [item for item in artifacts if item["name"] == "CoberturaCoverage"]
    if len(matches) > 1:
        raise ValueError("Multiple CoberturaCoverage artifacts found")
    return bool(matches)


def duplicate_coverage(pr_number: str, build: dict, head_sha: str, *, deadline: float) -> int | None:
    if build["status"] != "completed" or build.get("result") != "succeeded":
        return None
    records = get_json(f"{BUILD_API}/builds/{build['id']}/timeline?api-version=7.1", deadline=deadline)["records"]
    stages = {record["identifier"]: record.get("result") for record in records if record["type"] == "Stage"}
    # EvaluateDuplicate succeeds only for ordinary PR validation; its true
    # output skips both Build and Coverage in validation-stages.yml.
    if not (stages.get("EvaluateDuplicate") == "succeeded"
            and stages.get("Build") == "skipped" and stages.get("Coverage") == "skipped"):
        return None
    # Match the duplicate guard's bounded history window, skipping duplicate chains.
    for prior in builds_for_pr(pr_number, deadline=deadline, successful=True):
        if (prior["id"] != build["id"] and prior["status"] == "completed"
                and prior.get("result") == "succeeded"
                and prior["triggerInfo"]["pr.sourceSha"] == head_sha):
            if github_head(pr_number, deadline=deadline) != head_sha:
                raise CoverageBuildChanged
            current = latest_build(pr_number, deadline=deadline)
            if (current is None or current["id"] != build["id"]
                    or current["triggerInfo"]["pr.sourceSha"] != head_sha
                    or canceled(current) or current.get("result") == "failed"):
                raise CoverageBuildChanged
            if has_coverage(prior["id"], deadline=deadline):
                print(f"Using coverage from build {prior['id']} for duplicate build {build['id']}...", flush=True)
                return prior["id"]
    return None


def wait_for_artifact(pr_number: str, timeout: float = 75 * 60) -> tuple[int, str]:
    if not re.fullmatch(r"[1-9][0-9]*", pr_number):
        raise ValueError("Invalid PR number")
    deadline = time.monotonic() + timeout
    retry_delay = 10
    while time.monotonic() < deadline:
        delay = 10
        try:
            head_sha = github_head(pr_number, deadline=deadline)
            build = latest_build(pr_number, deadline=deadline)
            if build is None:
                print(f"Waiting for an ADO build for PR #{pr_number}...", flush=True)
            elif build["triggerInfo"]["pr.sourceSha"] != head_sha:
                print(f"Waiting for an ADO build for PR #{pr_number} head {head_sha}...", flush=True)
            elif canceled(build):
                print(f"Build {build['id']} was canceled; waiting for its replacement...", flush=True)
            else:
                build_id = build["id"]
                if build.get("result") == "failed":
                    raise RuntimeError(f"Azure DevOps build {build_id} failed; cannot report coverage")
                print(f"Checking coverage artifact for build {build_id} ({build['status']})...", flush=True)
                coverage_id = (
                    build_id if has_coverage(build_id, deadline=deadline)
                    else duplicate_coverage(pr_number, build, head_sha, deadline=deadline)
                )
                if coverage_id is not None:
                    # A push can precede its ADO build, including during this request.
                    current = latest_build(pr_number, deadline=deadline)
                    if (current is not None and current["id"] == build_id
                            and current["triggerInfo"]["pr.sourceSha"] == head_sha
                            and not canceled(current) and current.get("result") != "failed"
                            and github_head(pr_number, deadline=deadline) == head_sha):
                        return coverage_id, head_sha
                    retry_delay = 10
                    continue
                delay = random.randint(300, 600)
        except CoverageBuildChanged:
            print("PR head or latest build changed during history lookup; refreshing the poll.", flush=True)
            retry_delay = 10
            continue
        except PollingDeadlineExceeded:
            raise
        except (HTTPError, TimeoutError) as error:
            if isinstance(error, HTTPError) and error.code not in (408, 429, 500, 502, 503, 504):
                raise
            print(f"Temporary coverage API failure: {error}; retrying within the polling deadline.", flush=True)
            delay = retry_delay
            retry_delay = min(retry_delay * 2, 300)
        else:
            retry_delay = 10
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
