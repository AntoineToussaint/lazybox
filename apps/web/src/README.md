# Browser test client

The Vite host mounts one reusable `AgentWindow` with Claude selected. The component owns connection and session behavior; this host supplies the resolved connection, stores the bearer for reloads, and handles session URLs and opening independent viewers. RPC traffic uses the local development proxy. Run `make web` after starting the Codefly Runner.
