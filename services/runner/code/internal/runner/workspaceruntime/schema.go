package workspaceruntime

import _ "embed"

// Schema is the JSON Schema of a workspace's `.lazybox/workspace.yaml`
// (`lazybox schema workspace`), the same contract the parser enforces.
//
//go:embed workspace.schema.json
var Schema []byte
