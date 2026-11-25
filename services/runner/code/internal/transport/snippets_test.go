package transport

import (
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"os"
	"path/filepath"
	"testing"
)

func TestOwnerSavesHelpSnippetsToRunnerCatalog(t *testing.T) {
	f := newTerminalFixture(t)
	f.bench.Config.SnippetCommand = filepath.Join(t.TempDir(), "catalog")
	if err := os.WriteFile(f.bench.Config.SnippetCommand, []byte("#!/bin/sh\necho '[]'\n"), 0700); err != nil {
		t.Fatal(err)
	}
	ctx := timeout(t)
	request := func(key string) *pb.SaveSnippetRequest {
		return &pb.SaveSnippetRequest{Yaml: "- key: " + key + "\n  body: |\n    Inspect changes.\n    Report a verdict."}
	}
	if _, err := f.client.SaveSnippet(bearer(ctx, f.agentToken), request("unapproved")); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("agent save: %v", err)
	}
	owner := bearer(ctx, ownerToken)
	done := make(chan error, 2)
	for _, key := range []string{"check-first", "check-second"} {
		go func() { _, err := f.client.SaveSnippet(owner, request(key)); done <- err }()
	}
	for range 2 {
		if err := <-done; err != nil {
			t.Fatal(err)
		}
	}
	catalog, err := f.client.ListSnippets(owner, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	saved := map[string]bool{}
	for _, entry := range catalog.Snippets {
		if entry.Origin == "saved" {
			saved[entry.Key] = true
		}
	}
	if len(saved) != 2 || !saved["check-first"] || !saved["check-second"] {
		t.Fatalf("saved catalog: %v", saved)
	}
}
