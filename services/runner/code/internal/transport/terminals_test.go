package transport

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func TestTerminalRPCsRunAShellInsideTheWorkspaceOnly(t *testing.T) {
	t.Setenv("SHELL", "/bin/sh")
	t.Setenv("HOME", t.TempDir())
	t.Setenv("ENV", "")
	t.Setenv("LAZYBOX_V2_STATE", "/runner/private/state")
	s, _ := collectionService(t)
	collection := createTestCollection(t, s)
	ctx := context.Background()

	for _, directory := range []string{"..", "../..", "/etc", "missing"} {
		_, err := s.OpenTerminal(ctx, &pb.OpenTerminalRequest{CollectionId: collection.Id, Directory: directory, Cols: 80, Rows: 24})
		if status.Code(err) != codes.FailedPrecondition {
			t.Fatalf("directory %q outside the workspace must be refused, got %v", directory, err)
		}
	}
	if _, err := s.OpenTerminal(ctx, &pb.OpenTerminalRequest{CollectionId: collection.Id, Cols: 0, Rows: 24}); err == nil {
		t.Fatal("a zero-size terminal must be refused")
	}
	if _, err := s.OpenTerminal(ctx, &pb.OpenTerminalRequest{CollectionId: "unknown", Cols: 80, Rows: 24}); status.Code(err) != codes.NotFound {
		t.Fatalf("an unknown workspace must be reported, got %v", err)
	}

	opened, err := s.OpenTerminal(ctx, &pb.OpenTerminalRequest{CollectionId: collection.Id, Directory: "api", Cols: 80, Rows: 24, RequestKey: "panel-1"})
	if err != nil {
		t.Fatal(err)
	}
	if opened.Directory != "api" || opened.Shell != "/bin/sh" || opened.CollectionId != collection.Id || !strings.HasSuffix(opened.Title, "/ api") {
		t.Fatalf("unexpected terminal %+v", opened)
	}
	retried, err := s.OpenTerminal(ctx, &pb.OpenTerminalRequest{CollectionId: collection.Id, Directory: "api", Cols: 80, Rows: 24, RequestKey: "panel-1"})
	if err != nil || retried.Id != opened.Id {
		t.Fatalf("a retried open must not start a second shell: %v", err)
	}
	listed, err := s.ListTerminals(ctx, &pb.ListTerminalsRequest{CollectionId: collection.Id})
	if err != nil || len(listed.Terminals) != 1 {
		t.Fatalf("listed %+v %v", listed, err)
	}

	if _, err = s.WriteTerminal(ctx, &pb.WriteTerminalRequest{TerminalId: opened.Id, Data: []byte("echo \"[$PWD|$LAZYBOX_V2_COLLECTION_ID|${LAZYBOX_V2_ENDPOINT:-none}|${LAZYBOX_V2_STATE:-unset}]\"\n")}); err != nil {
		t.Fatal(err)
	}
	if _, err = s.ResizeTerminal(ctx, &pb.ResizeTerminalRequest{TerminalId: opened.Id, Cols: 132, Rows: 50}); err != nil {
		t.Fatal(err)
	}
	member, _ := filepath.EvalSymlinks(filepath.Join(collection.Root, "api"))
	terminal, err := s.Bench.Terminals.Get(opened.Id)
	if err != nil {
		t.Fatal(err)
	}
	wait, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	var output strings.Builder
	// The shell gets its own terminal credential, never the Runner's state or the owner's.
	endpoint := s.Bench.TerminalEndpoint(opened.Id)
	for after := uint64(0); !strings.Contains(output.String(), "["+member+"|"+collection.Id+"|"+endpoint+"|unset]"); {
		data, offset, _, err := terminal.Next(wait, after)
		if err != nil {
			t.Fatalf("%v; output %q", err, output.String())
		}
		output.Write(data)
		after = offset + uint64(len(data))
	}

	closed, err := s.CloseTerminal(ctx, &pb.TerminalRequest{TerminalId: opened.Id})
	if err != nil || !closed.Done {
		t.Fatalf("closing must stop the shell: %+v %v", closed, err)
	}
	if _, err = s.WriteTerminal(ctx, &pb.WriteTerminalRequest{TerminalId: opened.Id, Data: []byte("x")}); status.Code(err) != codes.NotFound {
		t.Fatalf("a closed terminal is gone, got %v", err)
	}
	if _, err := os.Stat(endpoint); !os.IsNotExist(err) {
		t.Fatalf("a closed terminal's credential file is kept: %v", err)
	}
	if entries, _ := os.ReadDir(filepath.Join(s.Bench.Config.State, "terminals")); len(entries) != 0 {
		t.Fatalf("a closed terminal leaves no record: %v", entries)
	}
}
