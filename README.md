<div align="center">

# 📥 lazybox

**Run a fleet of coding agents from your terminal — and step in when it
matters.** Every task gets its own isolated git worktree and a live embedded
terminal for Claude Code, Codex, Cursor, or a shell, so you can spin up and
juggle many agents without ever managing worktrees by hand. Useful even with a
quiet GitHub.

Wire up GitHub or Linear and it's also a **reactive inbox**: instead of
refreshing, events flow to you — new comments, CI failures, and review requests
surface as they land, with per-row read/unread tracking.

Think a TUI inbox (lazygit-style) where every row is also a ready-to-run
workspace — built for developers juggling many PRs and AI coding agents at once.

[![Latest release](https://img.shields.io/github/v/release/AntoineToussaint/lazybox?logo=github&label=release&color=6f42c1)](https://github.com/AntoineToussaint/lazybox/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/AntoineToussaint/lazybox/ci.yml?branch=main&logo=githubactions&logoColor=white&label=CI)](https://github.com/AntoineToussaint/lazybox/actions/workflows/ci.yml)
[![Homebrew](https://img.shields.io/badge/brew-AntoineToussaint%2Flazybox-FBB040?logo=homebrew&logoColor=white)](https://github.com/AntoineToussaint/homebrew-lazybox)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-CE412B?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-macOS%20%C2%B7%20Linux-555?logo=apple&logoColor=white)](#install)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Docs](https://img.shields.io/badge/docs-lazybox.ai-0a7?logo=readthedocs&logoColor=white)](https://lazybox.ai/docs/)

</div>

<div align="center">

<video src="https://github.com/user-attachments/assets/c3f11255-5cba-4904-8c58-299ebfcccef6" poster="demo/hero.png" controls muted autoplay loop playsinline width="900">
  <img src="demo/hero.gif" alt="lazybox: the inbox on the left listing live PRs and issues across repos, an opened workspace with description, activity, and embedded agent terminals on the right" />
</video>

</div>

<sub>Video not playing? Here's the [animated GIF](demo/hero.gif) and a [static screenshot](demo/hero.png). There's also a fully reproducible `--test` demo — code, not a recording — driven by [`demo/lazybox.tape`](demo/lazybox.tape).</sub>

## ✨ Highlights

- **📨 Reactive inbox** — new comments, CI failures, and review requests surface automatically, with per-row read/unread tracking. No refreshing.
- **🌳 A worktree per task** — every row opens an isolated git worktree, so PRs never step on each other's working trees.
- **🤖 Agents built in** — spawn Claude Code, Codex, or Cursor in a row's worktree with one key; `w` picks the right prompt for the row's state (fix CI / address comments / implement issue).
- **🖥️ Embedded terminals** — a live PTY per workspace (split & tile them), powered by a vendored ghostty VT parser.
- **🔌 Source-agnostic** — GitHub and Linear today, surfacing in one inbox behind the same interface, with an optional Slack mirror.
- **🛰️ Remote-friendly** — a client/daemon split runs over an SSH-forwarded socket for working against a remote box.

## Install

Prebuilt binaries (macOS · Linux):

```sh
brew install AntoineToussaint/lazybox/lazybox
# …or: curl --proto '=https' --tlsv1.2 -LsSf \
#   https://github.com/AntoineToussaint/lazybox/releases/latest/download/lazybox-tui-installer.sh | sh
```

Then `gh auth login` (if you haven't) and run `lazybox`.

Prefer to build it yourself, or hacking on lazybox? Build from source:

```sh
git clone https://github.com/AntoineToussaint/lazybox.git
cd lazybox
make setup     # one-shot: downloads pinned zig 0.15.2 to ~/.cache/lazybox/zig/
make run       # build + run
```

**Prerequisites:** Rust 1.85+, a C compiler (for bundled SQLite), the
[GitHub CLI](https://cli.github.com/) logged in (`gh auth login` — lazybox reads
`gh auth token`), and network access to github.com on the first build.

Linux also needs a C/C++ toolchain plus **libc++ and libc++abi** — the embedded
ghostty terminal is built by zig against LLVM's libc++ (not GNU libstdc++), so
the link step fails without them. `make setup` checks for these and points you
at the right command for your distro:

```sh
# Debian/Ubuntu
sudo apt install build-essential pkg-config libc++-dev libc++abi-dev
# Fedora/RHEL
sudo dnf install gcc gcc-c++ pkgconf-pkg-config libcxx-devel libcxxabi-devel
# Arch
sudo pacman -S --needed base-devel pkgconf libc++ libc++abi
```

Full install options (Homebrew, `curl | sh`, source), build notes, and
troubleshooting are in the [Quickstart](https://lazybox.ai/docs/tutorials/quickstart/).

## First 60 seconds

Want to see the UI immediately, with no GitHub account and nothing to configure?

```sh
lazybox --test     # throwaway tempdir repo + one seeded workspace, no GitHub
```

You land on the inbox. Then:

```
↑ / ↓     move between workspaces   (j / k also works)
Enter     open the selected workspace
c         spawn a Claude Code session in its worktree   (s for a plain shell)
]]        back to the inbox
```

`c` needs the `claude` CLI on your `PATH`; `s` (a plain shell) always works.

That's the whole model in one screen: the workspace got an **isolated git
worktree** and a **live embedded terminal**, and you never left the inbox.
Run `lazybox` (no `--test`) to do the same against your real PRs — the first
launch walks you through a short setup wizard. Run `lazybox --help` any time for
an orientation of every command.

## Documentation

📖 **[lazybox.ai/docs](https://lazybox.ai/docs/)** — organized by what you're trying to do:

- **[Quickstart](https://lazybox.ai/docs/tutorials/quickstart/)** — install → run → your first win, in ~5 minutes.
- **[How-to guides](https://lazybox.ai/docs/how-to/)** — add a repo, run an agent per workspace, per-repo env/mounts, remote over SSH, mirror to Slack.
- **[Reference](https://lazybox.ai/docs/reference/)** — every [CLI command](https://lazybox.ai/docs/reference/cli/), the full [keybindings](https://lazybox.ai/docs/reference/keybindings/), and the [`~/.lazybox/config.yaml`](https://lazybox.ai/docs/reference/configuration/) schema.
- **[Explanation](https://lazybox.ai/docs/explanation/)** — the [mental model](https://lazybox.ai/docs/explanation/mental-model/) (worktree- and agent-per-workspace) and the [architecture](https://lazybox.ai/docs/explanation/architecture/).

Copy-paste config starters live in [`examples/`](examples/). Deep architecture
notes are in [`CLAUDE.md`](CLAUDE.md) and [`DESIGN.md`](DESIGN.md); the
per-feature dev catalog — what each piece does, where it lives, and how to test
it — is in [`docs/features/`](docs/features/).

## Essential keys

The bottom hint bar always shows what's available in the focused pane; press `?`
for the full overlay. lazybox is also fully mouse-driven — click a pane or row
to focus it, drag the splitters to resize, wheel-scroll, and right-click links
(or rows) for the context menu. The keys you'll reach for first:

| Key | Action |
|---|---|
| `Tab` | Cycle Sidebar → Activity → Terminals |
| `↑` / `↓` · `Enter` | Navigate the inbox (`j` / `k` also works) · open a workspace |
| `c` / `x` / `u` · `s` | Spawn Claude / Codex / Cursor · spawn a shell (`c` needs the `claude` CLI on `PATH`; `s` always works) |
| `w` | "Work" — spawn Claude with the right prompt for the row's state (fix CI / address comments / implement issue) |
| `m` · `r` | Mark read · reply |
| `g` | GitHub menu (which-key popup): `g m` merge · `g v` reviewers · `g a` assignees · `g l` labels · `g o` open in browser (`Shift-M` / `Shift-V` / `Shift-L` / `Shift-O` are direct aliases) |
| `,` · `?` · `q q` | Settings · help · quit |
| `]]` | Leave a terminal, back to the sidebar |

The [full keybinding reference](https://lazybox.ai/docs/reference/keybindings/) covers every pane.

## Status

Pre-1.0, **early-adopter dev mode**. Daily-driver for the author on macOS; Linux
runs the same code paths but gets less testing. Expect sharp edges, log spam in
`/tmp/lazybox.log`, and the occasional breaking change. Prebuilt binaries ship
via the Homebrew tap, the `curl | sh` installer, and GitHub Releases (see
[Install](#install)).

Run a side-by-side dev instance against its own state with `make dev`
(`LAZYBOX_HOME=~/.lazybox-dev`) if you want to try lazybox without disturbing your
main inbox.

## Contributing & support

Issues and PRs welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) for the build
loop and standing rules (tests with every change; the four core libraries stay
dependency-free of each other). Questions, bugs, and feature ideas:
[`SUPPORT.md`](SUPPORT.md) points you at
[Discussions](https://github.com/AntoineToussaint/lazybox/discussions) and the
[issue templates](https://github.com/AntoineToussaint/lazybox/issues/new/choose).

## License

MIT — see [`LICENSE`](LICENSE).
