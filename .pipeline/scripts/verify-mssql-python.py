# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Smoke check the installed cross-repo runtime before starting SQL Server."""

from importlib import metadata
import inspect
import os
from pathlib import Path

from packaging.requirements import Requirement
from packaging.utils import canonicalize_name
import pytest


def require_provider(condition, message):
    if condition:
        return
    if os.environ.get("BUILD_REASON") in (None, "", "PullRequest"):
        pytest.fail(message)
    print(f"##vso[task.logissue type=warning]Upstream provider drift: {message}")
    pytest.skip(message)


def test_installed_runtime():
    for text in metadata.requires("mssql-python") or []:
        requirement = Requirement(text)
        if requirement.marker and not requirement.marker.evaluate({"extra": ""}):
            continue
        try:
            installed = metadata.version(requirement.name)
        except metadata.PackageNotFoundError:
            pytest.fail(f"Missing runtime dependency {requirement}; update setup requirements")
        if canonicalize_name(requirement.name) in {"mssql-python-rs", "mssql-python-odbc"}:
            print(f"{requirement.name}: {installed} (job-owned; ignoring upstream pin)")
            continue
        # requirements.txt supplies third-party dependencies. Do not implement
        # another resolver for extras, URLs, or their transitive requirements.
        assert not (requirement.extras or requirement.url), (
            f"Runtime requirement {requirement} needs explicit setup support"
        )
        assert requirement.specifier.contains(installed, prereleases=True), (
            f"Runtime dependency {requirement}: installed {installed}; update setup requirements"
        )

    # Import/loader failures and exceptions inside the query remain blocking.
    import mssql_python

    query = getattr(mssql_python, "get_native_provider_info", None)
    require_provider(callable(query), "get_native_provider_info is missing or not callable")
    try:
        inspect.signature(query).bind()
    except TypeError:
        require_provider(False, "get_native_provider_info now requires arguments")
    provider = query()
    require_provider(
        isinstance(provider, dict) and provider.get("id") == "msodbcsql18",
        f"Expected provider id msodbcsql18, got {provider!r}",
    )
    driver = provider.get("driver_path")
    require_provider(
        isinstance(driver, str) and bool(driver) and Path(driver).is_file(),
        f"Provider driver_path is missing or not a file: {provider!r}",
    )
    print(f"ODBC provider: {provider}")
    print("##vso[task.setvariable variable=mssqlPythonReady]true")
