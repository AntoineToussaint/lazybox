# box installer — a matching lazybox daemon on the box

`install.sh` is what a provisioned GCP sandbox box (`lazybox sandbox ensure`,
epic #885 / #977) runs to bring up a lazybox daemon whose **wire fingerprint
matches the client that stamped it** — build parity by construction, so the
`crates/ipc` handshake gate becomes an implementation detail instead of an
operational trap.

The Terraform startup script (`terraform/sandbox/gcp/startup.sh.tftpl`) clones
lazybox at the client's commit and hands off to this script under a
`lazybox-build.service` oneshot (a generous timeout keeps the ~10-minute first
build from looking like a failed boot). It then:

1. installs the build prerequisites (Rust, plus the C++ toolchain / `libc++`
   ghostty's VT build needs — the box counterparts of `scripts/bootstrap.sh`);
2. builds the pinned commit (`make setup && make release`) and installs the
   binary to `/usr/local/bin/lazybox`;
3. records the built commit at `/etc/lazybox/build-sha`;
4. copies + `systemctl enable --now`s the packaged units — the daemon on boot
   ([`contrib/systemd`](../systemd/README.md)) and the stop-on-idle timer
   ([`contrib/box-lifecycle`](../box-lifecycle/README.md)).

It is **idempotent**: once the recorded SHA matches the checkout, it re-asserts
the systemd wiring and exits without rebuilding, so re-running it on every boot
is cheap.

## Rebuild to a new commit

Changing a live GCE instance's startup-script metadata does not re-run it, so
moving the box to a new commit is an over-SSH rebuild:

```sh
lazybox sandbox rebuild            # → sudo /usr/local/bin/lazybox-box-install.sh <sha>
```

The startup script leaves the installer at that stable path and grants the
daemon user a narrow `NOPASSWD` sudo for exactly that command, so `rebuild`
(and the automatic footer notice the `r`-spawn worker raises on a fingerprint
mismatch) can restore parity without a manual SSH session.

## Inputs

Read from the environment (the build unit sources `/etc/lazybox/box.env`); the
optional first argument overrides `LAZYBOX_GIT_SHA` for a rebuild.

| var               | default            | meaning                          |
| ----------------- | ------------------ | -------------------------------- |
| `LAZYBOX_SRC`     | `/opt/lazybox/src` | repo checkout to build           |
| `LAZYBOX_GIT_SHA` | *(HEAD as-is)*     | commit to build                  |
| `LAZYBOX_USER`    | `lazybox`          | account the daemon runs as       |

The client reaches the box as `LAZYBOX_USER` (`sandbox.user`), so its daemon
socket — `/home/<user>/.lazybox/run/daemon.sock` — is exactly what `connect`
and the `r`-spawn forward.
