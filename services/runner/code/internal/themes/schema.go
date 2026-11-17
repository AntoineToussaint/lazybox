package themes

import _ "embed"

// Schema is the JSON Schema of a theme file (`lazybox schema theme`). The
// client owns the token catalogue, so the schema is generated there
// (packages/surfaces/src/appearance/theme-schema.ts) and kept identical by
// scripts/theme-schema.test.ts.
//
//go:embed theme.schema.json
var Schema []byte
