# E2B sandbox provider spike

## Result

The provider path is implemented around E2B's memory-preserving lifecycle:

- `ensure` creates from a named template or reuses the running/paused sandbox
  carrying the same `lazybox_name` metadata.
- `wake` calls the connect endpoint, which resumes a paused sandbox.
- `sleep` calls pause with `memory: true`. It never calls delete.
- `destroy` is the only operation that permanently deletes the sandbox.
- The create request also sets memory-preserving auto-pause so a timeout does
  not turn an unattended sandbox into a cold boot.

The end-to-end cloud probe was not run in the implementation workspace on
2026-08-12 because it had no `E2B_API_KEY`, E2B CLI, or E2B account context.
Consequently there are no locally observed costs or honest local timing numbers
to report yet. The checked-in probe records those numbers and fails if tmux,
scrollback, the Claude process, or the sub-five-second perceived-resume bound
does not survive.

## Build and run

Install and authenticate the E2B CLI, then build the Dockerfile template from
the repository root. The 4 GiB allocation is for the Rust build and also makes
the pause-vs-RAM measurement useful.

```bash
npm install --global @e2b/cli
e2b auth login
e2b template create lazybox-e2b \
  --path . \
  --dockerfile terraform/sandbox/e2b/e2b.Dockerfile \
  --cmd /usr/local/bin/lazybox-e2b-start \
  --ready-cmd 'test -S /home/user/.lazybox/run/daemon.sock' \
  --cpu-count 2 \
  --memory-mb 4096
```

The template contains tmux, git, Claude Code, SSH/WebSocket transport, and a
baked lazybox daemon. `ensure` then runs the same on-box build helper used by
GCP, pinned to the invoking client's build SHA, in direct-service mode. This
last stamp is what guarantees the client and daemon wire fingerprints match;
the template's baked binary is only the bootstrap.

Set the API key and ensure the box:

```bash
export E2B_API_KEY=...
cargo build -p lazybox-tui-boot --release
target/release/lazybox sandbox ensure --provider e2b --template lazybox-e2b
```

`websocat` must be installed locally because E2B's documented SSH path carries
SSH over a WebSocket proxy. On macOS, `brew install websocat` supplies it.

For the persistence measurement, authenticate Claude Code inside the sandbox
once, then run:

```bash
LAZYBOX_BIN=target/release/lazybox \
  scripts/e2b-pause-resume-spike.sh
```

The default run creates a tmux session with Claude running, writes 200 unique
scrollback lines, pauses for 300 seconds, resumes, waits until SSH is usable,
and re-checks the session, scrollback, and process. Output is one measurement
row per cycle:

```text
cycle=1 memory_mb=<RAM> pause_api_ms=<pause latency> paused_ms=<held time> perceived_resume_ms=<API-to-SSH latency>
```

Set `CYCLES=2` (or more) to verify repeated pause/resume cycling. Each successful
cycle exercises the transition E2B documents as resetting its continuous
runtime counter; a wall-clock 24-hour test is therefore not necessary to test
the transition itself, but should still be part of managed-compute soak testing.

## Findings against the roadmap assumptions

These are E2B-published figures and behavior, not measurements from this
workspace:

| Question | Current finding |
| --- | --- |
| Process and tmux persistence | A full-memory pause preserves filesystem, memory, and running processes. A filesystem-only pause explicitly loses processes, so the provider always sends `memory: true`. |
| Pause latency | E2B documents approximately 4 seconds per GiB of RAM. A 4 GiB template should therefore take roughly 16 seconds to pause. |
| Resume latency | E2B documents approximately 1 second. The probe measures API call through working SSH and enforces the product's less-than-5-second perceived bound. |
| Retention | Paused sandboxes are retained indefinitely until explicitly killed. |
| Continuous-runtime cap | Hobby permits 1 hour and Pro permits 24 hours continuously running. E2B says pause/resume resets that counter. |
| Cost while paused | The roadmap's “storage-only cost” assumption is stale. E2B's current billing documentation says billing stops immediately while paused; no paused-sandbox charge is listed. |
| Cost observed by this spike | None: no credential or account was available in the implementation workspace, so no billable E2B resource was created. |
| Storage | Full-memory pause persists both the root filesystem and RAM. Persistent volumes are a separate feature and are not needed for pause/resume continuity. |

Sources: [sandbox persistence](https://docs.e2b.dev/sandbox/persistence),
[billing and limits](https://docs.e2b.dev/billing), and
[SSH access](https://docs.e2b.dev/sandbox/ssh-access).

## Network controls

Outbound internet access is enabled by default. E2B exposes both a coarse
`allowInternetAccess` switch and network rules with outbound IP/CIDR/domain
allowlists and IP/CIDR denylists. Domain filtering applies to HTTP on port 80
and TLS on port 443; QUIC is not filtered by domain. Allow rules win over deny
rules. The create-only public-traffic controls can require a traffic access
token for sandbox URLs.

The provider currently retains default outbound access because template build
stamping fetches git, Rust, Zig, and Cargo dependencies, and agents need their
API endpoints. A production managed-compute design should replace that default
with an explicit allowlist and authenticated public traffic. This is a roadmap
security requirement, not a blocker for lifecycle semantics.

Source: [E2B internet access controls](https://docs.e2b.dev/network/internet-access).
