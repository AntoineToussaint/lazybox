#!/usr/bin/env bash
# Extract and execute every cargo-dist binary archive produced on this runner.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
distrib="${1:-${root}/target/distrib}"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

tested=0
while IFS= read -r archive; do
  name="$(basename "${archive}")"
  dest="${work}/${name}"
  mkdir -p "${dest}"
  case "${archive}" in
    *.tar.xz|*.tar.gz|*.tgz) tar -xf "${archive}" -C "${dest}" ;;
    *.zip) unzip -q "${archive}" -d "${dest}" ;;
    *) continue ;;
  esac

  binary="$(find "${dest}" -type f -name lazybox -perm -111 | head -1)"
  if [ -z "${binary}" ]; then
    continue
  fi
  alias_binary="$(find "${dest}" -type f -name lb -perm -111 | head -1)"
  if [ -z "${alias_binary}" ]; then
    echo "${name}: archive has lazybox but is missing the lb alias" >&2
    exit 1
  fi
  tested=$((tested + 1))
  echo "smoke-testing ${name}"
  version="$("${binary}" --version)"
  alias_version="$("${alias_binary}" --version)"
  echo "${version}"
  if [ "${alias_version}" != "${version}" ]; then
    echo "${name}: lb did not delegate to the packaged lazybox binary" >&2
    exit 1
  fi
  "${binary}" --help >/dev/null
  python3 "${root}/scripts/smoke-tui.py" "${binary}"
done < <(
  find "${distrib}" -maxdepth 2 -type f \
    \( -name '*.tar.xz' -o -name '*.tar.gz' -o -name '*.tgz' -o -name '*.zip' \) \
    ! -iname '*source*' | sort
)

if [ "${tested}" -eq 0 ]; then
  echo "no executable release archives found under ${distrib}" >&2
  exit 1
fi
