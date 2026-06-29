#!/usr/bin/env python3
"""Duplicate PR validation guard.

Queries the Azure DevOps Build REST API for a prior completed-and-succeeded
validation run on the same pipeline and PR head commit. When one is found, emits
``skipDuplicate=true`` so downstream PR stages can short-circuit redundant work.

Any missing prerequisite (access token, source commit) or API error falls back
to ``skipDuplicate=false`` so full validation proceeds (safe default).

Consumed by the ``EvaluateDuplicate`` stage in
``.pipeline/templates/validation-stages.yml``.
"""
import json
import os
import urllib.parse
import urllib.request


def set_skip_duplicate(value):
    print(f"##vso[task.setvariable variable=skipDuplicate;isOutput=true]{value}")


def main():
    token = os.environ.get("SYSTEM_ACCESSTOKEN", "")
    if not token:
        print(
            "##vso[task.logissue type=warning]System.AccessToken is unavailable; continuing with full validation."
        )
        set_skip_duplicate("false")
        return

    collection_uri = os.environ["SYSTEM_COLLECTIONURI"]
    team_project = os.environ["SYSTEM_TEAMPROJECT"]
    definition_id = os.environ["SYSTEM_DEFINITIONID"]
    current_build_id = int(os.environ["BUILD_BUILDID"])
    build_source_branch = os.environ.get("BUILD_SOURCEBRANCH", "")
    source_commit = os.environ.get("SYSTEM_PULLREQUEST_SOURCECOMMITID", "").lower()
    pull_request_id = os.environ.get("SYSTEM_PULLREQUEST_PULLREQUESTID", "")
    pull_request_number = os.environ.get("SYSTEM_PULLREQUEST_PULLREQUESTNUMBER", "")
    print(
        "Duplicate guard context: "
        f"buildId={current_build_id}, "
        f"branch={build_source_branch or '<empty>'}, "
        f"sourceCommit={source_commit or '<empty>'}, "
        f"pullRequestId={pull_request_id or '<empty>'}, "
        f"pullRequestNumber={pull_request_number or '<empty>'}"
    )
    current_pr_identifiers = {
        value for value in (pull_request_id, pull_request_number) if value
    }
    if not source_commit:
        print(
            "##vso[task.logissue type=warning]System.PullRequest.SourceCommitId is unavailable; continuing with full validation."
        )
        set_skip_duplicate("false")
        return
    # 200 keeps API payloads small while covering repeated draft/ready transitions on busy PRs.
    # If a PR exceeds this window, the guard falls back to full validation (safe false negative).
    max_candidates = "200"

    query_params = {
        "definitions": definition_id,
        "reasonFilter": "pullRequest",
        "statusFilter": "completed",
        "resultFilter": "succeeded",
        "queryOrder": "finishTimeDescending",
        "$top": max_candidates,
        "api-version": "7.1",
    }
    if build_source_branch:
        # PR validation runs are indexed by the synthetic PR branch (for example refs/pull/<id>/merge),
        # not by System.PullRequest.SourceBranch (refs/heads/<branch>).
        query_params["branchName"] = build_source_branch
    query = urllib.parse.urlencode(query_params)
    url = f"{collection_uri}{team_project}/_apis/build/builds?{query}"
    headers = {"Authorization": "Bearer " + token}
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request) as response:
            payload = json.load(response)
    except Exception as exc:
        print(
            f"##vso[task.logissue type=warning]Failed to query prior PR validation runs ({type(exc).__name__}: {exc}). Continuing with full validation."
        )
        set_skip_duplicate("false")
        return

    matching_run = None
    candidates = payload.get("value", [])
    print(f"Duplicate guard candidates fetched: {len(candidates)}")
    skipped_for_pr_mismatch = 0
    skipped_for_missing_sha = 0
    for build in candidates:
        build_id = int(build.get("id", 0))
        if build_id == current_build_id:
            continue

        trigger_info = build.get("triggerInfo", {})
        # Field names vary across providers/API revisions; prefer provider-specific canonical keys first.
        # - pr.sourceSha: Azure Repos PR triggers
        # - pr.sourceCommitId: GitHub-backed PR triggers
        # - pr.headSha: alternate/legacy PR trigger payloads
        prior_source_sha = (
            trigger_info.get("pr.sourceSha")
            or trigger_info.get("pr.sourceCommitId")
            or trigger_info.get("pr.headSha")
        )
        if prior_source_sha:
            prior_source_sha = prior_source_sha.lower()
        if not prior_source_sha:
            skipped_for_missing_sha += 1
            continue
        prior_pr_identifiers = []
        for key in ("pr.number", "pr.pullRequestId"):
            value = trigger_info.get(key)
            if value is not None:
                prior_pr_identifiers.append(str(value))
        # Without branch scope in the REST query, PR identifier mismatch is the only safe discriminator
        # to avoid treating runs from other PRs as duplicates.
        if (
            current_pr_identifiers
            and prior_pr_identifiers
            and set(prior_pr_identifiers).isdisjoint(current_pr_identifiers)
        ):
            if not build_source_branch:
                skipped_for_pr_mismatch += 1
                continue
            print(
                f"Candidate run {build_id} has trigger PR identifiers {prior_pr_identifiers}, "
                f"which do not overlap with current PR identifiers {sorted(current_pr_identifiers)}; "
                "continuing with commit comparison because the API query is already branch-scoped."
            )

        if prior_source_sha == source_commit:
            matching_run = build
            break

    if matching_run:
        matching_run_id = matching_run.get("id", "unknown")
        print(
            f"PR head {source_commit} already validated by run {matching_run_id}; skipping duplicate stages."
        )
        set_skip_duplicate("true")
    else:
        print(
            "Duplicate guard summary: "
            f"skipped_for_pr_mismatch={skipped_for_pr_mismatch}, "
            f"skipped_for_missing_source_sha={skipped_for_missing_sha}"
        )
        print(f"No successful prior validation run found for PR head {source_commit}; continuing.")
        set_skip_duplicate("false")


if __name__ == "__main__":
    main()
