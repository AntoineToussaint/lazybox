#!/usr/bin/env bash
#
# Build (or rebuild) the lazybox daemon on a sandbox box at a pinned commit
# and (re)start it under systemd — the on-box half of build-parity (#977).
#
# So a client and the box's daemon share a commit and the wire-fingerprint
# handshake passes by construction. Reused by two callers:
#   - the boot-time transient unit (terraform/sandbox/gcp/startup.sh.tftpl)
#     runs it once, configured through /etc/lazybox/build.env;
#   - `lazybox sandbox rebuild` runs it over IAP SSH with the client's new
#     SHA as $1, recovering from a fingerprint mismatch WITHOUT a reboot
#     (changing GCE metadata does not re-run the startup script on a live box).
#
# Must run as root: it installs to /usr/local/bin + /etc/systemd/system and
# drives systemctl. The compile itself runs unprivileged as $LAZYBOX_USER.
#
# Idempotent: re-running rebuilds the same (or a new) commit and restarts the
# daemon; apt/rustup/`make setup` all skip work already done.
set -euo pipefail

LAZYBOX_USER="${LAZYBOX_USER:-lazybox}"
SRC_DIR="${LAZYBOX_SRC_DIR:-/opt/lazybox/src}"
REPO_URL="${LAZYBOX_REPO_URL:-https://github.com/AntoineToussaint/lazybox}"
# Target commit: an explicit arg (the rebuild path) wins over the boot-time
# EnvironmentFile. Empty means track the default branch tip — a client with
# no baked SHA (e.g. a release tarball).
TARGET_SHA="${1:-${LAZYBOX_TARGET_SHA:-}}"
BIN_DEST="${LAZYBOX_BIN_DEST:-/usr/local/bin/lazybox}"
SHA_FILE="${LAZYBOX_SHA_FILE:-/etc/lazybox/build-sha}"

log() { echo "lazybox-build: $*" >&2; }

# Run a command as the unprivileged build user with a login shell so its
# rustup/cargo (installed under ~/.cargo) are on PATH.
as_build() { sudo -u "$LAZYBOX_USER" -H bash -lc "$1"; }

# 1. Rust toolchain for the build user (idempotent — skip when cargo present).
if ! as_build 'command -v cargo >/dev/null 2>&1'; then
  log "installing rustup for $LAZYBOX_USER"
  as_build 'curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable'
fi

# 2. Source at the target commit (clone if the boot bootstrap did not).
if [ ! -d "$SRC_DIR/.git" ]; then
  log "cloning $REPO_URL -> $SRC_DIR"
  install -d -o "$LAZYBOX_USER" -g "$LAZYBOX_USER" "$(dirname "$SRC_DIR")"
  as_build "git clone '$REPO_URL' '$SRC_DIR'"
fi
as_build "cd '$SRC_DIR' && git fetch --all --tags --prune"
# Fall back to the default branch when the pinned commit can't be checked out.
# `git fetch --all` only retrieves commits reachable from an origin ref, so a
# client built from an UNPUSHED local commit (the common dogfooding state)
# passes a SHA the box can't resolve. Aborting here would leave the box with no
# daemon at all — the exact failure #977 exists to remove — so instead build the
# default-branch tip: the box still comes up running a daemon, and a leftover
# wire-fingerprint mismatch surfaces as an actionable "run rebuild once the
# commit is pushed" notice rather than a silent dead box.
if [ -n "$TARGET_SHA" ] && as_build "cd '$SRC_DIR' && git checkout --detach '$TARGET_SHA'"; then
  log "checked out $TARGET_SHA"
else
  if [ -n "$TARGET_SHA" ]; then
    log "WARNING: commit $TARGET_SHA is not fetchable (unpushed, or gone from origin) — building the default branch tip so the box still runs a daemon; rerun the rebuild once the commit is pushed"
  else
    log "no pinned SHA — tracking the default branch tip"
  fi
  as_build "cd '$SRC_DIR' && git checkout main && git pull --ff-only"
fi

# 3. Build the daemon. `make setup` fetches the pinned zig + ghostty and
# primes the caches; `make release` then builds `lazybox` against them.
log "building (first run can take 10+ minutes)"
as_build "cd '$SRC_DIR' && make setup && make release"

# 4. Install the binary + record the exact commit somewhere greppable.
install -m0755 "$SRC_DIR/target/release/lazybox" "$BIN_DEST"
install -d -m0755 "$(dirname "$SHA_FILE")"
as_build "cd '$SRC_DIR' && git rev-parse HEAD" >"$SHA_FILE"
log "installed $BIN_DEST at $(cat "$SHA_FILE")"

# 5. Install the systemd units from the checkout and (re)start the daemon +
# idle-stop timer. `enable` wires them to boot; `restart` picks up the new
# binary on the rebuild path (and starts a not-yet-running unit on the first).
install -m0644 "$SRC_DIR/contrib/systemd/lazybox-daemon@.service" /etc/systemd/system/
install -m0644 "$SRC_DIR/contrib/box-lifecycle/lazybox-idle-stop.service" /etc/systemd/system/
install -m0644 "$SRC_DIR/contrib/box-lifecycle/lazybox-idle-stop.timer" /etc/systemd/system/
install -m0755 "$SRC_DIR/contrib/box-lifecycle/lazybox-idle-stop.sh" /usr/local/bin/lazybox-idle-stop.sh
systemctl daemon-reload
systemctl enable "lazybox-daemon@$LAZYBOX_USER.service"
systemctl restart "lazybox-daemon@$LAZYBOX_USER.service"
systemctl enable --now lazybox-idle-stop.timer
log "daemon + idle-stop timer active"
