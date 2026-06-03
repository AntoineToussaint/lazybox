# Example configs

Copy-paste starter templates for `~/.lazybox/config.yaml`. They double as living
docs for the parts of the config the setup wizard does not write for you.

## Before you copy anything

lazybox writes most of `~/.lazybox/config.yaml` itself. On first run it walks you
through a setup wizard; later you press `,` to reopen the settings palette to
add repo scopes, pick the default agent, and manage detected editors. You
rarely need to hand-edit those parts.

These templates cover the bits the wizard does **not** manage:

- per-repo environment variables, mounts, and helper scripts,
- the optional Slack mirror,
- autonomous-agent permission flags.

Credentials never live in this file. lazybox resolves a GitHub token at runtime
via the credential chain (`GH_TOKEN` → `GITHUB_TOKEN` → `gh auth token`); run
`gh auth login` once. For Linear, export `LINEAR_API_KEY`.

For the full list of keys and defaults, see the configuration reference:
<https://lazybox.ai/docs/reference/configuration/>.

## How to use a template

Each file is a complete, valid `config.yaml`. Two ways to apply one:

- **Bootstrap from scratch** — copy the whole file into place:

  ```bash
  cp examples/config.minimal.yaml ~/.lazybox/config.yaml
  ```

- **Merge into an existing config** — open the template, copy just the block
  you want (e.g. the `slack:` key or a single `repos.<owner/name>` entry), and
  paste it into your current `~/.lazybox/config.yaml`. YAML keys at the top level
  merge by name; don't duplicate a top-level key like `repos:` twice.

After editing, restart lazybox (or run it fresh) to pick up the changes.

## The templates

| File | What it does |
| --- | --- |
| [`config.minimal.yaml`](config.minimal.yaml) | The handful of keys it's reasonable to set by hand (poll interval, log path, default agent). Which repos lazybox watches is managed by the wizard, not this file. |
| [`config.per-repo-env.yaml`](config.per-repo-env.yaml) | Per-repo `env`, `mounts`, and `scripts`: inject `DATABASE_URL`/`OPENAI_API_KEY` into worktrees, symlink a shared data dir, and materialize a cleanup script. |
| [`config.slack-mirror.yaml`](config.slack-mirror.yaml) | The optional `slack:` block (bot/app tokens, anchor channel, per-workspace channels). Easiest set up via `lazybox slack init`. |
| [`config.autonomous-agent.yaml`](config.autonomous-agent.yaml) | The `agent:` flags for autonomous runs, with notes on the worktree-bounded blast radius. |
| [`rust-cleanup.sh`](rust-cleanup.sh) | A real, executable cleanup script for Rust workspaces, ready to wire into `repos.<owner/name>.scripts[].source`. |

## Notes on a few keys

- **`repos.<owner/name>.env`** is injected into every shell and agent terminal
  in that repo's worktrees. Prefer references to a secret manager over pasting
  real secrets.
- **`mounts`** symlink shared directories into fresh worktrees. `placement:
  inside` links under the worktree; `placement: above` links into the
  worktree's parent, shared across sibling worktrees of the same repo.
- **`scripts`** land at `<worktree>/_lazybox/scripts/<name>` in every worktree.
  Use `content:` for inline scripts or `source:` to pull from a file on disk.
- **`agent.autonomous_skip_permissions`** runs autonomous Claude work with
  `--dangerously-skip-permissions`. The blast radius is bounded to the task's
  worktree, but the agent can still push branches and open PRs — review its
  output.

See [`CLAUDE.md`](../CLAUDE.md) and [`DESIGN.md`](../DESIGN.md) in the repo root
for the architecture behind worktree-per-workspace, agent-per-workspace, and
the reactive event bus.
