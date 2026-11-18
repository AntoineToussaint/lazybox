# Lazybox host probe (REMOTE_RUNNERS.md §4.1): POSIX sh, run with `sh -s`
# over SSH. It changes nothing and prints one JSON line about this host.
clean() { printf '%s' "$1" | tr -d '"\\' | tr '\n\t' '  '; }
has() { command -v "$1" >/dev/null 2>&1 && echo true || echo false; }

os=$(uname -s 2>/dev/null)
arch=$(uname -m 2>/dev/null)
host=$(hostname 2>/dev/null || uname -n)
data="${XDG_DATA_HOME:-$HOME/.local/share}/lazybox/runner"

lazybox=""
if [ -x "$data/current/bin/lazybox" ]; then
  lazybox="$data/current/bin/lazybox"
elif command -v lazybox >/dev/null 2>&1; then
  lazybox=$(command -v lazybox)
fi
version=""
link=0
if [ -n "$lazybox" ]; then
  version=$("$lazybox" --version 2>/dev/null | head -n 1 | awk '{print $2}')
  if "$lazybox" link serve --probe 2>/dev/null | grep -q '^lazybox-link 1'; then link=1; fi
fi

systemd=false
if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then systemd=true; fi
launchd=$(has launchctl)
free=$(df -Pk "$HOME" 2>/dev/null | awk 'NR==2 {print $4}')
[ -n "$free" ] || free=0
platform=""
if [ -r /etc/os-release ]; then platform=$(. /etc/os-release && echo "$PRETTY_NAME"); fi
if [ "$os" = Darwin ] && command -v sw_vers >/dev/null 2>&1; then platform="macOS $(sw_vers -productVersion 2>/dev/null)"; fi

printf '{"os":"%s","arch":"%s","hostname":"%s","lazybox":"%s","version":"%s","link":%s,"dataDir":"%s","systemd":%s,"launchd":%s,"freeKB":%s,"claude":%s,"codex":%s,"git":%s,"platform":"%s"}\n' \
  "$(clean "$os")" "$(clean "$arch")" "$(clean "$host")" "$(clean "$lazybox")" "$(clean "$version")" "$link" "$(clean "$data")" \
  "$systemd" "$launchd" "$free" "$(has claude)" "$(has codex)" "$(has git)" "$(clean "$platform")"
