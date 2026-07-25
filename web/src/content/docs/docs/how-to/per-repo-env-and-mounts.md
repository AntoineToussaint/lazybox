---
title: Per-repo env and mounts
description: Inject environment variables, symlink shared directories, and materialize scripts into worktrees.
---

Goal: make every worktree lazybox creates for a repository feel like a real
checkout — with the environment variables, shared directories, and helper
scripts your project needs.

Because each workspace lives in its own fresh git worktree, things that sit
*outside* version control (a `.env`, a `node_modules`, a local credentials dir)
are not there by default. Per-repo configuration fills that gap.

All of this lives under `repos.<owner/name>` in `~/.lazybox/config.yaml`.

## Inject environment variables

`env` is injected into every shell and agent PTY launched in that repo's
worktrees:

```yaml
repos:
  acme/widgets:
    env:
      DATABASE_URL: postgres://localhost/widgets_dev
      RUST_LOG: widgets=debug
```

## Symlink shared directories with `mounts`

`mounts` symlinks an existing directory into each worktree so you don't
re-create or re-download it per workspace. Each mount has a `source`, a
`link_at` path, and a `placement`:

```yaml
repos:
  acme/widgets:
    mounts:
      - source: /Users/me/widgets-shared/node_modules
        link_at: node_modules
        placement: inside     # symlink lives inside the worktree
      - source: /Users/me/widgets-secrets
        link_at: .secrets
        placement: above      # symlink placed above the worktree
```

- `placement: inside` — the symlink is created inside the worktree directory.
- `placement: above` — the symlink is created in the worktree's parent.

## Materialize helper scripts

`scripts` writes executables into `<worktree>/_lazybox/scripts/<name>` so agents
and shells can call them. Provide the body inline with `content`, or point at a
file with `source`:

```yaml
repos:
  acme/widgets:
    scripts:
      - name: seed-db
        content: |
          #!/usr/bin/env bash
          set -euo pipefail
          psql "$DATABASE_URL" -f db/seed.sql
      - name: lint
        source: /Users/me/widgets-shared/lint.sh
```

Each script lands at `<worktree>/_lazybox/scripts/seed-db` (and `lint`),
executable, in every worktree for that repo.

## Verify

Open a new workspace for the repo and press `s` for a shell. Then check that the
variables and files are present:

```sh
echo "$DATABASE_URL"
ls -l node_modules _lazybox/scripts
ls -l ../.secrets    # placement: above → the link lives in the worktree's parent
```

## Related

- The full [configuration reference](/docs/reference/configuration/#repos) for
  the `repos` schema.
