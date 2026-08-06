# systemd units for a lazybox box

Templated system units so a remote box brings the daemon up unattended on
boot — no operator SSHing in to run `server start`. Closes the "no packaged
unit file" gap noted in [`docs/remote-daemon-scoping.md`][scoping].

Two independent doors to the same box, each restart-on-crash and scoped to a
distinct `LAZYBOX_HOME` per user:

- **`lazybox-daemon@.service`** — `lazybox server start`, the Unix-socket
  daemon TUI clients reach with `lazybox --connect` over an SSH-forwarded
  socket.
- **`lazybox-api@.service`** — `lazybox server api`, the loopback JSON gateway
  the desktop/web clients reach over an SSH-forwarded port.

Both are templated on the account name (`%i`), so one unit file serves every
user on the box. Run either or both — the gateway does not bind the daemon
socket, so they coexist against one home (at the cost of two poll loops over
the same store).

## Install (golden image)

Bake the units and an env dir into the image (see epic #885 / #886):

```sh
install -m0644 contrib/systemd/lazybox-daemon@.service /etc/systemd/system/
install -m0644 contrib/systemd/lazybox-api@.service    /etc/systemd/system/
install -d -m0755 /etc/lazybox
install -m0755 target/release/lazybox /usr/local/bin/lazybox
```

`ExecStart` points at `/usr/local/bin/lazybox`; adjust both units if lazybox
lives elsewhere.

## Stamp per user

At per-user provisioning time, drop the env file and enable the unit(s):

```sh
install -m0600 -o alice contrib/systemd/lazybox.env.example /etc/lazybox/alice.env
# edit /etc/lazybox/alice.env — set LAZYBOX_API_TOKEN, provider creds

systemctl enable --now lazybox-daemon@alice        # socket daemon
systemctl enable --now lazybox-api@alice            # + JSON gateway (optional)
```

`enable --now` both starts them and wires them to `multi-user.target`, so they
come back on reboot. `LAZYBOX_HOME` defaults to `/home/<user>/.lazybox`; set it
in the env file for a non-standard home.

## Operate

```sh
systemctl status lazybox-daemon@alice
journalctl -u lazybox-daemon@alice -f      # logs (lazybox also writes /tmp/lazybox.log)
systemctl restart lazybox-daemon@alice
```

## Notes

- **Auth.** `lazybox-api@` requires `LAZYBOX_API_TOKEN` (its `EnvironmentFile`
  is mandatory); the gateway refuses to start without it and binds loopback
  only. Reach it remotely through an SSH tunnel — never expose the port. See
  [`docs/byo-remote-runbook.md`][runbook].
- **One daemon per user.** Provider credentials resolve from the daemon's
  process environment, so a shared box needs one daemon per user with a
  distinct `LAZYBOX_HOME`, not one daemon serving several people — which is
  exactly what the `%i` template gives you.

[scoping]: ../../docs/remote-daemon-scoping.md
[runbook]: ../../docs/byo-remote-runbook.md
