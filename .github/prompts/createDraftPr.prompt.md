---
name: createDraftPr
description: Describe when to use this prompt
---
Create a DRAFT GitHub PR for this branch in the `microsoft/mssql-rs` repository, targeting the `main` branch.

Do not use unicode characters or superlatives in the PR description.

The PR description should be created using the changes from the current branch and the target branch. The changes should be listed in a bullet point format.

Do not list metrics like lines of code changed or number of files changed. Instead, focus on the functional changes and improvements.
