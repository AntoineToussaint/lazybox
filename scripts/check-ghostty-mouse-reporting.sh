#!/usr/bin/env bash

set -euo pipefail

if [ "$(uname -s)" != "Darwin" ]; then
  echo "Ghostty AppleScript mouse checks require macOS" >&2
  exit 1
fi

ghostty_bin="${GHOSTTY_BIN:-/Applications/Ghostty.app/Contents/MacOS/ghostty}"
if [ ! -x "${ghostty_bin}" ]; then
  echo "Ghostty is not installed at ${ghostty_bin}" >&2
  exit 1
fi
default_config="$("${ghostty_bin}" +show-config --default)"
if ! grep -q '^mouse-reporting = true$' <<<"${default_config}"; then
  echo "Ghostty's default mouse-reporting setting is not enabled" >&2
  exit 1
fi
active_config="$("${ghostty_bin}" +show-config)"
if grep -q '^mouse-reporting = false$' <<<"${active_config}"; then
  echo "Ghostty mouse reporting is disabled in the active configuration" >&2
  exit 1
fi

probe_dir="$(mktemp -d /tmp/lazybox-ghostty-mouse.XXXXXX)"
trap 'rm -rf "${probe_dir}"' EXIT
probe="${probe_dir}/probe.sh"
events="${probe_dir}/events.bin"

printf '%s\n' \
  '#!/bin/sh' \
  'stty raw -echo' \
  "printf '\\033[2J\\033[Hmouse reporting probe\\033[?1000h\\033[?1002h\\033[?1003h\\033[?1015h\\033[?1006h'" \
  "dd if=/dev/tty of='${events}' bs=1 count=48 2>/dev/null" \
  >"${probe}"
chmod +x "${probe}"

terminal_id="$(
  osascript \
    -e 'tell application "Ghostty"' \
    -e "set cfg to new surface configuration from {command:\"${probe}\", wait after command:false}" \
    -e 'set probe_window to new window with configuration cfg' \
    -e 'activate window probe_window' \
    -e 'set probe_terminal to focused terminal of selected tab of probe_window' \
    -e 'return id of probe_terminal' \
    -e 'end tell'
)"

sleep 1
osascript \
  -e 'tell application "Ghostty"' \
  -e 'set probe_terminal to missing value' \
  -e 'repeat with candidate in terminals' \
  -e "if id of candidate is \"${terminal_id}\" then set probe_terminal to candidate" \
  -e 'end repeat' \
  -e 'send mouse position x 300 y 200 to probe_terminal' \
  -e 'repeat 2 times' \
  -e 'send mouse button right button action press to probe_terminal' \
  -e 'send mouse button right button action release to probe_terminal' \
  -e 'end repeat' \
  -e 'end tell'

expected_prefix="$(printf '\033[<2;')"
for _ in $(seq 1 50); do
  if [ -f "${events}" ] && grep -aFq "${expected_prefix}" "${events}"; then
    echo "Ghostty forwarded an SGR right-button event with default mouse reporting"
    exit 0
  fi
  sleep 0.1
done

echo "Ghostty did not forward a right-button event" >&2
exit 1
