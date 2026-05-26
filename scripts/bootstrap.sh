#!/usr/bin/env bash
# Bootstrap pilot's build environment.
#
# Idempotent. Installs:
#   - zig 0.15.2 to vendor/zig/<host>/. Used by libghostty's build.zig
#     (which rejects zig >= 0.16). The Makefile prepends this to PATH
#     so any system zig is ignored.
#
# libghostty-rs is vendored under crates/libghostty-vt* — no separate
# clone needed.
#
# After running, `make build` / `make run` work without the user
# having any specific zig version on PATH.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENDOR="${ROOT}/vendor"
ZIG_VERSION="0.15.2"

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

# ── Install zig 0.15.2 ──────────────────────────────────────────────────
zig_dir="${VENDOR}/zig/${host}-${ZIG_VERSION}"
zig_bin="${zig_dir}/zig"

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
  # macOS Homebrew is 0.16 and rejects ghostty's `requireZig(0.15.2)`.
  rm -rf "${zig_dir}"
  mkdir -p "${VENDOR}/zig"
  url="https://ziglang.org/download/${ZIG_VERSION}/zig-${arch}-${os}-${ZIG_VERSION}.tar.xz"
  tmp="$(mktemp -d)"
  trap "rm -rf ${tmp}" EXIT
  curl -fsSL "${url}" -o "${tmp}/zig.tar.xz"
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

# ── tmux (warn only) ────────────────────────────────────────────────────
if ! command -v tmux >/dev/null 2>&1; then
  echo "warning: tmux not found — sessions won't persist across pilot restarts"
fi

# ── Linux libc++ check (warn only) ──────────────────────────────────────
# Zig builds ghostty against LLVM's libc++ (NOT GNU libstdc++) — pilot
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

# ── Print activation hint ───────────────────────────────────────────────
echo
echo "Bootstrap complete. To use pinned zig in this shell:"
echo "  export PATH=\"${zig_dir}:\$PATH\""
echo
echo "Or run via Makefile (which sets PATH automatically):"
echo "  make build"
