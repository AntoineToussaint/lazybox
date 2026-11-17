package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestWorkspaceReviewAndConversationNamingTransport(t *testing.T) {
	s, _ := collectionService(t)
	ctx := context.Background()
	c, err := s.Bench.Collections.Create(ctx, "review", collections.Spec{Name: "Review", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(c.Members[0].Path, "hello.txt"), []byte("hello review\n"), 0600); err != nil {
		t.Fatal(err)
	}
	preview, err := s.ReadWorkspaceFile(ctx, &pb.WorkspaceFileRequest{CollectionId: c.ID, Path: "api/hello.txt"})
	if err != nil || preview.Text != "hello review\n" || preview.Path != "api/hello.txt" {
		t.Fatal(preview, err)
	}
	if _, err = s.ReadWorkspaceFile(ctx, &pb.WorkspaceFileRequest{CollectionId: c.ID, Path: "../outside"}); err == nil {
		t.Fatal("preview accepted traversal")
	}
	resolved, err := s.ResolveWorkspacePath(ctx, &pb.ResolveWorkspacePathRequest{CollectionId: c.ID, Path: "hello.txt"})
	if err != nil || len(resolved.Matches) != 1 || resolved.Matches[0].Path != "api/hello.txt" || resolved.Matches[0].Directory || !resolved.Exact {
		t.Fatal(resolved, err)
	}
	found, err := s.SearchWorkspaceFiles(ctx, &pb.SearchWorkspaceFilesRequest{CollectionId: c.ID, Query: "hel", Limit: 5})
	if err != nil || len(found.Matches) != 1 || found.Matches[0].Path != "api/hello.txt" || len(found.Matches[0].Positions) != 3 || found.Indexed == 0 {
		t.Fatal(found, err)
	}
	changes, err := s.ListWorkspaceChanges(ctx, &pb.WorkspaceChangesRequest{CollectionId: c.ID})
	if err != nil || len(changes.Repositories) != 1 || len(changes.Repositories[0].Files) != 1 || changes.Repositories[0].Files[0].Path != "hello.txt" {
		t.Fatal(changes, err)
	}
	diff, err := s.ReadWorkspaceDiff(ctx, &pb.WorkspaceDiffRequest{CollectionId: c.ID, Member: "api", Path: "hello.txt"})
	if err != nil || !strings.Contains(diff.Text, "hello review") {
		t.Fatal(diff, err)
	}
	if _, err = s.ReadWorkspaceDiff(ctx, &pb.WorkspaceDiffRequest{CollectionId: c.ID, Member: "../api", Path: "hello.txt"}); err == nil {
		t.Fatal("arbitrary member accepted")
	}
	opened, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: c.ID})
	if err != nil {
		t.Fatal(err)
	}
	renamed, err := s.RenameConversation(ctx, &pb.RenameConversationRequest{SessionId: opened.Session.Id, Title: "Review the API"})
	if err != nil || renamed.Session.Title != "Review the API" {
		t.Fatal(renamed, err)
	}
	history, err := s.ListHistory(ctx, &pb.ListHistoryRequest{CollectionId: &c.ID, Search: "the API"})
	if err != nil || len(history.Sessions) != 1 || history.Sessions[0].Title != "Review the API" {
		t.Fatal(history, err)
	}
	other := "other"
	history, err = s.ListHistory(ctx, &pb.ListHistoryRequest{CollectionId: &other})
	if err != nil || len(history.Sessions) != 0 {
		t.Fatal(history, err)
	}
	current, err := s.GetSession(ctx, &pb.SessionRequest{SessionId: opened.Session.Id})
	if err != nil || current.Session.Title != "Review the API" {
		t.Fatal(current, err)
	}
}
