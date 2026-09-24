#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Prepare the upstream checkout without resolving over locally built wheels."""

import argparse
from importlib import metadata
import inspect
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

from packaging.requirements import Requirement
from packaging.utils import canonicalize_name

LOCAL_DISTRIBUTIONS = {"mssql-python-rs", "mssql-python-odbc"}
CONTRACT_EXIT = 10


class UpstreamContractError(RuntimeError):
    """A recognized upstream packaging or provider interface has changed."""


def select_odbc_wheel(directory):
    wheels = sorted(directory.glob("mssql_python_odbc-*.whl"))
    if len(wheels) != 1:
        raise UpstreamContractError(
            f"Expected exactly one mssql-python-odbc wheel, found {len(wheels)} "
            f"in {directory}; produced: {[path.name for path in directory.iterdir()]}"
        )
    return wheels[0]


def active_requirements(entries, extras=()):
    requirements = [Requirement(text) for text in entries or []]
    return [
        requirement for requirement in requirements
        if requirement.marker is None or any(
            requirement.marker.evaluate({"extra": extra}) for extra in ("", *extras)
        )
    ]


def check_runtime_dependencies():
    try:
        upstream = metadata.distribution("mssql-python")
    except metadata.PackageNotFoundError as error:
        raise UpstreamContractError(
            "Editable install did not provide the mssql-python distribution"
        ) from error
    active = active_requirements(upstream.requires)
    names = {canonicalize_name(requirement.name) for requirement in active}
    missing_native = LOCAL_DISTRIBUTIONS - names
    if missing_native:
        raise UpstreamContractError(
            f"Upstream runtime metadata no longer declares {sorted(missing_native)}"
        )

    problems = []
    visited = set()
    while active:
        requirement = active.pop()
        name = canonicalize_name(requirement.name)
        try:
            installed = metadata.version(name)
        except metadata.PackageNotFoundError:
            problems.append(f"{requirement}: not installed")
            continue
        if name in LOCAL_DISTRIBUTIONS:
            print(f"{name}: {installed} (locally built; ignoring upstream {requirement})")
        elif requirement.url:
            problems.append(f"{requirement}: cannot verify a direct-URL runtime dependency")
        elif not requirement.specifier.contains(installed, prereleases=True):
            problems.append(f"{requirement}: installed {installed}")
        key = (name, frozenset(requirement.extras))
        if key not in visited:
            visited.add(key)
            active.extend(active_requirements(metadata.requires(name), requirement.extras))
    if problems:
        raise RuntimeError(
            "Unsatisfied mssql-python runtime dependencies after requirements.txt setup: "
            + "; ".join(problems)
            + ". Update the setup dependencies; do not resolve over the local Rust/ODBC wheels."
        )


def verify_provider():
    try:
        import mssql_python
    except ImportError as error:
        raise UpstreamContractError(f"Cannot import the upstream native provider: {error}") from error
    get_info = getattr(mssql_python, "get_native_provider_info", None)
    if not callable(get_info):
        raise UpstreamContractError("mssql_python.get_native_provider_info is missing or not callable")
    signature = inspect.signature(get_info)
    try:
        signature.bind()
    except TypeError as error:
        raise UpstreamContractError(f"Provider query now requires arguments: {signature}") from error
    provider = get_info()
    if not isinstance(provider, dict) or provider.get("id") != "msodbcsql18":
        raise UpstreamContractError(f"Expected provider id msodbcsql18, got {provider!r}")
    driver = provider.get("driver_path")
    if not isinstance(driver, str) or not driver or not Path(driver).is_file():
        raise UpstreamContractError(f"Provider driver_path is missing or not a file: {provider!r}")
    print(f"ODBC provider: {provider}")


def prepare(upstream):
    # Missing checkouts and command failures are infrastructure/build errors,
    # not evidence of upstream interface drift.
    if not upstream.is_dir():
        raise FileNotFoundError(f"Missing mssql-python checkout: {upstream}")
    if not (upstream / "setup_odbc.py").is_file():
        raise UpstreamContractError("Upstream checkout no longer provides setup_odbc.py")
    with tempfile.TemporaryDirectory(prefix="mssql-python-odbc-") as directory:
        subprocess.run(
            [sys.executable, "setup_odbc.py", "bdist_wheel", "--dist-dir", directory],
            cwd=upstream, check=True,
        )
        wheel = select_odbc_wheel(Path(directory))
        subprocess.run(
            [sys.executable, "-m", "pip", "install", "--no-deps", str(wheel)],
            check=True,
        )
    subprocess.run(
        [sys.executable, "-m", "pip", "install", "--no-deps", "-e", str(upstream)],
        check=True,
    )
    check_runtime_dependencies()
    # A fresh interpreter processes the editable install's .pth files.
    result = subprocess.run(
        [sys.executable, str(Path(__file__).resolve()), "--verify-provider"],
        check=False,
    )
    if result.returncode == CONTRACT_EXIT:
        raise UpstreamContractError("Upstream provider verification failed; see diagnostics above")
    result.check_returncode()


def write_result(path, outcome, message):
    suite = ET.Element(
        "testsuite", name="mssql-python setup", tests="1",
        failures=str(int(outcome == "failure")), skipped=str(int(outcome == "skipped")),
    )
    case = ET.SubElement(suite, "testcase", name="Runtime dependencies and native provider")
    if outcome != "passed":
        ET.SubElement(case, outcome, message=message).text = message
    ET.ElementTree(suite).write(path, encoding="utf-8", xml_declaration=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-provider", action="store_true")
    parser.add_argument("--upstream", type=Path)
    parser.add_argument("--results", type=Path)
    args = parser.parse_args(argv)
    if args.verify_provider:
        try:
            verify_provider()
        except UpstreamContractError as error:
            print(f"Upstream provider contract: {error}", file=sys.stderr)
            return CONTRACT_EXIT
        return 0
    if args.upstream is None or args.results is None:
        parser.error("--upstream and --results are required for setup")

    print("##vso[task.setvariable variable=mssqlPythonReady]false", flush=True)
    outcome = "failure"
    message = "Setup did not complete; see the build/setup log."
    exit_code = 0
    try:
        prepare(args.upstream.resolve())
        outcome, message = "passed", ""
    except UpstreamContractError as error:
        message = f"Upstream mssql-python contract drift: {error}"
        if os.environ.get("BUILD_REASON") not in (None, "", "PullRequest"):
            outcome = "skipped"
            print(f"##vso[task.logissue type=warning]{message}")
        else:
            print(f"##vso[task.logissue type=error]{message}", file=sys.stderr)
            exit_code = 1
    finally:
        write_result(args.results, outcome, message)
    if outcome == "passed":
        print("##vso[task.setvariable variable=mssqlPythonReady]true")
    elif outcome == "skipped":
        print("##vso[task.complete result=SucceededWithIssues;]")
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
