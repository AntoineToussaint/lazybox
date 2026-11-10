# Native desktop host

This independent Cargo workspace contains the Tauri 2 native host. It reads `LAZYBOX_V2_ENDPOINT` supplied by `make desktop`, validates the local origin and bearer, and grants the native HTTP plugin access to that exact Runner RPC origin/path at runtime. The bundled frontend uses a Tauri bootstrap command and native HTTP fetch. No browser/Vite server is needed.

Native console windows carry only validated session/command identities in their navigation. The Runner remains an independent Codefly Go process. `make check-desktop` runs format, unit tests, and clippy. No V1 Rust or daemon implementation is imported; only the existing application icon assets are reused.

Windows use HTML5 file drops in the shared chat component. The native Tauri drag/drop handler is disabled so it does not consume those events. Dropped files use the same browser File API and Runner upload as the picker; no additional filesystem or authentication permissions are granted.
