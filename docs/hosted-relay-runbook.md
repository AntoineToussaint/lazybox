# Hosted relay deployment

The hosted relay is a raw TCP service on port `9443`. A box dials out and
registers, a client dials in, and the relay splices the two streams without
executing workloads or decoding their payload. One `shared-cpu-1x` Fly Machine
or a small GCE VM is sufficient for an initial deployment.

The commands below start from a clean lazybox checkout and produce the same
`lazybox-relay` image in both deployment paths. Allow about 10–20 minutes for
the first container build and less than 10 minutes for the remaining steps.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `LAZYBOX_RELAY_LISTEN_ADDR` | `0.0.0.0:9443` | TCP address the relay binds. |
| `LAZYBOX_PLATFORM_URL` | unset | Base URL of the entitlement platform. Use `https://platform.lazybox.ai` in production. |
| `LAZYBOX_PLATFORM_API_KEY` | unset | Bearer credential for entitlement checks. Store it as a platform secret or in a mode-`0600` env file. |
| `RUST_LOG` | `lazybox_relay=info` | [`tracing-subscriber` filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html), for example `lazybox_relay=debug`. |
| `LAZYBOX_RELAY_HEALTHCHECK_ADDR` | derived from the listener | Optional target for `lazybox-relay --healthcheck`; wildcard listeners are probed through `127.0.0.1`. |

The platform gate is delivered in companion issue
[#1080](https://github.com/AntoineToussaint/lazybox/issues/1080); this deployment
contract supplies its URL and API key through the two variables above. Until
that change is present, `Relay::new()` still installs `AllowAll`; do not call an
instance a hosted, entitlement-enforcing relay until both variables are set
**and** the deployed revision contains #1080. The relay must fail closed if the
platform cannot be reached. Leaving the variables unset is reserved for a
self-hosted free relay.

## Build and test locally

The Dockerfile builds only `lazybox-relay` on Alpine/musl, then copies the
single release binary and CA bundle into `scratch`. Cargo therefore never
builds `libghostty-vt` and neither Zig nor the Ghostty source/toolchain is
installed in the builder.

```sh
git clone https://github.com/AntoineToussaint/lazybox.git
cd lazybox
cargo build --locked --release -p lazybox-tui-boot
scripts/smoke-hosted-relay.sh
```

The smoke script builds `crates/relay/Dockerfile`, starts the container on a
random loopback port, waits for its Docker health check, and then runs:

1. `lazybox server start` and `lazybox serve --relay …` as the box;
2. `lazybox --connect-relay … --smoke` as an isolated client;
3. a Noise handshake, lazybox wire handshake, subscription request, and
   snapshot response through the relay.

The last response proves bytes traveled client → relay → box daemon and back,
not merely that port `9443` accepted a connection. To test an already-running
relay without creating a local container, pass its `host:port`:

```sh
scripts/smoke-hosted-relay.sh relay.lazybox.ai:9443
```

With #1080 deployed, create a persistent smoke identity and enroll its Ed25519
public key before testing the gate:

```sh
SMOKE_BOX_HOME="$(mktemp -d)"
LAZYBOX_HOME="$SMOKE_BOX_HOME" target/release/lazybox device box --format base64
# Enroll the printed key as an active box in lazybox-platform.

LAZYBOX_PLATFORM_URL=https://platform.lazybox.ai \
LAZYBOX_PLATFORM_API_KEY='replace-with-test-secret' \
LAZYBOX_SMOKE_BOX_HOME="$SMOKE_BOX_HOME" \
  scripts/smoke-hosted-relay.sh
```

The script forwards both platform variables into the container and reuses the
provided box home without deleting it. Deactivate that same test key in the
platform, wait for the gate's active-decision cache to expire, and require the
smoke to fail before considering the gate verified.

## Fly Machine

Install and authenticate [`flyctl`](https://fly.io/docs/flyctl/install/), then
run from the repository root. Pick the region closest to most boxes by editing
`primary_region` in `contrib/fly/lazybox-relay.toml`.

```sh
fly apps create lazybox-relay
fly secrets set --app lazybox-relay \
  LAZYBOX_PLATFORM_URL=https://platform.lazybox.ai \
  LAZYBOX_PLATFORM_API_KEY='replace-with-production-secret'

fly ips allocate-v4 --app lazybox-relay
fly ips allocate-v6 --app lazybox-relay
fly deploy --ha=false --app lazybox-relay \
  --config contrib/fly/lazybox-relay.toml
```

The first command allocates a **dedicated** IPv4 address, not a shared one.
Fly's shared IPv4 proxy routes plain TCP only when it has HTTP host data or TLS
SNI; the relay protocol has neither, so raw TCP on `9443` needs a dedicated
IPv4. The IPv6 address is already dedicated. `--ha=false` creates one Machine;
remove it when the service needs multi-Machine availability.

Read the assigned addresses and create these records with the DNS provider:

```sh
fly ips list --app lazybox-relay
```

| DNS name | Type | Value |
| --- | --- | --- |
| `relay.lazybox.ai` | `A` | Dedicated Fly IPv4 |
| `relay.lazybox.ai` | `AAAA` | Dedicated Fly IPv6 |

Keep the service endpoint explicit as `relay.lazybox.ai:9443`. Do not proxy the
record through an HTTP CDN. Confirm deployment health and raw reachability:

```sh
fly checks list --app lazybox-relay
fly logs --app lazybox-relay
nc -vz relay.lazybox.ai 9443
```

Fly uses the configured TCP check every 15 seconds. The image also carries a
Docker health check for runtimes that honor it.

## GCE VM with systemd

This alternative uses a small Ubuntu VM and the same hardening/restart pattern
as the units in `contrib/systemd/`. Substitute the project, zone, and account
names once at the start.

```sh
gcloud compute addresses create lazybox-relay --region us-east1
RELAY_IP="$(gcloud compute addresses describe lazybox-relay \
  --region us-east1 --format='value(address)')"

gcloud compute firewall-rules create lazybox-relay-9443 \
  --allow tcp:9443 --target-tags lazybox-relay
gcloud compute instances create lazybox-relay \
  --zone us-east1-b --machine-type e2-small \
  --image-family ubuntu-2404-lts-amd64 \
  --image-project ubuntu-os-cloud \
  --address "$RELAY_IP" --tags lazybox-relay
```

SSH to the VM and build/install the static-ish relay binary. The Docker build
does not install Rust, Zig, or Ghostty on the host.

```sh
gcloud compute ssh lazybox-relay --zone us-east1-b

sudo apt-get update
sudo apt-get install -y docker.io git
git clone https://github.com/AntoineToussaint/lazybox.git
cd lazybox
sudo docker build -f crates/relay/Dockerfile -t lazybox-relay .
container="$(sudo docker create lazybox-relay)"
sudo docker cp "$container:/usr/local/bin/lazybox-relay" /usr/local/bin/
sudo docker rm "$container"

sudo install -d -m0755 /etc/lazybox
sudo install -m0600 contrib/systemd/lazybox-relay.env.example \
  /etc/lazybox/relay.env
sudoedit /etc/lazybox/relay.env
sudo install -m0644 contrib/systemd/lazybox-relay.service \
  /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now lazybox-relay
```

Back on the operator machine, point an `A` record for
`relay.lazybox.ai` at `$RELAY_IP`, wait for DNS propagation, and check:

```sh
gcloud compute ssh lazybox-relay --zone us-east1-b \
  --command 'sudo systemctl status lazybox-relay --no-pager'
gcloud compute ssh lazybox-relay --zone us-east1-b \
  --command 'sudo /usr/local/bin/lazybox-relay --healthcheck'
nc -vz relay.lazybox.ai 9443
```

To deploy a later revision, rebuild the image on the VM, copy the replacement
binary from a stopped container, and run `sudo systemctl restart
lazybox-relay`. `journalctl -u lazybox-relay -f` follows service logs.

## Why the relay does not terminate TLS

The relay's small routing handshake is length-prefixed framing; after a client
and box are paired, the payload stream is protected end to end by the Noise
channel in `lazybox-e2e-channel`. The client pins the box's persistent channel
public key, so the relay cannot decrypt the stream or impersonate the box. TLS
at the relay would encrypt the same ciphertext for one hop without improving
payload confidentiality or box authentication.

This does not make the relay invisible: it still observes connection timing,
IP addresses, box routing identifiers, and entitlement metadata, and it can
drop traffic. Protect the platform entitlement request itself with HTTPS, keep
the API key secret, and restrict operator access to relay logs.

## Production verification from two networks

Run the box side from its actual network (a home connection or cloud VM):

```sh
lazybox server start
lazybox serve --relay relay.lazybox.ai:9443
```

Copy the printed box id and channel key. From a client on a different network,
using the same lazybox build revision as the box, run:

```sh
lazybox --connect-relay <box-id> \
  --relay relay.lazybox.ai:9443 \
  --box-key <channel-key> \
  --smoke
```

Success prints `relay smoke passed: encrypted daemon round trip completed`.
Before announcing the endpoint, also verify that an inactive box key is
refused, `fly checks list` or `systemctl status` is healthy, and both networks
resolve `relay.lazybox.ai` to the intended public address.

Relevant platform references: [Fly raw TCP services](https://fly.io/docs/networking/services/),
[Fly TCP checks](https://fly.io/docs/reference/configuration/#services-tcp_checks),
[Fly custom-domain DNS](https://fly.io/docs/networking/custom-domain/), and
[GCE VPC firewall rules](https://cloud.google.com/firewall/docs/using-firewalls).
