package process

import (
	"bytes"
	"encoding/json"
	"flag"
	"os"
	"testing"
)

var update = flag.Bool("update", false, "rewrite schema/process.v1.json from the Go types")

const schemaFile = "schema/process.v1.json"

// The published schema is the generator's output, byte for byte: change the
// Go types, run `go test ./internal/process -update`, and review the diff.
func TestSchemaIsGeneratedFromTheTypes(t *testing.T) {
	generated := Schema()
	if *update {
		if err := os.WriteFile(schemaFile, generated, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	committed, err := os.ReadFile(schemaFile)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(committed, generated) {
		t.Fatalf("%s is stale; run `go test ./internal/process -update` and review the diff", schemaFile)
	}
	if !bytes.Equal(Schema(), generated) {
		t.Fatal("schema generation is not stable")
	}
	var doc map[string]any
	if err = json.Unmarshal(generated, &doc); err != nil {
		t.Fatal(err)
	}
	if doc["$id"] != SchemaID || doc["$ref"] != "#/$defs/Process" {
		t.Fatalf("schema root: $id %v $ref %v", doc["$id"], doc["$ref"])
	}
	defs := doc["$defs"].(map[string]any)
	for _, name := range []string{"Process", "Role", "Rule", "Action", "Gate", "Ceiling", "Budget"} {
		if defs[name] == nil {
			t.Errorf("schema has no %s definition", name)
		}
	}
}
