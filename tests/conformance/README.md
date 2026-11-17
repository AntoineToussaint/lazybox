# Agent conformance tests

Conformance cases are implemented alongside the Runner's engine, adapters and
transport in [`services/runner/code/internal`](../../services/runner/code/internal),
with native protocol/live scenarios in
[`tests/integration`](../../services/runner/code/tests/integration).
They cover conversation operations, pending requests, interruption, receipts,
continuation and provider-specific limits. This directory is an index, not the
only test location. The [audit](../../audits/2026-11-09/README.md) distinguishes
passing fixtures from unverified real-provider/device behavior.
