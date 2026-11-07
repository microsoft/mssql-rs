# Git Hooks

This directory contains git hooks that can be shared across the team to maintain code quality and consistency.

## Setup

From a regular clone's repository root, run:

```bash
bash dev/setup-hooks.sh
```

The installer expects a `.git` directory; it does not support linked worktrees.

## Available Hooks

### pre-commit

- **Purpose**: Ensures code is properly formatted before commits
- **What it does**: 
  - Runs `cargo fmt` for the workspace and the excluded `mssql-py-core` crate
  - Prevents commit if formatting changes are needed
  - Shows which files need to be reviewed and re-staged

The hook does not run clippy or tests. Run the repository's local checks
separately.

### Usage

The hook runs automatically before each commit. If formatting changes are needed:

1. The hook will show you which files were modified
2. Review the changes with `git diff`
3. Add the formatted files with `git add .`
4. Commit again

### Bypassing Hooks

If you need to bypass the pre-commit hook (not recommended), use:

```bash
git commit --no-verify
```
