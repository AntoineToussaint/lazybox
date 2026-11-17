# Client core

The local Runner client and independent cursor/reconnect helper are implemented in `index.ts`. This package has no React, DOM, Tauri, or Expo dependency.

`createRunnerClient(baseUrl, token, fetch?)` accepts a host transport. The native desktop host supplies Tauri's official HTTP plugin; the browser uses its default fetch. Generated unary/streaming RPC behavior and bearer injection remain shared.
