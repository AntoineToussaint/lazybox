package transport

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func TestThemeRPCsListAndCreateInTheStateThemesFolder(t *testing.T) {
	state := t.TempDir()
	b, err := runner.New(runner.Config{State: state})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	s := Service{Bench: b}
	ctx := context.Background()
	dir := filepath.Join(state, "themes")
	if b.Config.ThemeDirectory != dir {
		t.Fatalf("theme directory %q, want %q", b.Config.ThemeDirectory, dir)
	}
	empty, err := s.ListThemes(ctx, &pb.ListThemesRequest{})
	if err != nil || empty.Directory != dir || len(empty.Files) != 0 || empty.Revision == "" {
		t.Fatal(empty, err)
	}
	created, err := s.CreateTheme(ctx, &pb.CreateThemeRequest{Name: "Dusk", Content: "[tokens]\naccent = \"#c0ffee\"\n"})
	if err != nil {
		t.Fatal(err)
	}
	if created.File != "Dusk.toml" || created.Path != filepath.Join(dir, "Dusk.toml") || created.Folder.Revision == empty.Revision || len(created.Folder.Files) != 1 {
		t.Fatal(created)
	}
	if f := created.Folder.Files[0]; f.Format != "toml" || f.Json != `{"tokens":{"accent":"#c0ffee"}}` || f.Error != "" || f.ModifiedUnixMillis == 0 {
		t.Fatal(f)
	}
	// An edit made outside the Runner shows at the next list.
	if err = os.WriteFile(filepath.Join(dir, "broken.json"), []byte("{\n,"), 0o600); err != nil {
		t.Fatal(err)
	}
	listed, err := s.ListThemes(ctx, &pb.ListThemesRequest{})
	if err != nil || len(listed.Files) != 2 || listed.Files[1].Error == "" || listed.Revision == created.Folder.Revision {
		t.Fatal(listed, err)
	}
	for _, c := range []struct {
		r    *pb.CreateThemeRequest
		code codes.Code
	}{
		{&pb.CreateThemeRequest{Name: "Dusk", Content: "a = 1"}, codes.AlreadyExists},
		{&pb.CreateThemeRequest{Name: "../escape", Content: "a = 1"}, codes.InvalidArgument},
		{&pb.CreateThemeRequest{Name: "ok", Content: "a = = 1"}, codes.InvalidArgument},
	} {
		if _, err = s.CreateTheme(ctx, c.r); status.Code(err) != c.code {
			t.Errorf("%v: got %v, want %v", c.r, err, c.code)
		}
	}
}

// Themes are the owner's: no scoped credential may read or write them.
func TestThemeRPCsAreOwnerOnly(t *testing.T) {
	for _, method := range []string{pb.RunnerService_ListThemes_FullMethodName, pb.RunnerService_CreateTheme_FullMethodName, pb.RunnerService_DeleteTheme_FullMethodName} {
		_, agent := agentRPCs[method]
		_, terminal := terminalRPCs[method]
		_, device := deviceRPCs[method]
		if agent || terminal || device {
			t.Errorf("%s is allowlisted for a scoped credential", method)
		}
	}
}
