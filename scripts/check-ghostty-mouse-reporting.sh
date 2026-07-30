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
case "${ghostty_bin}" in
  */Contents/MacOS/*)
    ghostty_app="${ghostty_bin%/Contents/MacOS/*}"
    ;;
  *)
    echo "GHOSTTY_BIN must point inside a Ghostty .app bundle" >&2
    exit 1
    ;;
esac
if [ ! -d "${ghostty_app}" ]; then
  echo "Ghostty app bundle is not installed at ${ghostty_app}" >&2
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

probe_dir="$(mktemp -d "${TMPDIR:-/tmp}/lazybox-ghostty-mouse.XXXXXX")"
probe="${probe_dir}/probe.sh"
events="${probe_dir}/events.bin"
ready="${probe_dir}/ready"
probe_window_id=""

applescript_escape() {
  local value="${1//\\/\\\\}"
  printf '%s' "${value//\"/\\\"}"
}

ghostty_app_script="$(applescript_escape "${ghostty_app}")"

# shellcheck disable=SC2329 # invoked indirectly by the EXIT trap
cleanup() {
  local status=$?
  trap - EXIT
  if [ -n "${probe_window_id}" ]; then
    local probe_window_script
    probe_window_script="$(applescript_escape "${probe_window_id}")"
    osascript \
      -e "tell application \"${ghostty_app_script}\"" \
      -e 'repeat with candidate in windows' \
      -e "if id of candidate is \"${probe_window_script}\" then close window candidate" \
      -e 'end repeat' \
      -e 'end tell' \
      >/dev/null 2>&1 || true
  fi
  rm -rf "${probe_dir}"
  exit "${status}"
}

trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

printf '%s\n' \
  '#!/bin/sh' \
  'stty raw -echo' \
  "printf '\\033[2J\\033[Hmouse reporting probe\\033[?1000h\\033[?1002h\\033[?1003h\\033[?1015h\\033[?1006h'" \
  "touch '${ready}'" \
  "dd if=/dev/tty of='${events}' bs=1 2>/dev/null" \
  >"${probe}"
chmod +x "${probe}"

probe_script="$(applescript_escape "${probe}")"
probe_ids="$(
  osascript \
    -e "tell application \"${ghostty_app_script}\"" \
    -e "set cfg to new surface configuration from {command:\"${probe_script}\", wait after command:false}" \
    -e 'set probe_window to new window with configuration cfg' \
    -e 'activate window probe_window' \
    -e 'set probe_terminal to focused terminal of selected tab of probe_window' \
    -e 'set probe_window_id to id of probe_window' \
    -e 'set probe_terminal_id to id of probe_terminal' \
    -e 'end tell' \
    -e 'return probe_window_id & tab & probe_terminal_id'
)"
IFS=$'\t' read -r probe_window_id terminal_id <<<"${probe_ids}"
if [ -z "${probe_window_id}" ] || [ -z "${terminal_id}" ]; then
  echo "Ghostty did not return the probe window and terminal IDs" >&2
  exit 1
fi

for _ in $(seq 1 50); do
  if [ -f "${ready}" ]; then
    break
  fi
  sleep 0.1
done
if [ ! -f "${ready}" ]; then
  echo "Ghostty mouse probe did not become ready" >&2
  exit 1
fi

terminal_id_script="$(applescript_escape "${terminal_id}")"
osascript \
  -e "tell application \"${ghostty_app_script}\"" \
  -e 'set probe_terminal to missing value' \
  -e 'repeat with candidate in terminals' \
  -e "if id of candidate is \"${terminal_id_script}\" then set probe_terminal to candidate" \
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

captured_bytes=0
if [ -f "${events}" ]; then
  captured_bytes="$(wc -c <"${events}")"
fi
echo "Ghostty did not forward a right-button event (${captured_bytes} bytes captured)" >&2
exit 1
