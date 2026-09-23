#!/usr/bin/env bash
# Add lb -m while keeping plain lb on the installed desktop release.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
build_dir="${LAZYBOX_BUILD_DIR:-${root}/target/debug}"
prefix="${LAZYBOX_MOBILE_PREFIX:-${HOME}/.local}"
for binary in lazybox lb; do
  test -x "${build_dir}/${binary}" || { echo "Build first: make build" >&2; exit 1; }
done
test -x "${prefix}/bin/lazybox" || { echo "Install the regular Lazybox release first." >&2; exit 1; }
mkdir -p "${prefix}/lib/lazybox-mobile" "${prefix}/bin"
if [[ -f "${prefix}/bin/lb" && ! -e "${prefix}/lib/lazybox-mobile/lb.desktop-original" ]]; then
  cp -p "${prefix}/bin/lb" "${prefix}/lib/lazybox-mobile/lb.desktop-original"
fi
install -s -m 755 "${build_dir}/lazybox" "${prefix}/lib/lazybox-mobile/lazybox.new"
mv -f "${prefix}/lib/lazybox-mobile/lazybox.new" "${prefix}/lib/lazybox-mobile/lazybox"
ln -sfn "${prefix}/lib/lazybox-mobile/lazybox" "${prefix}/bin/lazybox-mobile"
install -s -m 755 "${build_dir}/lb" "${prefix}/bin/lb.new"
mv -f "${prefix}/bin/lb.new" "${prefix}/bin/lb"
for alias in lb-m lazybox-m; do
  if [[ -L "${prefix}/bin/${alias}" && "$(readlink "${prefix}/bin/${alias}")" == "${prefix}/lib/lazybox-mobile/lb-m" ]]; then
    rm "${prefix}/bin/${alias}"
  fi
done
printf 'Mobile build installed. Run: lb -m\n'
