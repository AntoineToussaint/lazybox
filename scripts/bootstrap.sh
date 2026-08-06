#!/usr/bin/env bash
# Bootstrap lazybox's build environment.
#
# Idempotent. Installs:
#   - zig 0.16.0 to a HOST-LEVEL cache (default ~/.cache/lazybox/zig/,
#     override with LAZYBOX_ZIG_CACHE). Used by libghostty's build.zig
#     (which rejects zig >= 0.16). The Makefile prepends this to PATH
#     so any system zig is ignored. Caching outside the checkout means
#     every clone and git worktree shares one download instead of each
#     re-fetching ~45MB into its own vendor/zig/.
#
#     Set LAZYBOX_ZIG_LOCAL=1 to install into this worktree's vendor/zig/
#     instead — run.sh and the Makefile prefer a local install over the
#     shared cache when one is present.
#
# The Rust libghostty wrapper lives under crates/libghostty-vt*. Its pinned
# upstream Ghostty source is prepared once in the same host-level cache.
#
# After running, `make build` / `make run` work without the user
# having any specific zig version on PATH.

set -euo pipefail

for tool in cargo curl git tar; do
  if ! command -v "${tool}" >/dev/null 2>&1; then
    echo "ERROR: ${tool} is required to prepare the build cache" >&2
    exit 1
  fi
done
if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
  echo "ERROR: sha256sum or shasum is required to verify the Zig archive" >&2
  exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# Single source of truth for the pinned zig version, shared with the
# Makefile and CI (.github/actions/rust-build-env runs this script). Bump
# `.zig-version` and every consumer follows.
ZIG_VERSION="$(cat "${ROOT}/.zig-version")"
GHOSTTY_COMMIT="$(cat "${ROOT}/.ghostty-version")"
# Shared cache root, overridable. Keep this default in lockstep with
# the Makefile's `ZIG_CACHE` so `make build`/`run` (which compute the
# pinned PATH themselves) find what `make setup` downloaded here.
ZIG_CACHE="${LAZYBOX_ZIG_CACHE:-${HOME}/.cache/lazybox/zig}"
GHOSTTY_CACHE="${LAZYBOX_GHOSTTY_CACHE:-${HOME}/.cache/lazybox/ghostty}"
# LAZYBOX_ZIG_LOCAL=1 installs into this worktree's vendor/zig/ instead
# of the shared cache. run.sh and the Makefile prefer a local install.
if [ "${LAZYBOX_ZIG_LOCAL:-}" = "1" ]; then
  ZIG_CACHE="${ROOT}/vendor/zig"
fi

# ── Detect host ─────────────────────────────────────────────────────────
case "$(uname -s)" in
  Darwin)  os=macos ;;
  Linux)   os=linux ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64)        arch=x86_64 ;;
  *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac
host="${arch}-${os}"

# ── Install zig 0.16.0 ──────────────────────────────────────────────────
zig_dir="${ZIG_CACHE}/${host}-${ZIG_VERSION}"
zig_bin="${zig_dir}/zig"

# Migrate a pre-existing in-repo install (the old vendor/zig/ layout)
# into the shared cache so current checkouts don't re-download. Only
# moves when the cache slot is empty and the legacy binary is intact.
legacy_dir="${ROOT}/vendor/zig/${host}-${ZIG_VERSION}"
if [ ! -x "${zig_bin}" ] && [ -x "${legacy_dir}/zig" ]; then
  echo "migrating zig ${ZIG_VERSION} from ${legacy_dir} → ${zig_dir}"
  mkdir -p "${ZIG_CACHE}"
  mv "${legacy_dir}" "${zig_dir}"
  rmdir "${ROOT}/vendor/zig" "${ROOT}/vendor" 2>/dev/null || true
fi

if [ -x "${zig_bin}" ]; then
  echo "zig ${ZIG_VERSION}: already at ${zig_bin}"
else
  echo "downloading zig ${ZIG_VERSION} for ${host}..."
  # Clear any partial / corrupt prior install. Without this, a
  # previous half-extract leaves `${zig_dir}` as a non-empty dir,
  # and the `mv` below NESTS the new extract inside it instead of
  # replacing — yielding `${zig_dir}/zig-${host}-${ZIG_VERSION}/zig`
  # with no binary at the expected `${zig_bin}` path. The Makefile's
  # `$(PINNED_PATH)` then falls through to system zig, which on
  # macOS Homebrew may be a different minor and is rejected by ghostty's
  # `requireZig`.
  rm -rf "${zig_dir}"
  mkdir -p "${ZIG_CACHE}"
  url="https://ziglang.org/download/${ZIG_VERSION}/zig-${arch}-${os}-${ZIG_VERSION}.tar.xz"
  tmp="$(mktemp -d)"
  trap "rm -rf ${tmp}" EXIT
  curl -fsSL "${url}" -o "${tmp}/zig.tar.xz"
  expected_sha="$(awk -v host="${host}" '$1 == host { print $2 }' "${ROOT}/.zig-checksums")"
  if [ -z "${expected_sha}" ]; then
    echo "ERROR: no Zig checksum recorded for ${host}" >&2
    exit 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    actual_sha="$(sha256sum "${tmp}/zig.tar.xz" | awk '{print $1}')"
  else
    actual_sha="$(shasum -a 256 "${tmp}/zig.tar.xz" | awk '{print $1}')"
  fi
  if [ "${actual_sha}" != "${expected_sha}" ]; then
    echo "ERROR: Zig archive checksum mismatch for ${host}" >&2
    echo "  expected: ${expected_sha}" >&2
    echo "  actual:   ${actual_sha}" >&2
    exit 1
  fi
  tar -xJf "${tmp}/zig.tar.xz" -C "${tmp}"
  # The archive expands to zig-${arch}-${os}-${ZIG_VERSION}/.
  extracted="${tmp}/zig-${arch}-${os}-${ZIG_VERSION}"
  if [ ! -d "${extracted}" ]; then
    # Fall back: take whatever single dir was extracted.
    extracted="$(find "${tmp}" -maxdepth 1 -type d -name 'zig-*' | head -1)"
  fi
  mv "${extracted}" "${zig_dir}"
  # Verify the binary landed at the expected path. Past bug: the
  # tarball layout changed between releases (sometimes nested
  # double-deep), so a silent failure here cost a user 10+ minutes
  # of debugging "why does my build pick up system zig 0.16?"
  if [ ! -x "${zig_bin}" ]; then
    echo "ERROR: zig binary not at ${zig_bin} after install" >&2
    echo "Extract layout was:" >&2
    ls -la "${zig_dir}" >&2 || true
    exit 1
  fi
  echo "zig ${ZIG_VERSION}: installed to ${zig_bin}"
fi

# ── Cache pinned Ghostty source ─────────────────────────────────────────
# The Rust build script consumes this immutable checkout and keeps mutable
# Zig build output in Cargo's OUT_DIR. This replaces the old behavior where
# every profile/build-script hash cloned another 150–400 MB copy under target/.
ghostty_src="${GHOSTTY_CACHE}/src-${GHOSTTY_COMMIT}"
ghostty_stamp="${ghostty_src}/.ghostty-commit"
if [ -f "${ghostty_src}/build.zig" ] && [ "$(cat "${ghostty_stamp}" 2>/dev/null || true)" = "${GHOSTTY_COMMIT}" ]; then
  echo "ghostty ${GHOSTTY_COMMIT}: already at ${ghostty_src}"
else
  echo "downloading ghostty ${GHOSTTY_COMMIT}..."
  mkdir -p "${GHOSTTY_CACHE}"
  ghostty_tmp="${GHOSTTY_CACHE}/.src-${GHOSTTY_COMMIT}.tmp-$$"
  rm -rf "${ghostty_tmp}"
  git clone --filter=blob:none --no-checkout https://github.com/ghostty-org/ghostty.git "${ghostty_tmp}"
  git -C "${ghostty_tmp}" checkout "${GHOSTTY_COMMIT}"
  printf '%s\n' "${GHOSTTY_COMMIT}" > "${ghostty_tmp}/.ghostty-commit"
  rm -rf "${ghostty_src}"
  mv "${ghostty_tmp}" "${ghostty_src}"
  echo "ghostty ${GHOSTTY_COMMIT}: installed to ${ghostty_src}"
fi

# `make setup` opts into a one-time native prebuild. Besides Cargo crates this
# populates Ghostty's hashed Zig package cache, so later `make release` can use
# Cargo's strict offline mode without Zig reaching for package URLs either.
if [ "${LAZYBOX_PREFETCH_BUILD:-0}" = "1" ]; then
  echo "preparing Cargo and Ghostty caches for offline builds..."
  cargo fetch --locked
  PATH="${zig_dir}:${PATH}" \
    LAZYBOX_GHOSTTY_CACHE="${GHOSTTY_CACHE}" \
    cargo build --locked -p libghostty-vt-sys
fi

# ── Expose pinned zig to subsequent CI steps ────────────────────────────
# In GitHub Actions, $GITHUB_PATH is the only way to mutate PATH for later
# steps. Append the pinned zig dir so CI's `cargo build`/`test` resolve the
# version we just installed — the same dir the Makefile prepends via
# PINNED_PATH locally. Keeps CI on the exact toolchain contributors run.
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "${zig_dir}" >> "${GITHUB_PATH}"
fi

# ── tmux (warn only) ────────────────────────────────────────────────────
if ! command -v tmux >/dev/null 2>&1; then
  echo "warning: tmux not found — sessions won't persist across lazybox restarts"
fi

# ── Linux libc++ check (warn only) ──────────────────────────────────────
# Zig builds ghostty against LLVM's libc++ (NOT GNU libstdc++) — lazybox
# fails to link with `undefined reference to std::__1::*` on Linux
# unless libc++ + libc++abi are installed. CI installs them
# (.github/release-setup.yml); local users typically forget.
if [ "${os}" = "linux" ]; then
  missing=""
  if [ ! -f /usr/include/c++/v1/string ] && [ ! -f /usr/include/x86_64-linux-gnu/c++/v1/string ] && [ ! -f /usr/include/aarch64-linux-gnu/c++/v1/string ]; then
    missing="libc++ headers"
  fi
  if ldconfig -p 2>/dev/null | grep -q libc++abi; then
    :
  else
    missing="${missing:+${missing}, }libc++abi"
  fi
  if [ -n "${missing}" ]; then
    echo
    echo "warning: missing Linux build dependency: ${missing}"
    echo "  Debian/Ubuntu: sudo apt-get install -y libc++-dev libc++abi-dev"
    echo "  Fedora/RHEL:   sudo dnf install -y libcxx-devel libcxxabi-devel"
    echo "  Arch:          sudo pacman -S --needed libc++ libc++abi"
    echo "without these, cargo build will fail at link time with"
    echo "\`undefined reference to std::__1::*\`."
  fi
fi

# ── Install git hooks ───────────────────────────────────────────────────
# Point git at the in-tree .githooks/ so the FAST fmt-only pre-commit
# check runs on every commit (catches a botched format before it reaches
# CI; ~1s, no cargo build). Git won't auto-run hooks from a checkout, so
# this step is what turns them on. We deliberately do NOT set
# `lazybox.precommitFull` here: the heavier fmt+clippy+rustdoc gate is
# opt-in via `make install-hooks`, since a full build on every commit
# deadlocks on the shared target/ build lock when several worktrees
# commit at once. Idempotent: only writes when not already pointed there.
# core.hooksPath lives in the shared repo config, so one install covers
# every worktree of this clone. Skipped in CI — the runner has no commits
# to gate and the checks run as their own jobs.
if [ -z "${GITHUB_ACTIONS:-}" ] && [ -z "${CI:-}" ] && [ -d "${ROOT}/.githooks" ]; then
  if [ "$(git -C "${ROOT}" config core.hooksPath 2>/dev/null || true)" = ".githooks" ]; then
    echo "git hooks: already installed (.githooks, fmt-only)"
  else
    git -C "${ROOT}" config core.hooksPath .githooks
    echo "git hooks: installed .githooks (fmt-only; \`make install-hooks\` adds clippy + rustdoc)"
  fi
fi

# ── Register the desktop-contract merge driver ──────────────────────────
# A wire-crate edit rewrites the generated protocol fingerprint, so a rebase
# across one conflicts on apps/desktop/src/generated/* every time. The
# `lazybox-contract` merge driver (see .gitattributes) resolves those by
# regenerating instead of dropping markers. Like the hooks it's local config,
# so setup registers it once per clone (shared config covers every worktree).
# Skipped in CI — no rebases to resolve there.
if [ -z "${GITHUB_ACTIONS:-}" ] && [ -z "${CI:-}" ] && [ -x "${ROOT}/scripts/install-merge-driver.sh" ]; then
  "${ROOT}/scripts/install-merge-driver.sh"
fi

# ── Print activation hint ───────────────────────────────────────────────
echo
echo "Bootstrap complete. To use pinned zig in this shell:"
echo "  export PATH=\"${zig_dir}:\$PATH\""
echo
echo "Or run via Makefile (which sets PATH automatically):"
echo "  make build"
