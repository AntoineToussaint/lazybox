# Integration tests

Implemented Runner/provider integration tests live in
[`services/runner/code/tests/integration`](../../services/runner/code/tests/integration).
Browser and environment fixtures live in [`scripts`](../../scripts); the native
host has its own Rust checks. Distinguish fake providers, native protocol fixtures,
real inference/accounts and actual device/network evidence. See the
[current audit](../../audits/2026-11-09/README.md) for fresh results and omissions.
