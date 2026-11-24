# Runners on your other computers

> **Preview.** Add a computer you reach with `ssh` — a Linux box, a cloud VM,
> another Mac — and work on it from this window as on this Mac. Its agents keep
> working while your laptop sleeps.

A *Runner* is the service that runs your agents. Every computer you add runs
its own Runner, with its own workspaces, worktrees, sign-ins and agents. This
window shows it beside the Runners on this Mac (see
[Several Runners in one window](several-runners.md)), and when you open one of
its workspaces the whole page is that computer's: chat, files, changes and
review, terminals, command output, settings.

## Before you start

- You can `ssh <host>` from a terminal on this Mac **without typing a
  password**: an SSH key, an agent (1Password, Secretive), Tailscale SSH. Lazybox
  uses your own `ssh` and `~/.ssh/config`, so `Include`, `ProxyJump` and
  every alias you have work as they do in your terminal.
- Connect once in a terminal first, so the host's key is in `known_hosts`.
- The computer runs Linux (glibc 2.28 or later, x64 or arm64) or macOS.

## Add a computer

**Settings → Runners → Add a computer…** lists the hosts of your
`~/.ssh/config`. Type to filter, or type `user@host` for one that is not in it;
arrow keys move, **Return** chooses.

Lazybox then does, one step after the other:

1. **Connect** to it with your `ssh`.
2. **Check the host**: its system and CPU, free disk, whether Lazybox is there,
   and whether Claude Code, Codex and git are. Nothing changes yet.
3. **Upload and verify** the Runner when the computer has none (or an older
   one). The computer checks the SHA-256 this Mac computed before it unpacks
   anything, and keeps the previous version for rollback, in
   `~/.local/share/lazybox/runner/`.
4. **Start the Runner** there, as a service: a systemd user service on Linux
   (Lazybox's own supervisor where there is no systemd, as in a container), the
   launchd service on a Mac. Agents there keep running when it restarts or
   updates. The step says whether it also starts at boot: on Linux that needs
   lingering, and the step names the command when it is off.
5. **Add this Mac as owner**: this Mac's own key is added to that Runner's
   owners, and this Mac remembers (pins) that Runner's key.
6. **Connect over the link**, to check it all works.

Each step shows a tick, or what went wrong in plain words with what to do
(*build-box refused the SSH connection: is SSH (Remote Login) turned on
there?*). Closing the sheet does not stop it; a notice says when it is done.

From a terminal it is one command:

```sh
lazybox remote add build-box        # a Host from ~/.ssh/config, or user@host
lazybox remote ls                   # the computers added, and how each link is
lazybox --runner build-box ls       # any lazybox verb, on that computer's Runner
lazybox remote rm build-box         # forget it here, and revoke this Mac's key there
```

## In the window

- **The sidebar groups Runners by computer**: *This Mac* (this window's own
  Runner, and Dev, Working or the installed app's if you chose to show them),
  then each computer you added, with how it is reached (*SSH*). A computer you
  added shows without asking.
  The round trip shows when it is slow (over 300 ms), and always when you rest
  the pointer on the computer's name.
- **When the link drops** (your Mac slept, you changed Wi-Fi, the computer
  restarted) the header says *Reconnecting…* and Lazybox connects again by
  itself, usually within seconds. The agents on that computer never noticed:
  they run there.
- **Open a workspace** of that computer and the page is its Runner, exactly as
  for a Runner on this Mac. Actions that act on this Mac's own files (*Reveal in
  Finder*, *Open in Finder* or an editor for a home checkout) are not offered
  there, because the files are on the other computer; *Open in terminal* gives
  you a shell on it.
- **Settings → Runners** shows each computer's route, round trip and pinned key
  (`SHA256:…`), and **Remove**, which forgets it here and revokes this Mac's key
  on it. Its workspaces and agents stay on that computer.

## Sign-ins on that computer

Claude Code, Codex and GitHub are signed in **on that computer**, as on this
Mac: in its Settings → Agents and Settings → GitHub, or in a terminal there.
Lazybox never copies a sign-in, token or key from this Mac to it.

## How it is kept safe

- **No new port.** The Runner on that computer opens nothing to the network.
  The link runs inside your SSH connection (`ssh build-box lazybox link serve
  --stdio`), so only someone who can log in as you there can even try it.
- **Its own key per computer.** This Mac has a key of its own (in
  `~/.config/lazybox/keys/`, readable only by you). That computer keeps the
  keys it accepts in its owners list; `lazybox link owners ls` there shows
  them, and `lazybox link owners revoke -- <key>` cuts one off: its links end
  within a second. (A key ID may begin with `-`; the `--` keeps it from being
  read as an option.)
- **The other Runner's key is pinned.** If the name ever points at another
  computer, or the Runner was replaced, Lazybox says *This is not build-box's
  Runner* and sends nothing.
- **The Runner's own password never leaves that computer.** Each connection
  proves this Mac's key and is encrypted end to end (Noise), inside SSH; the
  Runner's local password is added on that computer only.
- **An activity log.** Connections, refusals and keys added or revoked are
  recorded on that computer in `audit.jsonl` in its state directory.

## Not yet

- On a Linux computer without lingering (`loginctl enable-linger $USER`
  there), the Runner and its agents stop when you are no longer logged in to
  it, a few minutes after Lazybox's own SSH connection ends; with lingering
  they keep running and start at boot. In a container, nothing starts the
  Runner when the container starts: **Add a computer** again, or
  `lazybox service start` there.
- Previewing an app that runs on that computer (port forwarding), *Open in
  editor* with a remote editor URI, and windows of their own for that computer's
  chats come next.
- The keys are in a private file; the Keychain and the Secure Enclave come next.
