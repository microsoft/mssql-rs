# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Emit the run-scoped SQL credential generation ID as a nonsecret output.

Fork PR builds receive no secret variables, including secret outputs passed
across jobs, so the SQL password itself never leaves the SQL host job. The
generation ID only namespaces cross-pool artifacts per initialization.
"""

import secrets


def main():
    generation = secrets.token_hex(16)
    print(f"##vso[task.setvariable variable=SQL_CREDENTIAL_GENERATION;isOutput=true]{generation}")


if __name__ == "__main__":
    main()
