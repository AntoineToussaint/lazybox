#!/usr/bin/env python3
"""Boot an installed lazybox --test in a PTY and quit it cleanly."""

from __future__ import annotations

import os
import pty
import select
import signal
import sys
import time


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: smoke-tui.py /path/to/lazybox", file=sys.stderr)
        return 2

    binary = os.path.abspath(sys.argv[1])
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.setdefault("TERM", "xterm-256color")
        os.execv(binary, [binary, "--test"])

    deadline = time.monotonic() + 20
    next_quit = time.monotonic() + 3
    output = bytearray()
    status: int | None = None
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    output.extend(os.read(fd, 65536))
                except OSError:
                    pass

            waited, candidate = os.waitpid(pid, os.WNOHANG)
            if waited == pid:
                status = candidate
                break

            if time.monotonic() >= next_quit:
                os.write(fd, b"qq")
                next_quit = time.monotonic() + 2
    finally:
        os.close(fd)

    if status is None:
        os.kill(pid, signal.SIGTERM)
        _, status = os.waitpid(pid, 0)
        print("lazybox --test did not exit after q q", file=sys.stderr)
        return 1
    if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
        tail = bytes(output[-4000:]).decode("utf-8", errors="replace")
        print(f"lazybox --test exited abnormally ({status})\n{tail}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
