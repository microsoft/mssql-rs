# Plan: Move Development to GitHub, Rename ADO `main` → `stable`

## Problem

Today the source-of-truth for development is Azure DevOps (`mssql-rs/mssql-rs`):

- Contributors PR to ADO `development`.
- ADO `validation-pipeline.yml` runs on PR + on push to `development`.
- A helper pipeline (`sync-development-to-main.yml`) auto-opens an ADO PR `development → main` once `development` is green.
- OneBranch official build runs on push to ADO `main`. Releases come from there.
- ADO `main` is one-way mirrored to `github.com/microsoft/mssql-rs` `main` for transparency.

We want to flip this so GitHub is the development surface, ADO is the release surface:

- Contributors PR to GitHub `main` (which becomes the new "development" branch).
- ADO `main` is renamed to `stable`. `stable` is the release branch.
- `validation-pipeline.yml` is **split into two separate pipeline YAMLs** so each can be authorized on a different set of service connections:
  - **`validation-pipeline-pr.yml`** — runs on PR open/update against GitHub `main`. Stages: `Build`, `Kerberos_Test_PR` (2 distros), `Build_mssql_python` cross-repo. Acts as the required GitHub PR check. **Authorized on a single read-only SC** (`mssqlrs-acr-pull`, AcrPull on `tdslibrs`) — no ACI, no ACR push, no Azure SQL, no broad subscription scope. Safe to run from contributor (eventually fork) source.
  - **`validation-pipeline-ci.yml`** — runs on push to GitHub `main` (post-merge only). Stages: full distro matrix (`Test_alpine`, `Test_alpine_arm64`, `Test_amd64`, `Test_arm64`), `Kerberos_Test_Full` (9 distros), `PrivateLink_Smoke`. **Authorized on the elevated SCs** (ACI deploy, ACR push, MI handling) because it only ever runs from already-merged trusted code on `main`.
  - The split replaces the `Build.Reason == 'PullRequest'` stage gating that exists today in a single `validation-pipeline.yml`. Common stage YAML lives in `templates/` and is referenced by both pipelines.
- The GitHub→ADO sync is gated on **`validation-pipeline-ci.yml`** going green — never on the PR pipeline. Only after the CI pipeline passes on GitHub `main` does the sync pipeline open the ADO PR to `stable`.
- All releases (OneBranch official build / release) continue to run from ADO `stable`.

## Decisions (confirmed)

| Topic | Decision |
|---|---|
| `stable` on GitHub | **No mirror** — `stable` lives only in ADO. |
| PR validation CI for GitHub PRs | **ADO pipelines via Azure Pipelines GitHub App** (existing 1ES pools, ACR, internal feeds). Fork PRs require maintainer approval. |
| GitHub → ADO sync | **Auto-create ADO PR** when GitHub `main` CI is green; human gates the merge. |
| ADO `development` branch | **Keep** as transitional/internal branch (do not delete). |
| External contributors | **Internal Microsoft contributors first**, open externally in a later phase. |
| Cutover style | **Phased** — stand up GitHub flow in parallel, then deprecate ADO `development` PRs. |
| Validation pipeline shape | **Split** `validation-pipeline.yml` into `validation-pipeline-pr.yml` and `validation-pipeline-ci.yml` so PR runs and post-merge CI runs can be authorized on different (least-privilege) service connections. |
| ADO project hosting | **`validation-pipeline-pr.yml` lives in a public ADO project** so external GitHub PR viewers can click through to the check and read the run/logs anonymously. **All other pipelines — including `validation-pipeline-ci.yml`, the sync pipeline, `fuzz-pipeline.yml`, `benchmark-pipeline.yml`, and the OneBranch Official Build/Release — stay in the existing private `mssql-rs/mssql-rs` project**, even when their source repo is GitHub. The elevated service connections (`mssqlrs-aci-deploy`, `mssqlrs-acr-push`) live in the private project; only `mssqlrs-acr-pull` (read-only) is created in the public project. |

## Target end-state

```
GitHub: microsoft/mssql-rs
  main    ← contributors send PRs here
            • PR opened/updated  → validation-pipeline-pr.yml (Build + Kerberos_Test_PR + Build_mssql_python)
                                    AcrPull-only SC; posts required PR check back to GitHub
            • PR merged to main → validation-pipeline-ci.yml
                                    (full Test_alpine / amd64 / arm64 matrix, Kerberos_Test_Full, PrivateLink_Smoke)
                                    holds the ACI / ACR / MI service connections
            • CI green on main  → sync pipeline auto-opens ADO PR (GitHub main → ADO stable)

Azure DevOps: mssql-rs/mssql-rs
  stable  ← release branch (renamed from `main`)
            • merge from sync PR → OneBranch Official Build (Wheels) triggers on push
            • Manual release    → OneBranch Release pipeline (publish NuGet, tag, branch)
  development ← retained as transitional/internal branch (no longer the dev surface)
```

## Visual overview

> Mermaid blocks below use the Azure DevOps Wiki fence syntax (`::: mermaid` ... `:::`) instead of the standard fenced code block. They use only `flowchart` and `sequenceDiagram` types, plain ASCII labels (no `<br/>`, no emoji, no special unicode), no `stateDiagram-v2` composite states, and no thick (`==>`) or piped-label link variants.

### Today (current state)

::: mermaid
flowchart LR
    Dev([Developer])
    subgraph ADO["Azure DevOps - mssql-rs/mssql-rs"]
        ADOdev[(development)]
        ADOmain[(main)]
        ValPipe{{validation-pipeline.yml}}
        SyncPipe{{sync-development-to-main.yml}}
        OBBuild{{OneBranch Official Build}}
        OBRelease{{OneBranch Release}}
    end
    subgraph GH["GitHub - microsoft/mssql-rs (mirror)"]
        GHmain[("main (read-only mirror)")]
    end

    Dev -- "PR" --> ADOdev
    ADOdev -- "PR / push triggers" --> ValPipe
    ValPipe -- "green" --> SyncPipe
    SyncPipe -- "auto-PR" --> ADOmain
    ADOmain -- "push triggers" --> OBBuild
    OBBuild -- "manual" --> OBRelease
    ADOmain -- "one-way mirror" --> GHmain
:::

### Target (after cutover)

::: mermaid
flowchart LR
    Dev([Microsoft contributor])
    subgraph GH["GitHub - microsoft/mssql-rs (authoritative for dev)"]
        GHmain[(main)]
    end
    subgraph ADOPub["Azure DevOps - public project (anon read)"]
        ValPipePR{{"validation-pipeline-pr.yml (AcrPull only)"}}
    end
    subgraph ADO["Azure DevOps - mssql-rs/mssql-rs (private)"]
        ADOstable[("stable (release branch)")]
        ADOdev2[("development (internal-only)")]
        ValPipeCI{{"validation-pipeline-ci.yml (elevated SCs)"}}
        SyncNew{{sync-github-main-to-stable.yml}}
        OBBuild2{{OneBranch Official Build}}
        OBRelease2{{OneBranch Release}}
    end

    Dev -- "PR to main" --> GHmain
    GHmain -- "PR trigger" --> ValPipePR
    ValPipePR -. "required check" .-> GHmain
    GHmain -- "push trigger (post-merge)" --> ValPipeCI
    ValPipeCI -- "pipeline-completion" --> SyncNew
    SyncNew -- "auto-PR (human approves)" --> ADOstable
    ADOstable -- "push triggers" --> OBBuild2
    OBBuild2 -- "manual" --> OBRelease2
:::

### PR to release sequence

::: mermaid
sequenceDiagram
    autonumber
    actor Dev as Microsoft contributor
    participant GH as GitHub main
    participant ADOPR as validation-pipeline-pr.yml
    participant ADOCI as validation-pipeline-ci.yml
    participant Sync as sync-github-main-to-stable
    participant Stable as ADO stable
    participant OB as OneBranch (Build then Release)

    Dev->>GH: Open PR to main
    GH->>ADOPR: PR trigger (AcrPull-only SC)
    ADOPR-->>GH: Required check (Build + Kerberos_PR + mssql-python build)
    Dev->>GH: Merge PR
    GH->>ADOCI: Push trigger (elevated SCs)
    ADOCI->>ADOCI: Full distro matrix + Kerberos_Full + PrivateLink_Smoke
    ADOCI-->>Sync: pipeline-completion (Succeeded only)
    Sync->>Stable: Fast-forward sync/github-main and open or update the rolling PR into stable
    Note over Stable: Human reviewer approves & merges
    Stable->>OB: Push triggers Official Build (wheels)
    OB->>OB: Manual release (publish NuGet, tag, branch)
:::

### Cutover phase progression

::: mermaid
flowchart TB
    Today["Today: ADO development = dev surface, ADO main = release, GitHub main = mirror"]
    Phase0["Phase 0: Audit pipelines and secrets"]
    Phase1a["Phase 1a: Harden secrets and service connections"]
    Phase1b["Phase 1b: Stand up GitHub-source pipeline (parallel run)"]
    Phase2["Phase 2: Build sync GitHub main to stable"]

    subgraph Cutover["Phase 3: Cutover window"]
        direction TB
        C1["Freeze ADO development"]
        C2["Rename ADO main to stable"]
        C3["Stop ADO to GitHub mirror"]
        C4["Land branch-ref YAML edits"]
        C5["Promote GitHub PR check to required"]
        C6["Internalize ADO development"]
        C7["Announce"]
        C1 --> C2 --> C3 --> C4 --> C5 --> C6 --> C7
    end

    Phase4["Phase 4: Hardening and cleanup"]
    Steady["Steady state: GitHub main = dev surface, ADO stable = release, ADO development = internal-only"]
    Phase5["Phase 5 (deferred): Open to external contributors (CLA, GH Actions)"]

    Today --> Phase0 --> Phase1a --> Phase1b --> Phase2 --> Cutover --> Phase4 --> Steady --> Phase5
:::

### Trust boundary and service connections (Phase 1a)

::: mermaid
flowchart TB
    subgraph GHrun["GitHub-sourced pipeline run (wider trust boundary)"]
        Job["Pipeline job"]
    end

    subgraph SC["Purpose-scoped OIDC service connections"]
        SCAci["mssqlrs-aci-deploy: scope rg=rust-lib-rg, role ACI ops only"]
        SCAcr["mssqlrs-acr-push: scope ACR=tdslibrs, role AcrPush"]
    end

    subgraph Azure["Azure resources"]
        ACI["ACI in trusted VNet, MI auth only"]
        ACR["ACR tdslibrs"]
        SQL["Azure SQL mssqlrustlibtest, MI auth (no password)"]
    end

    Job -- "least-privilege OIDC token" --> SCAci
    Job -- "least-privilege OIDC token" --> SCAcr
    SCAci -- "deploy / delete only" --> ACI
    SCAcr -- "push image" --> ACR
    ACI -- "MI auth" --> SQL
    ACI -. "pull image" .-> ACR

    Banned["Removed: stored ACI SQL password; subscription-wide Magnitude Test on GitHub-source pipeline; variable groups with secrets"]
    GHrun -. "no longer references" .-> Banned
:::

## Phased rollout

### Phase 0 — Pre-work (no user-visible change)

- Audit every `.pipeline/**/*.yml` reference to `development` and `main` and decide the new branch each one points at.
  - `OneBranch/OfficialPythonWheelsBuild.yml`: `main` → `stable`.
  - `OneBranch/OfficialPythonWheelsRelease.yml`: resource ref `refs/heads/main` → `refs/heads/stable`.
  - `OneBranch/NonOfficialPythonWheelsPublish.yml`: trigger `development` → GitHub `main`; PR `main`/`development` → GitHub `main`; nightly schedule `main` → `stable`; resource ref `refs/heads/main` → `refs/heads/stable`.
  - `validation-pipeline.yml`: **split** into `validation-pipeline-pr.yml` (PR trigger on GitHub `main`) and `validation-pipeline-ci.yml` (CI trigger on push to GitHub `main`). Move all reusable stages into `templates/` so both files are thin wrappers.
  - `fuzz-pipeline.yml`: schedule branches `main`, `development` → `stable`, GitHub `main`.
  - `benchmark-pipeline.yml`: PR branches `main`, `development` → GitHub `main`; `baselineBranch` default → GitHub `main`.
  - `sync-container-images.yml`: branches `development`, `main` → `stable` (and GitHub `main` if needed).
  - `sync-development-to-main.yml`: **delete** (replaced by new sync below).
  - `templates/test-mssql-python-template.yml` + `-macos`: confirm `MSSQL_PYTHON_BRANCH=main` fallback still correct (it points at `microsoft/mssql-python`, unrelated to this rename).
- Decide on `mssql-python` cross-repo branch references (`docs/release-management.md` mentions "main/development" — needs rewording for `stable` vs GitHub `main`).
- Inventory secret/service-connection access from GitHub-triggered runs (1ES pool authorization, ACR pull token, NuGet feed PAT, OneBranch service connections).

### Phase 1 — Stand up GitHub PR + CI validation in parallel

Goal: GitHub PRs to `main` produce the same PR check signal that ADO `development` PRs do today, **and** post-merge pushes to GitHub `main` produce the same full CI matrix run that pushes to ADO `development` do today — **without** disrupting the existing ADO `development` flow, and with PR runs and CI runs cleanly separated so they can hold different service-connection grants.

`validation-pipeline.yml` today encodes both modes in one file via `eq(variables['Build.Reason'], 'PullRequest')` conditions. We split it into two YAMLs (`validation-pipeline-pr.yml` and `validation-pipeline-ci.yml`) sharing the same stage templates from `templates/`. Each ADO pipeline definition points at one YAML and is authorized on its own service connections.

#### Phase 1a — Secret & service-connection hardening (prerequisite)

GitHub-sourced runs broaden the trust boundary (compromised contributor account, future fork PRs, leak via build logs). Before any GitHub trigger goes live, the pipeline must be safe to run under that wider trust model. Concretely:

1. **Verify (and keep) the no-stored-SQL-password posture.**
   - Audited as of this plan: every pipeline that needs a SQL Server password generates it per-job via `templates/generate-sql-password-template.yml` (job-scoped, secret-masked, ephemeral). Confirmed callers: `sql-setup-template.yml`, `build-template.yml`, `build-template-container.yml`, `test-longhaul-template.yml`, `test-matrix-template-alpine.yml`. `benchmark-pipeline.yml` inherits via `sql-setup-template.yml`.
   - Audited: `private-link-smoke-template.yml` connects to Azure SQL via **managed identity only** (`SMOKE_AUTH_MODE=managed_identity`, `MI_CLIENT_ID=…`, no password env var passed to the smoke container).
   - No stored SQL credential exists in a pipeline variable group today.
   - **Action**: add a CI guardrail (a small linter step in `validation-pipeline.yml`, or a docs-and-PR-template note) that flags any new template that introduces a `SQL_PASSWORD`, `MSSQL_SA_PASSWORD`, or similar pipeline variable / variable-group reference instead of including `generate-sql-password-template.yml` or using MI. The goal is to prevent regression once GitHub-sourced runs are live.

2. **Remove the ACI SQL host from pipeline-protected configuration.**
   - The Azure SQL server FQDN (`mssqlrustlibtest.database.windows.net`) and database name are currently template parameters with defaults — that's already non-secret. Confirm there is no variable group / secret variable holding host info that GitHub-sourced runs would inherit. Keep host/DB as plain template parameters, or move to a non-secret pipeline variable.
   - Document explicitly in the smoke template that host/DB are non-sensitive (the security boundary is at the network + MI level, not at hostname obscurity).

3. **Split the `Magnitude Test` service connection by trust boundary.**
   - Today this service connection is used by `azure-cli-login-template.yml`, `sync-container-images.yml`, `private-link-smoke-template.yml`, `test-longhaul-template.yml`, and `build-template-container.yml` — i.e. broad subscription-level Azure access.
   - Trust-boundary rule for the split:
     - **`validation-pipeline-pr.yml`** (runs from contributor source, eventually from forks): authorized on **exactly one** Azure service connection — `mssqlrs-acr-pull` — with **AcrPull only** on ACR `tdslibrs`. This is the minimum needed to pull build images for the PR build/test stages. Read-only, single resource, no mutating action, no SQL access, no MI handout, no VNet rights. Any stage that needs more than image pull does **not** belong in the PR pipeline.
     - **`validation-pipeline-ci.yml`** (runs only from already-merged code on `main`): authorized on the new least-privilege OIDC-federated service connections below.
   - The new connections:
     - `mssqlrs-acr-pull` — **AcrPull only** on ACR `tdslibrs`. **PR pipeline only.**
     - `mssqlrs-aci-deploy` — can only manage ACI in `rust-lib-rg`. CI pipeline only. Used by `private-link-smoke-template.yml`.
     - `mssqlrs-acr-push` — **AcrPush** on ACR `tdslibrs`. CI pipeline only. Used by `sync-container-images.yml` and the smoke build step.
     - `mssqlrs-storage-readonly` — only if any pipeline needs blob/Key Vault read; otherwise omit.
   - Because the CI pipeline only runs from `main` after a human-reviewed merge, the elevated SCs (`mssqlrs-aci-deploy`, `mssqlrs-acr-push`) are never exposed to PR-time code execution. A forked PR (Phase 5) physically cannot trigger `validation-pipeline-ci.yml` — it has no PR trigger. The worst a malicious PR can do via `mssqlrs-acr-pull` is read public-to-the-pipeline image bytes from one ACR.

   **Outline of the work (suggestive — needs further research before execution):**

   - Pick an identity type for each new service connection. The Microsoft corp tenant restricts `az ad app create`, so a classic Entra app registration likely needs an Identity Service request; a User-Assigned Managed Identity in our subscription may be a friendlier option. Confirm which one OneBranch / 1ES guidance currently recommends for ADO workload-identity federation in our org.
   - Decide the RBAC scope and role for each connection. Aim for the smallest scope that works (resource group for ACI, the ACR resource itself for image push, AcrPull on the same ACR for the PR connection). Built-in roles `AcrPull` / `AcrPush` cover the ACR cases; for ACI a custom role limited to `Microsoft.ContainerInstance/*` plus the VNet `join/action` is likely better than `Contributor`, but the exact action list needs to be verified against `private-link-smoke-template.yml` behavior.
   - Create each ADO service connection using the **Workload Identity federation (manual)** option and bind it to the identity from above. The exact UI flow and the federation subject claim format are evolving — check current ADO docs at the time of execution.
   - Lock each connection down: disable "Grant access permission to all pipelines" and grant access explicitly to the one pipeline definition that needs it (`mssqlrs-acr-pull` → PR definition only; `mssqlrs-aci-deploy` / `mssqlrs-acr-push` → CI definition only). Add an approval check for runs originating from forked PRs (dormant while internal-only, useful for Phase 5).
   - Parameterize `azure-cli-login-template.yml` and the ACI / ACR templates so the service-connection name comes from a parameter instead of being hardcoded to `Magnitude Test`. Keep the legacy default during the parallel period so the ADO-source pipeline is untouched; the GitHub-source pipelines pass the new least-privilege connection names.
   - Verify after wiring: queue a restricted run of each GitHub-source pipeline and confirm subscription-wide Azure operations fail with `AuthorizationFailed`; confirm the PR pipeline can only pull from ACR; confirm the CI pipeline can do ACI deploy + ACR push but nothing else.

   **Open research items before this can be executed:**
   - Which identity type (UAMI vs Entra app via Identity Service request) is the supported path in the corp tenant today; what the lead time is for either.
   - Exact list of `Action` / `DataAction` permissions needed for the ACI deploy + VNet-injection flow used by `private-link-smoke-template.yml` (drives the custom role definition).
   - Whether OneBranch official build pipelines have any opinion on / requirement for which service connection performs the smoke deploy.
   - Whether `test-longhaul-template.yml` and `build-template-container.yml` truly need their current `Magnitude Test` scope, or can move to one of the narrower connections (avoids leaving `Magnitude Test` in use as a back door).
   - Whether the PR pipeline's image pulls can be served by an anonymous-pull ACR repository or by a build-pool-side credential, removing the need for `mssqlrs-acr-pull` entirely.

   **Why split + federate** (rather than just narrow `Magnitude Test`):
   - One identity per blast radius — compromising the ACR-push connection cannot delete ACI; compromising the ACI connection cannot push malicious images; the PR connection can only read from one ACR.
   - OIDC federation eliminates stored client secrets, so a leaked pipeline log or a compromised ADO project secret store can't be replayed.
   - Per-pipeline authorization stops a future ADO pipeline (or a forked-PR run) from silently inheriting these credentials.

4. **Lock down `System.AccessToken` scope.**
   - On the new GitHub-source pipeline definition, set "Limit job authorization scope to current project" (already default) and "Protect access to repositories in YAML pipelines" → require the pipeline to declare every repo it touches under `resources.repositories`.

5. **Pipeline-side approval gate for protected resources.**
   - For each new least-privilege service connection, enable "Approvals and checks" → require a maintainer approval for any run from a forked PR. This is dormant while we're internal-only but pre-wires Phase 5.

6. **Audit existing variable groups.**
   - Enumerate every variable group (`Library` in ADO) referenced by the pipelines we're enabling on GitHub. Remove any secret that is no longer needed; for those that remain, mark them as pipeline-permission-restricted to the ADO-source pipeline definition only. The GitHub-source definition starts with **zero** variable group access and only adds what's strictly required after audit.

7. **Log scrubbing check.**
   - Run a probe job that echoes every variable used to confirm no surprise secret bleeds into logs visible to GitHub PR authors. The Azure Pipelines App posts log links into PRs.

Exit criteria for Phase 1a: a dry-run of both `validation-pipeline-pr.yml` (authorized on `mssqlrs-acr-pull` only) and `validation-pipeline-ci.yml` (authorized on the new least-privilege OIDC SCs) on a throwaway GitHub branch passes, with no stored SQL credential and no broad-subscription token in either run. Confirm by inspecting each run's "Authorized resources" panel that the PR pipeline lists exactly one Azure SC (`mssqlrs-acr-pull`) and the CI pipeline lists only `mssqlrs-aci-deploy` + `mssqlrs-acr-push`.

#### Phase 1b — GitHub pipeline wiring

1. **(Already done)** Azure Pipelines GitHub App is installed on `microsoft/mssql-rs`. No action — proceed to defining pipelines against the GitHub source.
2. Create the split YAMLs:
   - `validation-pipeline-pr.yml` — only `pr:` trigger on GitHub `main`. Stages: `Build`, `Kerberos_Test_PR`, `Build_mssql_python`. Reuses templates from `templates/`. No `azure-cli-login-template.yml`, no ACI/ACR/MI references.
   - `validation-pipeline-ci.yml` — only `trigger:` (CI) on push to GitHub `main`. Stages: `Test_alpine`, `Test_alpine_arm64`, `Test_amd64`, `Test_arm64`, `Kerberos_Test_Full`, `PrivateLink_Smoke`. May reference the elevated SCs from Phase 1a.
   - The legacy `validation-pipeline.yml` stays in place during the parallel period (still triggered by ADO `development`) and is removed in Phase 4.
3. Add **two new** ADO pipeline definitions (UI-created), both whose source repo is the GitHub repo, in the projects called out in the Decisions table:
   - **Public ADO project** → pipeline pointing at `validation-pipeline-pr.yml`. **Authorize it on `mssqlrs-acr-pull` only** (AcrPull on `tdslibrs`, created in this same public project). Plus the 1ES pool and the NuGet feed PAT. Nothing else. This pipeline's runs/logs are anonymously readable, which is the whole reason it lives here.
   - **Private `mssql-rs/mssql-rs` project** → pipeline pointing at `validation-pipeline-ci.yml`. **Authorize it on the new least-privilege OIDC SCs from Phase 1a** (`mssqlrs-aci-deploy`, `mssqlrs-acr-push`, etc., all created in the private project). Restrict to push events from `main` (no PR trigger). All other GitHub-sourced pipelines (`fuzz-pipeline.yml`, `benchmark-pipeline.yml`, etc.) also live here.
   - Initially restrict the PR-pipeline trigger to PRs from internal Microsoft contributors (use the GitHub team allowlist on the pipeline definition).
4. Confirm 1ES pool, ACR, NuGet feed, Kerberos test infra, and `mssql-python` cross-repo template all work when the source is GitHub, for both pipelines. Pay particular attention to:
   - `persistCredentials`/`System.AccessToken` semantics differ for GitHub-sourced runs.
   - Fork PRs cannot access protected resources — keep "approval for outside contributors" enabled on the PR pipeline (dormant while internal-only).
   - The CI pipeline must succeed with only the elevated SCs (no fallback to `Magnitude Test`).
5. Wire status checks back to GitHub (Azure Pipelines App does this automatically once installed). The PR pipeline posts the required check on the PR; the CI pipeline posts a status on the merge commit on `main`.
6. Validate with a couple of test PRs on a throwaway branch — confirm both pipelines run on the right events and complete successfully.

### Phase 2 — Build the GitHub-main → ADO-stable sync

Replace `sync-development-to-main.yml` with a new pipeline:

- **Trigger**: pipeline-completion trigger on **`validation-pipeline-ci.yml`** against GitHub `main` (the post-merge pipeline, not the PR pipeline). Optionally also a scheduled fallback (e.g. every 30 min) in case the completion trigger misfires.
- **Source repo**: ADO `mssql-rs/mssql-rs` (so `az repos pr create` has the right context).
- **Gating**: only proceeds if the upstream `validation-pipeline-ci.yml` run completed with `result == 'Succeeded'`. A failed CI run on GitHub `main` must NOT auto-promote to `stable`.
- **Behavior** (single rolling sync PR, not one-PR-per-commit):
  1. Fetch the GitHub `main` SHA the upstream CI run was built from (via `resources.pipelines.*.sourceCommit`).
  2. **Use a single, stable working branch name — `sync/github-main`** (no SHA suffix) — and **fast-forward** it to that SHA in the ADO repo. If the working branch doesn't exist yet, create it. If a non-fast-forward update would be needed (someone hand-edited the working branch), fail loudly — never force-push.
  3. Look up an existing **active** PR with source `sync/github-main` and target `stable`:
     - **No active PR**: open one. Title: `Sync GitHub main → stable (<short-sha>)`. Description lists the commit range `<previous-stable-tip>..<github-main-sha>` with one bullet per GitHub commit (subject + author + GitHub URL) and a link to the green CI run.
     - **Active PR exists**: do NOT open a second one. The branch fast-forward in step 2 has already updated the PR's source ref, so ADO automatically refreshes its diff and the file list. Update the PR title to the new HEAD short SHA and **append** the new commits to the description (mark previously-listed commits as already covered). Post a single PR comment summarizing what was added in this run and which CI run produced it. Reviewer votes are reset by the branch update; that is acceptable and intentional — the diff is now larger and needs re-review.
  4. Skip the entire step if `git diff stable..sync/github-main` is empty (already merged, or upstream commit was a no-op against `stable`).
  5. **Concurrency guard**: take a lease (e.g. an ADO build-tag-based mutex, or a short pipeline lock variable) so two CI runs completing back-to-back can't race on the branch update / PR open. The second run waits, then sees the first run's PR and falls through to the "active PR exists" path.
  6. **Race with human merge**: if between step 2 (fast-forward) and step 3 (PR lookup) the human merges the PR, the next run will find no active PR and a non-empty diff, and will correctly open a fresh PR for the new commits since `stable`. Idempotent.
- This produces **one open sync PR at all times** that always reflects the latest CI-green GitHub `main` HEAD; the human merges when ready, batches as many or as few commits as they like, and the next CI run after the merge starts the next batch.
- Human reviewer merges the ADO PR → triggers OneBranch Official Build on `stable` → release flow unchanged.

Add a one-time job (or document a manual step) to perform the **first** `stable ←→ GitHub main` reconciliation: at cutover, ADO `main` and GitHub `main` will diverge in history (rename + any in-flight merges), so the first sync may need a forced fast-forward or a merge commit.

### Phase 3 — Cutover

Schedule a cutover window (~1–2 hours):

1. **Freeze** ADO `development` (branch policy: block all pushes/PRs).
2. Drain in-flight ADO `development → main` PRs (merge or close).
3. **Rename** in ADO: `main` → `stable`. (Update default branch in repo settings.)
   - Update branch policies attached to the old `main` to attach to `stable`.
   - Update any pipeline definitions whose "Default branch for manual and scheduled builds" was `main`.
4. **Stop** the existing ADO `main` → GitHub `main` mirror job. From now on, GitHub `main` is authoritative; nothing else writes to it.
5. Verify GitHub `main` HEAD matches ADO `stable` HEAD at this instant (they should — the mirror was the source).
6. Merge the Phase-0 YAML edits (pipeline branch references) into ADO `stable` directly via a one-off `stable` PR (since `development` is frozen and GitHub-main validation is not yet promoted to required).
7. Mirror that change forward to GitHub `main` (one-time push) so the two are in sync.
8. **Promote** the GitHub-main validation pipeline (PR-mode) check to a **required** PR check on GitHub `main`. The CI-mode run is not a PR check (it runs on the merge commit) but its success is the gate for the GitHub→stable sync (Phase 2).
9. Add GitHub branch protection on `main`: require PR, require Azure Pipelines check, require CODEOWNERS review, dismiss stale reviews, no force pushes, no deletion.
10. Update `CONTRIBUTING.md`: document that PRs go to GitHub `main`; note that contributions are currently limited to Microsoft employees.
11. Update `.github/copilot-instructions.md` ("Integration branch" wording) and the prompt files in `.github/prompts/` (`createPr.prompt.md`, `createDraftPr.prompt.md`) to target GitHub `main` instead of ADO `development`.
12. Re-enable ADO `development` as a **read-only / internal-use** branch — keep it in case internal automation still references it, but block PRs to it via branch policy.
13. Announce cutover (team channels, README banner if desired).

### Phase 4 — Hardening & cleanup

- Delete `sync-development-to-main.yml` from the ADO repo (after the new GitHub→stable sync has been observed working for ≥ 1 week).
- Remove the legacy `validation-pipeline.yml` (and its ADO pipeline definition) once no PRs target ADO `development` and the GitHub-sourced PR + CI pipelines have been the source of truth for at least one full release cycle.
- Remove the parallel ADO-source pipeline definition; leave only the GitHub-source one.
- Update `docs/release-management.md` flow diagrams (replace "Push to main/development" with "Push to GitHub main" and "Push to ADO stable").
- Verify `mssql-python` cross-repo flow still works (it pulls `mssql-py-core` artifacts by NuGet version; nothing in those artifacts depends on the old branch names, but confirm).
- Decide on telemetry: ensure release notes/tags created by `OfficialPythonWheelsRelease.yml` reflect the `stable` branch correctly.

### Phase 5 (deferred) — Open to external contributors

Out of scope for this plan, but anticipated next:

- Add Microsoft CLA bot.
- Update `CONTRIBUTING.md` for external contributions.
- **(Already done)** "Require approval for builds from forks" and "Limit job authorization scope" are already configured on the GitHub-source pipeline definition. Verify the settings still apply once the PR/CI pipeline split lands.
- Consider a lightweight GitHub Actions workflow (fmt + clippy + unit tests on `ubuntu-latest`) so fork PRs get *some* signal without needing maintainer approval to run the heavyweight ADO matrix.

## Files that will change

Pipelines (all in `.pipeline/`):

- `validation-pipeline.yml` — **split** into:
  - New: `validation-pipeline-pr.yml` (PR pipeline; `mssqlrs-acr-pull` only)
  - New: `validation-pipeline-ci.yml` (CI pipeline; holds elevated SCs)
  - Legacy `validation-pipeline.yml` retained during parallel period, deleted in Phase 4.
- `fuzz-pipeline.yml` — schedule branches
- `benchmark-pipeline.yml` — PR branches + default param
- `sync-container-images.yml` — branches
- `sync-development-to-main.yml` — **delete** (Phase 4)
- `OneBranch/OfficialPythonWheelsBuild.yml` — trigger branch + resource ref
- `OneBranch/OfficialPythonWheelsRelease.yml` — resource ref
- `OneBranch/NonOfficialPythonWheelsPublish.yml` — trigger + PR + schedule + resource ref
- New: `.pipeline/sync-github-main-to-stable.yml`

Docs / repo metadata:

- `CONTRIBUTING.md`
- `README.md` (banner, if desired)
- `docs/release-management.md`
- `.github/copilot-instructions.md`
- `.github/prompts/createPr.prompt.md`
- `.github/prompts/createDraftPr.prompt.md`
- `.github/CODEOWNERS` (verify ownership rules still apply on GitHub PRs)
- `.github/PULL_REQUEST_TEMPLATE.md` (verify wording is GitHub-appropriate)

ADO / GitHub admin (no file changes — manual config):

- ADO branch rename `main` → `stable`, default branch update, branch policy migration.
- ADO branch policy on `development` → block PRs.
- ADO pipeline definitions: update default branch for scheduled/manual runs.
- Stop existing ADO→GitHub mirror job.
- GitHub: install Azure Pipelines App, branch protection rules on `main`, add Microsoft team as code reviewers, update CODEOWNERS if needed.

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| ADO pipelines triggered from GitHub can't access internal resources | Phase 1 validates this end-to-end before cutover; restrict to internal contributors so PR-secret-scope is acceptable. |
| Divergent history at cutover | Phase 3 step 5 verifies HEAD parity; if mismatched, do a one-shot reset of GitHub `main` to ADO `stable` (or vice-versa) inside the freeze window. |
| Existing scheduled triggers (nightly, fuzz, benchmark) silently stop firing because branch references go stale | Phase 0 inventory + Phase 4 verification with a calendar reminder one week post-cutover. |
| Auto-sync pipeline opens duplicate PRs or loops | Reuse the dedup logic from `sync-development-to-main.yml` (check for active PR + diff); fast-forward-only push to working branch. |
| Force-push or rewrite on GitHub `main` corrupts ADO `stable` | GitHub branch protection forbids force-push/delete; sync pipeline only fast-forwards. |
| Release pipeline (OneBranch) misconfigured for `stable` | Run a dry-run release with `publishNuGet=false`, `tagRelease=false` (defaults) on `stable` immediately post-cutover before the next real release. |
| Internal automation still pushes to ADO `development` | Phase 3 keeps `development` alive; Phase 4 monitors and removes once quiet. |

## Open items (not blockers, decide during execution)

- Pipeline-completion trigger vs scheduled fallback (or both) for the GitHub→stable sync.
- What happens if CI-mode on GitHub `main` fails: alert channel, who triages, whether to auto-revert the GitHub merge or just block the next sync until a forward-fix lands.
- Whether the GitHub-main validation pipeline should also publish coverage to GitHub PR comments (today `azurepipelines-coverage.yml` posts to ADO PRs).
- Whether to keep `azurepipelines-coverage.yml` as-is or add a Codecov/GH-native alternative.
