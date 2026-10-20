# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Emit the run-scoped SQL credential as an Azure Pipelines secret output."""

import secrets


def main():
    # Generate both before publishing anything; RNG failures must fail the job.
    password = "Aa1!" + secrets.token_hex(24)
    generation = secrets.token_hex(16)
    print(f"##vso[task.setvariable variable=SQL_PASSWORD;isSecret=true;isOutput=true]{password}")
    print(f"##vso[task.setvariable variable=SQL_CREDENTIAL_GENERATION;isOutput=true]{generation}")


if __name__ == "__main__":
    main()
