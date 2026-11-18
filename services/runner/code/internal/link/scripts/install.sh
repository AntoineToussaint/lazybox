# Lazybox Runner install (REMOTE_RUNNERS.md §4.1): POSIX sh, run with
# `sh -s -- VERSION SHA256 UPLOAD` over SSH after the bundle was uploaded to
# $HOME/UPLOAD. The SHA-256 the other computer computed is checked here before
# anything is unpacked; then versions/VERSION is unpacked and `current` is
# switched to it. The previous version is kept for rollback, older ones go.
set -eu
version=$1
want=$2
upload=$3
case "$upload" in /*) ;; *) upload="$HOME/$upload" ;; esac
data="${XDG_DATA_HOME:-$HOME/.local/share}/lazybox/runner"

if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$upload" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  got=$(shasum -a 256 "$upload" | awk '{print $1}')
elif command -v openssl >/dev/null 2>&1; then
  got=$(openssl dgst -sha256 -r "$upload" | awk '{print $1}')
else
  echo "lazybox-install: no sha256sum, shasum or openssl on this host to check the download" >&2
  rm -f "$upload"
  exit 3
fi
if [ "$got" != "$want" ]; then
  rm -f "$upload"
  echo "lazybox-install: checksum mismatch: expected $want, got $got" >&2
  exit 3
fi

umask 077
mkdir -p "$data/versions"
chmod 700 "$data"
staging="$data/versions/.incoming-$$"
rm -rf "$staging"
mkdir -p "$staging"
if ! tar -xzf "$upload" -C "$staging"; then
  rm -rf "$staging" "$upload"
  echo "lazybox-install: the bundle could not be unpacked" >&2
  exit 4
fi
rm -f "$upload"
if [ ! -x "$staging/bin/lazybox" ]; then
  rm -rf "$staging"
  echo "lazybox-install: the bundle has no bin/lazybox" >&2
  exit 4
fi
chmod -R go-w "$staging"

previous=""
if [ -L "$data/current" ]; then previous=$(basename "$(readlink "$data/current")"); fi
if [ -d "$data/versions/$version" ]; then
  rm -rf "$data/versions/.replaced-$$"
  mv "$data/versions/$version" "$data/versions/.replaced-$$"
fi
mv "$staging" "$data/versions/$version"
ln -sfn "versions/$version" "$data/current"
rm -rf "$data/versions/.replaced-$$"

for dir in "$data"/versions/*; do
  [ -d "$dir" ] || continue
  name=$(basename "$dir")
  if [ "$name" != "$version" ] && [ "$name" != "$previous" ]; then rm -rf "$dir"; fi
done

printf '{"version":"%s","path":"%s"}\n' "$version" "$data/current/bin/lazybox"
