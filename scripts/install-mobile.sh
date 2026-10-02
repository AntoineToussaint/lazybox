#!/usr/bin/env bash
# Add lb -m while keeping plain lb on the installed desktop release.
#
# The mobile client runs on the slowest device in the fleet, and `lb -m` is
# also what starts the session daemon when none is up — a daemon that then
# serves the desktop client too. So this installs a RELEASE build; pass
# LAZYBOX_MOBILE_PROFILE=debug to opt into an unoptimized one deliberately.
#
#   scripts/install-mobile.sh              install (release)
#   scripts/install-mobile.sh --uninstall  restore the previous lb, drop lb -m
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
profile="${LAZYBOX_MOBILE_PROFILE:-release}"
build_dir="${LAZYBOX_BUILD_DIR:-${CARGO_TARGET_DIR:-${root}/target}/${profile}}"
prefix="${LAZYBOX_MOBILE_PREFIX:-${HOME}/.local}"
libdir="${prefix}/lib/lazybox-mobile"
backup="${libdir}/lb.desktop-original"

if [[ "${1:-}" == "--uninstall" ]]; then
  if [[ -f "${backup}" ]]; then
    install -m 755 "${backup}" "${prefix}/bin/lb"
    rm -f "${backup}"
    printf 'Restored the previous %s/bin/lb.\n' "${prefix}"
  else
    printf 'No saved lb to restore; leaving %s/bin/lb as it is.\n' "${prefix}" >&2
  fi
  rm -f "${prefix}/bin/lazybox-mobile" "${libdir}/lazybox"
  rmdir "${libdir}" 2>/dev/null || true
  printf 'lb -m removed.\n'
  exit 0
fi

for binary in lazybox lb; do
  test -x "${build_dir}/${binary}" || {
    echo "Build first: make release (looked in ${build_dir})" >&2
    exit 1
  }
done
test -x "${prefix}/bin/lazybox" || { echo "Install the regular Lazybox release first." >&2; exit 1; }
mkdir -p "${libdir}" "${prefix}/bin"
# Keep the first lb we replace, so --uninstall has something to restore.
if [[ -f "${prefix}/bin/lb" && ! -e "${backup}" ]]; then
  cp -p "${prefix}/bin/lb" "${backup}"
fi
install -s -m 755 "${build_dir}/lazybox" "${libdir}/lazybox.new"
mv -f "${libdir}/lazybox.new" "${libdir}/lazybox"
ln -sfn "${libdir}/lazybox" "${prefix}/bin/lazybox-mobile"
install -s -m 755 "${build_dir}/lb" "${prefix}/bin/lb.new"
mv -f "${prefix}/bin/lb.new" "${prefix}/bin/lb"
printf 'Mobile build installed from %s. Run: lb -m\n' "${build_dir}"
printf 'Undo with: scripts/install-mobile.sh --uninstall\n'
