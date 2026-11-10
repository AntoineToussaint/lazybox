# V2 test CLI

Build with `make build-go` from `v2/`. `bin/lazybox` calls the generated Runner API and publishes transparent log pipes. It requires the Runner endpoint file; it never resolves V1 daemon state. See `v2/RUNNING.md`.
