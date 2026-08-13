#!/usr/bin/env bash
set -euo pipefail

install -d /run/sshd
ssh-keygen -A
/usr/sbin/sshd

sudo -u user -H bash -lc \
  'install -d ~/.lazybox && nohup /usr/local/bin/lazybox server start >~/.lazybox/daemon.log 2>&1 </dev/null &'

exec /usr/local/bin/websocat -b --exit-on-eof \
  ws-l:0.0.0.0:8081 tcp:127.0.0.1:22
