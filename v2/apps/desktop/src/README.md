# Desktop application

The Tauri 2 app bundles the shared `AgentWindow` and selects Claude. Rust reads the explicitly configured Runner endpoint file; the frontend receives connection information through a Tauri command, then uses the official native HTTP plugin with the generated Connect client. Its runtime HTTP scope permits only that resolved local Runner service. The bearer is absent from window URLs and build assets.

Run `make desktop` from `v2/` after `make run`. `PROJECT` selects the initial directory; optional `SESSION` attaches to an existing execution. **Open viewer** creates a native Tauri window with independent subscriptions. Closing a window never stops the Runner or its execution. This is a local host proof; persisted geometry, display restoration, and production enrollment remain future work.
