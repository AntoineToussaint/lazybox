package commands

import "github.com/codefly-dev/gortk"

// Compressor is the filter that turns a command's output into what an agent
// receives (COMMAND_OUTPUT.md): failures kept, progress dropped, secrets
// redacted. `lazybox run` uses it, and so does everything else that hands an
// agent output it did not run itself (a pull request's failing CI logs), so
// the agent gets the same view either way. gortk compiles its filters once,
// at construction.
var Compressor = gortk.Default().WithRedaction()
