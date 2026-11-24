# Install Lazybox

Lazybox is a Mac app (Apple Silicon, macOS 13.5 or later). It brings everything
it runs: its background service (the **Runner**), the Claude worker and the
Node that runs it, Claude Code, and the `lazybox` command. Claude Code only
needs you to sign in: the account your `claude` command already uses, or one
you add in Settings → Subscriptions. Codex, if you use it, is installed and
signed in as usual. You also need `git`.

During the preview, Lazybox is installed [from source](#install-from-source):
one command builds the app from a clone and installs it. The disk image and
Homebrew are for [published releases](#install-a-published-release); none is
published yet, and Homebrew and in-app updates start with the first stable one. For a Runner on a Linux machine,
see [Runner on a Linux machine](#runner-on-a-linux-machine).

## Install from source

```sh
git clone https://github.com/AntoineToussaint/lazybox.git
cd lazybox
make install
```

`make install` first checks what the build needs and, for anything missing,
prints the command that installs it. Then it:

1. **Builds Lazybox** on your Mac: the app, its Runner, the `lazybox`
   command, the Claude worker and the Node it runs on. The first build takes
   5 to 10 minutes and downloads about 1 GB; later ones take a few
   minutes. It needs about 8 GB of free disk space, and no accounts, keys or
   secrets: the app is signed for this Mac only.
2. **Installs `Lazybox.app` in `/Applications`**, replacing an earlier copy.
   If Lazybox is open, it closes it first. Your agents keep running.
3. **Sets up the Runner and the `lazybox` command**, exactly as the app's
   [first launch](#first-run) does (`lazybox service install`,
   `lazybox link`), so the app opens straight to its welcome.
4. **Opens Lazybox** and prints what to do next.

**What the build needs.** `make install-check` runs only the check.

| Tool | Version | Install it with |
| --- | --- | --- |
| An Apple Silicon Mac | macOS 13.5 or later | |
| Xcode Command Line Tools | any | `xcode-select --install` |
| Go | 1.27.3 or later (`services/runner/code/go.mod`) | `brew install go`, or [go.dev/dl](https://go.dev/dl/) |
| Node.js and npm | 24 or later, npm 11 or later (`package.json`) | `brew install node`, or [nodejs.org](https://nodejs.org) |
| Rust | 1.90 or later (`rust-toolchain.toml` picks the stable compiler) | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |

`make install-check` also reports `cargo-nextest`, which working on Lazybox
needs for its Rust tests ([CONTRIBUTING.md](../CONTRIBUTING.md#set-up)) and
installing it does not: a missing one does not fail the check.

The agents themselves are not part of the build: Lazybox brings Claude Code,
and Codex is installed and signed in as usual.

**Options.**

| | |
| --- | --- |
| `make install PREFIX=~/Applications` | Install for your account only, when you cannot write to `/Applications`. Pass the same `PREFIX` to `update` and `uninstall`. |
| `make install OPEN=0` | Do not open the app at the end. |
| `make install LINK_DIR=<folder>` | Link `lazybox` into this folder instead of `/usr/local/bin` (or `~/.local/bin`). |

**Update.** A build from source does not update itself, and never offers to:
**Settings → About & updates** says *Built from source; update with make update*, with
the commit it was built from. In your clone:

```sh
make update            # git pull, then make install; your data stays
```

**Uninstall.**

```sh
make uninstall                 # the app, the Runner service, the CLI link; your data stays
make uninstall PURGE=1         # also conversations, subscriptions, workspaces and logs (asks first)
```

macOS keeps permissions such as the microphone per build of an app it cannot
verify, so after an update Lazybox may ask for them again.

## Install a published release

Building [from source](#install-from-source) is the way to install Lazybox
today. What a release adds depends on its kind:

| | A prerelease (`0.1.0-pre.1`, `0.2.0-rc.1`) | A stable release (`0.1.0`) |
| --- | --- | --- |
| On the [releases page](https://github.com/AntoineToussaint/lazybox/releases) | Yes, marked *Pre-release*: the disk image, the Linux tarballs, checksums | Yes, the same files |
| Signed and notarized by Apple | Only if the title does not say *(unsigned)*: see [Unsigned builds](#unsigned-builds) | The same |
| Homebrew (`lazybox-v2` cask, `lazybox-runner` formula) | No: the tap is not touched | Yes |
| Updates in the app | No: it offers the next *stable* release, once there is one | Yes ([Updates](#updates)) |

No release has been published yet; the first ones will be prereleases. Until
a stable release, use the disk image of a prerelease, or build from source.

**Disk image.** Download `Lazybox_<version>_aarch64.dmg` from the
[releases](https://github.com/AntoineToussaint/lazybox/releases), open it,
and drag **Lazybox** onto **Applications**. Eject the disk image, then open
Lazybox from Applications.

**Homebrew** (from the first stable release):

```sh
brew install --cask AntoineToussaint/lazybox/lazybox-v2
```

Until then this command finds no `lazybox-v2` cask: the tap holds only V1's
`lazybox` formula.

Either way, open Lazybox from Applications once afterwards: the first launch
sets up the Runner and puts the `lazybox` command on your PATH (below).

If you installed an older Lazybox with this tap's `lazybox` formula, remove
it: its `lazybox` in `/opt/homebrew/bin` comes first on your PATH, so
`lazybox` in a terminal would run it. Setup offers to do it for you.

```sh
brew uninstall lazybox
```

### Unsigned builds

A release whose title says *(unsigned)* is not signed with an Apple Developer
ID or notarized. macOS refuses to open it the first time (*"Lazybox" Not
Opened* or *Apple could not verify "Lazybox" is free of malware…*). On macOS
15 and later, Control-click → **Open** no longer gets past this. Instead:

1. Open Lazybox from Applications once, and choose **Done** when macOS refuses.
2. Open **System Settings → Privacy & Security**, scroll to **Security**, and
   choose **Open Anyway** next to *"Lazybox" was blocked…*. Confirm with your
   password or Touch ID.
3. Open Lazybox again and choose **Open Anyway** (or **Open**). macOS does not
   ask again for this copy.

Or, in a terminal, remove the quarantine macOS put on the download, then open
Lazybox as usual:

```sh
xattr -dr com.apple.quarantine /Applications/Lazybox.app
```

A new unsigned release installed from a disk image or with Homebrew asks
again. Signed releases open without asking.

## First run

The first time Lazybox opens it explains, once, what it needs to set up:

- **The Runner**, a per-user background service (a launchd agent,
  `~/Library/LaunchAgents/ai.lazybox.runner.plist`). Your agents run in it, so
  they **keep working when you close the window or quit Lazybox**. It starts
  when you log in, answers only on this Mac (loopback), and keeps its data in
  `~/Library/Application Support/Lazybox`.
- **The `lazybox` command** (optional, ticked by default): a `lazybox` link
  in `/usr/local/bin` when you can write there without a password, otherwise
  in `~/.local/bin`, plus completions for your shell. If that directory is
  not on your PATH, Lazybox says which line to add. If another `lazybox`
  comes first on your PATH, setup says what it is and how to fix it (see
  [Another `lazybox` comes first](#another-lazybox-comes-first)).

Choose **Set up Lazybox**; when it says the Runner is running, **Open
Lazybox**. Closing the window instead quits without changing anything, and
the next launch asks again.

If Lazybox was opened straight from the disk image or Downloads, it asks you
to move it to Applications first: macOS runs such a copy from a temporary
location a background service must not point at.

Where things are:

| What | Where |
| --- | --- |
| Runner state, conversations, subscriptions | `~/Library/Application Support/Lazybox/state` |
| Runner settings (`runner.config.json`) | `~/Library/Application Support/Lazybox/` |
| Workspaces' git worktrees | `~/Library/Application Support/Lazybox/workspaces` |
| Runner log | `~/Library/Logs/Lazybox/runner.log` |
| Your actions and hooks, CLI Runner list | `~/.config/lazybox/` (shared with Dev and Working) |

## Updates

Lazybox offers updates and never forces one. **Settings → About & updates** (or
**Help → Check for updates…**) shows the result of a manual check. In a
dedicated workspace, the result opens in that window. From a detached chat,
artifact, console or web window, the native menu opens it in the main window.

Only stable releases are offered: a prerelease never reaches the update
feed, and the feed itself appears with the first stable release. A build
[from source](#install-from-source) updates with `make update` instead.

- **Check automatically** (on by default) asks for a new version when
  Lazybox opens and every six hours after. A check downloads one small file
  (`latest.json` from the `v2-updates` release on GitHub) and sends nothing
  about you. Turn it off and Lazybox looks only when you choose **Check now**.
- When a version is out, the header shows one quiet line, **Lazybox x.y.z ·
  Update…**, and Settings → About & updates says so. Nothing is downloaded yet.
- **Update…** shows what changed since your version, then:
  - **Install and restart** downloads the update, checks it against the
    signing key built into your copy of Lazybox (a download that does not
    match is refused and nothing changes), replaces the app, moves the Runner
    to the same version and reopens your windows where they were.
  - **Remind me later** hides it for a day (the × on the header line does the
    same).
  - **Skip this version** never offers that version again; the next one is
    offered as usual. **Stop skipping** in Settings → About & updates undoes it.

**Running agents keep running.** Before you confirm, the dialog says how many
agents are running. Installing replaces the app on disk while every running
program keeps its own copy, then restarts only the Runner: the old one
detaches from its agents, commands and terminals, and the new one picks them
back up within seconds (see [session hosts](../SESSION_HOSTS.md)). Output
they write meanwhile is kept, and an approval that is waiting stays waiting.
If the new version could not take an agent over, that agent stops and its
conversation resumes from where it was.

After the restart, **What's new** opens once with "Updated to Lazybox
x.y.z". If an install did not finish, the next launch says so instead:
Lazybox is still on the version you had, and nothing else changed.

The app, its Runner and the `lazybox` command are one release. Settings →
Updates shows the three versions; if the Runner is on another version than
the app, it says why and offers **Move the Runner now** (`lazybox service
install`; agents keep running). Lazybox also does this by itself the next
time it starts.

Homebrew installs (from the first stable release) update the same way (the cask is marked `auto_updates`);
`brew upgrade --greedy --cask lazybox-v2` also works.

| Message in Settings → About & updates | What it means |
| --- | --- |
| *Lazybox could not reach GitHub* | No network, or GitHub is unreachable. Nothing to do; it tries again later. |
| *No update information is published* | The feed does not exist yet (no stable release has been published; prereleases never write it), or GitHub answered without it. |
| *The download is not signed with Lazybox's key* | The update was refused before anything was replaced. Try again later; if it keeps happening, send feedback. |
| *Lazybox cannot replace itself where it is installed* | The app is somewhere it cannot write (not in Applications, or another user's copy). Move it to Applications, or install from the disk image. |

## Send feedback

**Help → Send feedback…** writes a report you read before it goes anywhere.
Lazybox has no telemetry and uploads nothing.

- **What happened**, in your words.
- **A screenshot of this window** (ticked when available): the window as it
  was when you chose Send feedback.
- **Diagnostics** (ticked): the app and Runner versions, macOS, and the last
  200 lines of the Runner's log. Tokens, keys and passwords are removed,
  e-mail addresses too, and paths in your home folder read `~`.
- **The open conversation** (not ticked): its title, agent, model, state and
  last error. Its messages are never included.

The right side shows exactly what will be shared. Then:

- **Save to file** writes `feedback.md` (and `screenshot.png`) to a new
  folder in Downloads, *Lazybox feedback <date and time>*.
- **Copy** puts the report on the clipboard.
- **Open a GitHub issue…** saves the report, then opens a new issue on
  [AntoineToussaint/lazybox](https://github.com/AntoineToussaint/lazybox/issues)
  in your browser, filled in with it. Read it, attach the saved files if you
  like, and post it yourself.

## The `lazybox` command

Everything the app does is also a command; `lazybox` with no arguments lists
them. For the installation itself:

```sh
lazybox --version                # the release; app, CLI and Runner match
lazybox doctor                   # check everything, with the fix for anything wrong
lazybox service status           # is the Runner installed, running, answering
lazybox service logs [-f]        # the Runner's log
lazybox service restart          # a fresh Runner; agents keep running
lazybox service stop | start     # stop until the next login, or start now
lazybox link                     # put lazybox on PATH (+ completions), again
lazybox link --remove-older      # remove an older Lazybox from Homebrew (asks first)
```

### Another `lazybox` comes first

Your terminal runs the first `lazybox` on your PATH. When that is not this
Lazybox's, setup and `lazybox doctor` say what it is, where, and the fix:

- **An older Lazybox, installed by Homebrew** (the `lazybox` formula, in
  `/opt/homebrew/bin`). Remove it with `brew uninstall lazybox`. Setup offers
  **Remove the older Lazybox…**, which shows you that exact command, runs it
  only once you confirm, and shows what Homebrew printed;
  `lazybox link --remove-older` does the same from a terminal. Or put the
  directory Lazybox linked into before `/opt/homebrew/bin` in your shell's
  PATH.
- **An older Lazybox installed another way** (`cargo install`, say). Remove
  the file it names, or fix the PATH order.
- **A version manager's shim** (mise, asdf…). There is nothing to uninstall:
  put the directory Lazybox linked into before the shims directory, after the
  line in your shell's startup file that sets the manager up, or stop the
  manager providing a `lazybox`.
- **Another program, or another copy of Lazybox.** Fix the PATH order, or
  rename or remove it; for another copy, `lazybox link --dir <its directory>`
  run from this copy points that link here.

Lazybox tells these apart by where the file lives (a Homebrew Cellar, an app,
a shims directory) and otherwise by asking it for `--version`, which it gives
three seconds. It removes nothing by itself.

`lazybox service install` is what the first run does; it also repairs a
service, refreshes the PATH agents get (read from your login shell), and moves
the service to the release it belongs to. `lazybox service plan` prints the
launchd job it would write.

From your own terminal, `lazybox run -- <command>` and `cmd | lazybox log`
find the Runner by themselves and publish to the workspace you are in (run
them inside a workspace's folder or worktree), or to `--collection <name>`.

## Troubleshooting

Start with:

```sh
lazybox doctor
```

It checks the release, the service, the endpoint, that app, CLI and Runner
are the same version, the bundled Node and Claude worker, Claude Code, Codex,
`git` and `gh` on the PATH agents get, your GitHub connection, whether Claude
Code and Codex are signed in, free disk space, your login shell's PATH (and
whether agents have all of it), and which `lazybox` your terminal runs, and
prints the fix next to anything that is not right. The app's setup
window runs the same checks (**Run diagnostics**) when it cannot reach the
Runner.

| Symptom | What to do |
| --- | --- |
| "Lazybox can't reach its Runner" | **Start the Runner**; if it stops again, **Show log** (or `lazybox service logs`). |
| An agent cannot find `codex`, `gh`, `npm`… | The Runner's PATH comes from your login shell when the service was installed. Fix your shell's PATH, then `lazybox service install`. |
| `lazybox` runs something else | Another `lazybox` comes first on your PATH: see [Another `lazybox` comes first](#another-lazybox-comes-first). |
| Versions differ in Settings → About & updates | **Move the Runner now** there, or quit and reopen Lazybox: it moves the Runner to its own version. Or `lazybox service install`. Agents keep running. |
| macOS will not open the app | An unsigned build: see [Unsigned builds](#unsigned-builds). |

Logs: `~/Library/Logs/Lazybox/runner.log` (the Runner),
`~/Library/Application Support/Lazybox/state/hosts/*/host.log` (each agent's
session host).

## Uninstall

```sh
lazybox uninstall --dry-run     # what would be removed
lazybox uninstall               # the service, the CLI link, completions
lazybox uninstall --remove-data # also conversations, subscriptions, workspaces, logs (asks first)
```

Then drag Lazybox from Applications to the Trash (or, for the Homebrew cask,
`brew uninstall --cask lazybox-v2`; `brew uninstall --zap` also deletes your
data). Without `--remove-data` your conversations and settings stay, so a
reinstall picks up where you left off. `--remove-data` deletes the
workspaces' worktrees too: anything uncommitted or unpushed in them is lost,
which is why it asks.

Agents still running when the service is removed stop by themselves within a
day, or are re-attached if you install again before then.

## Runner on a Linux machine

For remote use: a Runner on a Linux server, VM or cloud box, so agents keep
working there with your laptop closed. The desktop app connects to it once the
SSH link ships ([REMOTE_RUNNERS.md](../REMOTE_RUNNERS.md)); until then you use
it with the `lazybox` command on that machine. There is no Linux desktop app.

**Needs:** Linux on x64 or arm64 with glibc 2.28 or newer (Ubuntu 20.04,
Debian 11, RHEL 8 and later), the libraries the bundled Node needs
(`sudo apt install libatomic1` on Debian and Ubuntu, `sudo dnf install
libatomic` on Fedora and RHEL), and `git`. systemd is what starts the Runner
at boot; without it (a container) the Runner runs, but you start it.

**Add a computer** (Settings → Runners, or `lazybox remote add <ssh-host>`)
does all of this over SSH ([guide](guides/remote-computers.md)). By hand:

**From source.** From a Lazybox checkout, run `make install-check` and then
`make install`. The installer builds the Runner, CLI, Claude worker, and web
client without Xcode or Rust. It installs the runtime under
`~/.local/lib/lazybox`, registers and starts the Runner under your user account,
and links the CLI to your PATH. Use `PREFIX=/absolute/directory` to change the
runtime's parent directory, and `LINK_DIR=/absolute/directory` to choose where
to link the CLI. On Ubuntu, install `make`, `git`, and `libatomic1` using
APT, then use the Go and Node.js versions reported by `make install-check`.
`make update` rebuilds after a fast-forward pull. `make uninstall` stops the
Runner and removes its runtime and links while keeping your data. Pass the same
`PREFIX` if you installed to a custom directory.

**Homebrew** (from the first stable release; until then the tap has no
`lazybox-runner`). Install the formula from the Lazybox tap and register the
service under your user account:

```sh
brew install AntoineToussaint/lazybox/lazybox-runner
lazybox service install
lazybox doctor
```

After a Homebrew upgrade, run `lazybox service install` again to refresh the
service. For a prerelease, use the tarball below.

**Tarball.** Download the tarball for your architecture (`x64` or `arm64`)
and its checksum from the
[releases](https://github.com/AntoineToussaint/lazybox/releases), check it,
unpack it, and set up the service:

```sh
v=0.1.0-pre.1 arch=x64
sha256sum -c lazybox-runner-$v-linux-$arch.tar.gz.sha256
root=${XDG_DATA_HOME:-$HOME/.local/share}/lazybox/runner
mkdir -p "$root/versions/$v"
tar -xzf lazybox-runner-$v-linux-$arch.tar.gz -C "$root/versions/$v"
(cd "$root/versions/$v" && sha256sum --quiet -c SHA256SUMS)
ln -sfn "versions/$v" "$root/current"
"$root/current/bin/lazybox" link     # lazybox in ~/.local/bin, with completions
lazybox service install              # the Runner as a systemd user service
lazybox doctor
```

**Keep it running after you log out.** A systemd user service runs while you
are logged in, unless lingering is on. Turn it on once (some systems need
`sudo`); then the Runner also starts at boot:

```sh
loginctl enable-linger "$USER"
```

`lazybox doctor` says whether it is on (**Lingering**). Without it, the
Runner and its agents stop when your last session on that machine ends: their
scopes belong to your user manager, which stops then.

**What runs where.**

| | |
| --- | --- |
| Service | `~/.config/systemd/user/lazybox-runner.service`: `systemctl --user status lazybox-runner` |
| Logs | the journal: `lazybox service logs` (`-f` to follow), or `journalctl --user -u lazybox-runner` |
| State, configuration, workspaces | `~/.local/share/lazybox` (`state/`, `runner.config.json`, `workspaces/`) |
| Each agent's session host | `~/.local/share/lazybox/state/hosts/*/host.log`; in systemd as `lazybox-host-*.scope` |

**Agents outlive the Runner** here too: `lazybox service restart`, an update
or a crash leaves agents, commands and terminals running, and the next Runner
re-attaches them. Each runs in a systemd scope of its own, outside the
service, which a restart would otherwise take down with it
([SESSION_HOSTS.md](../SESSION_HOSTS.md#on-linux)).

**Without systemd** (a container, WSL without systemd), `lazybox service
install` runs the Runner under Lazybox's own supervisor, which starts it again
whenever it exits; its log is `~/.local/state/lazybox/runner.log`. Agents then
outlive a Runner restart, not a restart of the container, and nothing starts
the Runner when the container starts: run `lazybox service start`. Give the
container an init that reaps exited processes (`docker run --init`, or
`"init": true` in `devcontainer.json`).

**Claude Code and Codex** are not in the tarball: install and sign in to them
on that machine, in its own shell. Claude Code with
[Anthropic's installer](https://docs.anthropic.com/en/docs/claude-code), then
`claude` and `/login`; Codex with `npm i -g @openai/codex`, then
`codex login --device-auth`. Run `lazybox service install` afterwards so the
Runner's PATH includes them.

**Update.** Unpack the new version beside the old one as above, then run its
`lazybox service install` (`"$root/versions/<new>/bin/lazybox" service
install`) and `lazybox link`. Running agents are kept.

**Uninstall.** `lazybox uninstall` (add `--remove-data` to delete state and
workspaces), then delete `~/.local/share/lazybox/runner`.

## Dev and Working

The installed app is separate from the [Dev and Working environments](../ENVIRONMENTS.md)
used to develop Lazybox: its own app identity (`ai.lazybox.app`), state,
Runner and launchd job. `lazybox runners` lists every Runner the CLI can
reach; with several, it uses the installed one, then Working.
