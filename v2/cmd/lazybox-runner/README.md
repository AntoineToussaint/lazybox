# Runner entry point

The implemented Runner is the Codefly service at `services/runner/code/main.go`, configured through `work.go`. Launch it with `make run` from `v2/`; Codefly owns its network, health, generation, and lifecycle. This original scaffold directory is a pointer, not another server binary.
