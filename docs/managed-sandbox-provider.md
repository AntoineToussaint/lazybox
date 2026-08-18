# Managed Kubernetes sandbox provider

The `managed-k8s` provider is the hosted control-plane boundary for lazybox.
It implements the same `SandboxProvider` lifecycle as GCP and E2B while
keeping Kubernetes details behind `ManagedSandboxApi`:

- `ensure` creates or resumes a tenant namespace and agent pod;
- `stop` scales the workload to zero and `start` restores one replica;
- `status` reports normalized power and reachability;
- `connect` returns the backend's authenticated ingress or exec-over-WebSocket
  tunnel, still bound to the client's local daemon socket and requested TCP
  ports;
- `destroy` removes the tenant resources.

The lazybox repository currently ships the interface and a deterministic
`InMemoryManagedApi`, enabled by the `lazybox-sandbox/managed` Cargo feature.
The fake makes lifecycle, policy, and future fleet-conductor tests executable
without pretending a production hosted service exists. A real service client
will implement the same API once the hosted entitlement and isolation gates
are ready; no TUI/config option is exposed until then.
