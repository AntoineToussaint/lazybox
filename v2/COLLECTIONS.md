# Runner Collections

The UI calls these **Workspaces**. See [Workspaces](WORKSPACES.md) for the
GitHub-independent repository setup. Existing Collection RPC and storage names
remain compatible with saved state.

A **Collection** is a chosen set of one or more repositories, with a worktree
and branch for each member on a Runner. An agent session attaches to that
Collection and can work across its member repositories. Collection identity
and files outlive individual agent sessions and desktop windows.

## Agreed entry model

Confirmed by the user on 2026-10-29:

- **Repo library:** the repositories registered on a Runner that the user works
  with. Repository identity is distinct from a particular checkout.
- **Collection:** a selected set of those repositories and their concrete
  worktrees. Several members may reference different branches of the same repo.
- **Agent session:** a coding agent working in that Collection.

The client supports two entry paths through the same underlying model:

```text
Single repository → Start session → one-member Collection underneath
Select several repositories → Create Collection → Start session
```

The single-repo shortcut should avoid a separate Collection setup screen.
Existing Collections remain available to inspect and use for later sessions.
Opening or selecting one changes the displayed context. Creating/preparing
worktrees and changing branches are explicit operations.

The repo library and the single-repo shortcut are agreed client requirements;
they are not a claim that the current workspace prototype completes them.
Application help/planning conversations can also use the provider runtime
without a coding Collection; see [provider use modes](INTERFACES.md#provider-use-modes).

Attaching a Collection preserves the selected account and inherits native agent
permissions by default. Collection creation does not authorize account switching
or permission changes. Native project settings follow the execution directory;
permissions from different repositories are not silently combined.

```text
<configured worktreeRoot>/<collection-id>/
  api/             worktree from backend repository
  web/             worktree from frontend repository
  api-comparison/  another branch of backend repository
```

The Collection owns its worktree membership and provisioning facts. An execution
owns the agent process, conversation, commands, and account profile. A Hub
project remains the portable grouping of repositories and objectives; a
Collection is its concrete execution environment on a particular Runner.

## Configure repositories on the Runner

Add local Git repositories to `runner.config.json`. Paths refer to the Runner's
filesystem, including when its client is on another computer. Existing regular
and bare repositories are supported. This slice does not clone or fetch.

```json
{
  "repositories": [
    {"id": "backend", "path": "/absolute/runner/path/to/backend"},
    {"id": "frontend", "path": "/absolute/runner/path/to/frontend"}
  ]
}
```

Start with `make run BENCH_CONFIG=runner.config.json`. The Makefile supplies
`WORKTREE_ROOT`, defaulting to `$(BENCH_STATE)/worktrees`; override it explicitly
to keep Collections separate from temporary session state. Direct service
deployments set `worktreeRoot` in configuration or `LAZYBOX_V2_WORKTREE_ROOT`.
No V1 state, credentials, worktree location, or port is inferred.

The Runner locks the worktree root for its lifetime. Provisioning is serialized
in this initial implementation. Distinct Runners cannot own the same root.

## Create and attach

Save this as a request JSON file. Keep `requestKey` stable when retrying.

```json
{
  "requestKey": "checkout-feature-123",
  "spec": {
    "name": "Checkout feature",
    "members": [
      {"name": "api", "repositoryId": "backend", "baseRef": "HEAD"},
      {"name": "web", "repositoryId": "frontend", "baseRef": "HEAD"}
    ]
  }
}
```

Using the endpoint file printed by the running service:

```sh
bin/lazybox --endpoint "$ENDPOINT" repositories
bin/lazybox --endpoint "$ENDPOINT" create-collection --spec request.json
bin/lazybox --endpoint "$ENDPOINT" collections
bin/lazybox --endpoint "$ENDPOINT" collection --collection "$COLLECTION_ID"
bin/lazybox --endpoint "$ENDPOINT" open --profile claude-default --collection "$COLLECTION_ID"
```

`OpenSession.collection_id` replaces `cwd`. The Runner resolves the Collection
root, supplies member context through the native Claude/Codex configuration,
and starts managed commands from that root. Session replies include
`collection_id` and `working_directory`; native and checkpoint continuation
retain the Collection. Raw `cwd` attachment inside a managed Collection is
rejected. These are cooperative execution ownership guards, not a filesystem
security sandbox against other local processes.

Optionally associate an **existing** tracker/work record through `spec.workId`.
Only one Collection can claim that record within a managed root, including
after archive. No tracker record is filed. Without an association, the local
work identity defaults to the Collection ID. Hub-wide linkage and enforcement
across Runners remain future integration work.

## Provisioning and ownership

- A Collection supports 0–64 named worktrees, including several from one repo.
  Names are safe, case-insensitively unique directory components. There are at
  most 128 retained Collection records per root.
- All source commits are resolved before checkout. `HEAD` is the default local
  base; there is no implicit fetch. Each member gets a new branch, defaulting
  to `feature/<workspace-folder>`. An explicit `branch` must also be
  new. Existing branches are never force-checked out or reset.
- Atomic, fsynced manifests live under
  `<worktreeRoot>/.lazybox-collections/records/`, independently of agent journals.
  Retrying a request key resumes its pinned plan; changing that key's spec is
  rejected. Inspect failed attempts through `collections` or `collection`.
- Partial failures retain worktrees and error evidence. Retry reconciles Git's
  actual worktree registration and provisions missing members. Missing
  previously completed members or unexpected unfinished branch/commit state
  cause a visible failure. User files are never recursively rolled back.
- Up to 64 attached executions share a workspace. `ownerExecutionIds` retains
  every active claim; `ownerExecutionId` is the first claim for legacy readers.
  Ownership continues through managed commands and registered output producers
  after an agent closes. Releasing one claim does not release its siblings.
- `AddCollectionRepositories` appends worktrees to the existing root with an
  idempotent request key and pinned source commits. It retains existing files,
  members and ownership. Failed additions retain evidence and can be retried.
- Normal Runner shutdown stops owned execution before releasing its root lock
  and clears leases only for confirmed inactive executions. A persisted lease
  after an unclean restart becomes **uncertain**, blocking new attachment.
- `archive-collection --collection ID` is nondestructive and requires no active
  or uncertain owner. It retains branches, ignored/untracked files, and commits.

## Reconcile uncertain ownership

Automatic orphan-process discovery is not implemented. After an unclean
restart, inspect the reported `ownerExecutionId` and independently establish
that its agent and **all** its commands have stopped. Then an operator can
explicitly attest to that fact:

```sh
bin/lazybox --endpoint "$ENDPOINT" reconcile-collection \
  --collection "$COLLECTION_ID" --owner "$PREVIOUS_EXECUTION_ID" \
  --processes-stopped --reason 'Observed previous agent and commands stopped'
```

This compares the exact uncertain owner and records the reason. It is an
operator assertion, not automatic process verification or an instruction to
kill an unknown process. A current Runner's active lease cannot be cleared by
this operation.
For several uncertain claims, repeat for each reported owner. The workspace
remains blocked until all old claims have been reconciled.

## Current boundary

The backend, generated gRPC/Connect API, and CLI implement this initial slice.
Desktop Collection selection, membership editing, adopting existing branches,
clone/fetch enrollment, worktree deletion, distributed assignment, automatic
orphan reconciliation, and restoring provider conversations after Runner
reboot remain separate work. Archiving deliberately retains files.

Tests use real Git repositories in temporary directories and explicitly fake
agents for session lifecycle. The live Claude multi-repository test is present
but currently blocked by reported account capacity exhaustion. See
[runtime evidence](RUNTIME_EVIDENCE.md).

## Display names and immutable paths

Manifest version 4 records a separate `slug`. New workspace creation uses
`golang-petname` to generate three readable words, checks both manifest and disk
collisions, and creates the root once. `RenameCollection` changes only `name`;
worktree paths, branches, ownership claims and request fingerprints remain fixed.
Versions 1–3 acquire their existing folder basename as the slug when loaded. No
migration moves files. Older binaries cannot read version 4 manifests.
