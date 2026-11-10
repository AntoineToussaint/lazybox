# Lazybox V2 source structure

V2 is a Codefly workspace. The first implemented service is the **Runner**, created with `codefly.dev/go-grpc:0.1.69`. Its Go module lives under the generated service layout. The npm workspace remains at this folder's root.

```text
v2/
  workspace.codefly.yaml
  services/runner/
    service.codefly.yaml            pinned service agent and declared endpoints
    proto/api.proto                 authoritative Runner RPC contract
    proto/buf.gen.yaml              Codefly's Go generation configuration
    code/
      go.mod                        Runner's independent Go module
      main.go                       Codefly-generated composition/lifecycle
      work.go                       application configuration through WithConfigure
      pkg/adapters/*_gen.go          generated gRPC / Connect listeners and health
      pkg/gen/                      generated protobuf, gRPC, and Connect Go clients
      internal/accounts/            profiles, credentials, eligibility, runner options
      internal/agent/               provider-neutral operations and events
      internal/runner/
        bench.go                    local execution composition
        engine/                     ordering, follow-up queue, receipts, requests
        journal/                    bounded durable events and independent readers
        commands/                   managed processes, pipe capture, byte reads
        attachments/                bounded private generic-file retention and replay metadata
        checkpoints/                explicit work handoff
        adapters/codex/             native app-server adapter
        adapters/claude/            supported SDK worker adapter
        adapters/jsonl/             supervised local process transport
      internal/transport/           one gRPC implementation and authority checks
      cmd/lazybox/                  test client and transparent log publisher
      tests/integration/            real harness and deterministic gateway cases
  contracts/runner-v1.binpb          initial compatibility baseline
  packages/protocol/src/gen/        generated TypeScript schema and service descriptor
  packages/client-core/src/         generated client access and cursor recovery
  packages/surfaces/src/            AgentWindow connection/session wrapper and console drawer
  apps/web/                         Vite browser host
  apps/desktop/                     bundled React surface and independent Tauri Cargo workspace
  workers/claude-sdk/                pinned supported TypeScript SDK worker
  scripts/                          viewer, browser checks, generation drift check
```

The desktop host implements local agent and native console windows. The iPhone, Hub, and shared work packages remain planning placeholders. The local test bench does not implement Hub membership, distributed scheduling, or a shared work graph.

## Boundaries

The engine depends on the generic agent vocabulary. Provider parsing stays in the adapters. The command owner is shared by both adapters, the CLI, and human viewers. Profiles resolve child-process environments without mutating the parent. The transport authenticates a local bearer before allowing application operations.

Codefly owns network resolution, listener construction, health, service startup/shutdown, and Go RPC generation. `work.go` injects the Runner service through `Configuration.Service` and installs the same authority interceptors on gRPC and Connect. There is no separate Connect implementation of the application API. Do not hand-edit generated files.

The TypeScript dependency direction remains `protocol → client-core → surfaces → web/desktop`. The SDK worker imports no application package. Its provider callbacks call back to the generic Go command owner.

## Commands

From `v2/`:

```sh
make deps             # locked npm dependencies
make generate         # Codefly Go sync, then TypeScript schema generation
make check            # format, vet, types, workspace links, codegen drift, compatibility, tests
make build            # Runner, CLI, SDK worker, browser bundle
make run              # Codefly native Runner, temporary ports, isolated test state
make web              # Vite viewer on a bound temporary port; prints authenticated URL
make desktop          # build and launch the native Tauri app against the running Runner
make check-desktop    # native Rust formatting, tests, and clippy
make test-browser     # real Codefly service + Chromium, with make run already active
make test-real        # opt-in real remote model runs using existing harness logins
```

`make test` builds the SDK worker and runs deterministic Go tests, including the real Claude SDK against a local Messages API fixture and the installed Codex harness against a local routing fixture. It skips the latter when Codex is absent. Real remote tests require explicit opt-in; skipped or unavailable paths are reported in [runtime evidence](RUNTIME_EVIDENCE.md).

`make check` regenerates output to detect drift, so it needs Codefly and its protocol companion backend. `contracts/runner-v1.binpb` is the first experimental schema baseline; compatibility is checked against it. Updating a baseline is an explicit contract decision.

Builds default to one Go worker, one runtime processor, and one Cargo worker. Sample shared-host load before increasing them. The native host has a separate Cargo workspace and imports no V1 Rust source; its application icons reuse the existing brand assets. See [the runtime guide](RUNNING.md) for options, state retention, and limits.
