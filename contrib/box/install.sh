#!/usr/bin/env bash
#
# Build lazybox on this box at a specific commit and bring its daemon up
# under systemd — so a provisioned box runs a daemon whose wire fingerprint
# matches the client that stamped it (build-parity by construction, #977).
#
# Run as root by the `lazybox-build.service` oneshot the startup script
# installs, and again by `lazybox sandbox rebuild` to move the box to a new
# commit:
#
#   lazybox-box-install.sh [<git-sha>]
#
# The optional argument overrides LAZYBOX_GIT_SHA (the rebuild path). Every
# other input comes from the environment (the build unit reads
# /etc/lazybox/box.env):
#   LAZYBOX_SRC       repo checkout to build   (default /opt/lazybox/src)
#   LAZYBOX_GIT_SHA   commit to build          (default: leave HEAD as-is)
#   LAZYBOX_USER      account the daemon runs as (default lazybox)
#
# Idempotent: once the recorded build SHA matches the checkout's HEAD and the
# binary exists, it re-asserts the systemd wiring and exits without rebuilding,
# so re-running it on every boot is cheap after the first ~10-minute build.

set -euo pipefail

LAZYBOX_SRC="${LAZYBOX_SRC:-/opt/lazybox/src}"
LAZYBOX_USER="${LAZYBOX_USER:-lazybox}"
SHA_ARG="${1:-}"
TARGET_SHA="${SHA_ARG:-${LAZYBOX_GIT_SHA:-}}"

BUILD_SHA_FILE="/etc/lazybox/build-sha"
BIN_DST="/usr/local/bin/lazybox"
# rustup/cargo shared under /opt so both the build unit and a later rebuild
# reuse one toolchain rather than each re-installing into a per-user home.
export RUSTUP_HOME="${RUSTUP_HOME:-/opt/rust/rustup}"
export CARGO_HOME="${CARGO_HOME:-/opt/rust/cargo}"

log() { echo "lazybox-box-install: $*" >&2; }

# ── Move the checkout to the requested commit ──────────────────────────
if [ -n "$TARGET_SHA" ]; then
  # Best-effort fetch: the first build already checked the commit out, so a
  # rebuild without network (or without a private-repo token) still builds
  # whatever HEAD is rather than aborting.
  git -C "$LAZYBOX_SRC" fetch origin "$TARGET_SHA" 2>/dev/null \
    || git -C "$LAZYBOX_SRC" fetch origin 2>/dev/null || true
  git -C "$LAZYBOX_SRC" checkout --quiet --detach "$TARGET_SHA"
fi
HEAD_SHA="$(git -C "$LAZYBOX_SRC" rev-parse HEAD)"

install -d -m0755 /etc/lazybox

# ── Build (skipped when already at this commit) ────────────────────────
if [ "$(cat "$BUILD_SHA_FILE" 2>/dev/null || true)" = "$HEAD_SHA" ] && [ -x "$BIN_DST" ]; then
  log "already built $HEAD_SHA — re-asserting systemd wiring only"
else
  log "installing build prerequisites"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -y
  # ghostty's VT build (via `make setup`'s pinned zig) needs a C++ toolchain
  # and libc++; the rest are the client-side build deps from scripts/bootstrap.sh.
  apt-get install -y build-essential clang libc++-dev libc++abi-dev \
    pkg-config cmake curl git xz-utils ca-certificates

  if [ ! -x "$CARGO_HOME/bin/cargo" ]; then
    log "installing rust toolchain"
    install -d -m0755 /opt/rust
    curl -fsSL https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal
  fi
  export PATH="$CARGO_HOME/bin:$PATH"

  log "building lazybox at $HEAD_SHA (this can take 10+ minutes)"
  make -C "$LAZYBOX_SRC" setup
  make -C "$LAZYBOX_SRC" release
  install -m0755 "$LAZYBOX_SRC/target/release/lazybox" "$BIN_DST"
  echo "$HEAD_SHA" >"$BUILD_SHA_FILE"
  log "installed lazybox $HEAD_SHA to $BIN_DST"
fi

# ── Daemon account ─────────────────────────────────────────────────────
if ! id -u "$LAZYBOX_USER" >/dev/null 2>&1; then
  log "creating daemon user $LAZYBOX_USER"
  useradd --create-home --shell /bin/bash "$LAZYBOX_USER"
fi
USER_HOME="$(getent passwd "$LAZYBOX_USER" | cut -d: -f6)"
install -d -m0700 -o "$LAZYBOX_USER" -g "$LAZYBOX_USER" \
  "$USER_HOME/.lazybox" "$USER_HOME/.lazybox/run"

# ── systemd units: daemon on boot + stop-on-idle ───────────────────────
# The unit files ship in the repo (contrib/systemd, contrib/box-lifecycle);
# this only copies + enables them (#903, #913).
install -m0644 "$LAZYBOX_SRC/contrib/systemd/lazybox-daemon@.service" \
  /etc/systemd/system/lazybox-daemon@.service
install -m0755 "$LAZYBOX_SRC/contrib/box-lifecycle/lazybox-idle-stop.sh" \
  /usr/local/bin/lazybox-idle-stop.sh
install -m0644 "$LAZYBOX_SRC/contrib/box-lifecycle/lazybox-idle-stop.service" \
  /etc/systemd/system/lazybox-idle-stop.service
install -m0644 "$LAZYBOX_SRC/contrib/box-lifecycle/lazybox-idle-stop.timer" \
  /etc/systemd/system/lazybox-idle-stop.timer

# Refresh the installer copy so the next rebuild runs this checkout's script.
install -m0755 "$LAZYBOX_SRC/contrib/box/install.sh" /usr/local/bin/lazybox-box-install.sh

# Let `lazybox sandbox rebuild` re-run the installer over SSH as the daemon
# user — a single, fixed command, so the grant is narrow.
cat >/etc/sudoers.d/lazybox-rebuild <<SUDOERS
$LAZYBOX_USER ALL=(root) NOPASSWD: /usr/local/bin/lazybox-box-install.sh
SUDOERS
chmod 0440 /etc/sudoers.d/lazybox-rebuild

systemctl daemon-reload
systemctl enable --now "lazybox-daemon@$LAZYBOX_USER.service"
systemctl enable --now lazybox-idle-stop.timer
# A rebuild swapped the binary under a running daemon — restart so the box
# serves the freshly-built commit.
systemctl restart "lazybox-daemon@$LAZYBOX_USER.service"

log "daemon lazybox-daemon@$LAZYBOX_USER is up (socket $USER_HOME/.lazybox/run/daemon.sock)"
