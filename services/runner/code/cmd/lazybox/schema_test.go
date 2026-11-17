package main

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// Every schema ships with the CLI, is valid JSON, and names itself by a URN,
// never a web address that could stop answering.
func TestSchemasShipWithTheCLIAndPointNowhere(t *testing.T) {
	home := t.TempDir()
	for _, s := range schemas {
		got := runPower(context.Background(), t, home, nil, "", "schema", s.name)
		if got.code != 0 {
			t.Fatalf("%s: %+v", s.name, got)
		}
		var doc map[string]any
		if err := json.Unmarshal([]byte(got.stdout), &doc); err != nil {
			t.Fatalf("%s is not JSON: %v", s.name, err)
		}
		if id, _ := doc["$id"].(string); !strings.HasPrefix(id, "urn:lazybox:") {
			t.Errorf("%s: $id %q", s.name, id)
		}
		if strings.Contains(got.stdout, "lazybox.ai") {
			t.Errorf("%s names lazybox.ai", s.name)
		}
	}
	dir := filepath.Join(t.TempDir(), ".lazybox")
	if got := runPower(context.Background(), t, home, nil, "", "schema", "--write", dir); got.code != 0 {
		t.Fatal(got)
	}
	for _, s := range schemas {
		if _, err := os.Stat(filepath.Join(dir, s.file)); err != nil {
			t.Fatal(err)
		}
	}
	if got := runPower(context.Background(), t, home, nil, "", "schema", "nope"); got.code != exitNotFound {
		t.Fatalf("%+v", got)
	}
}
