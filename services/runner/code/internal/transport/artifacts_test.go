package transport

import (
	"context"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func TestAgentCredentialUsesOnlyItsOwnWorkspacesArtifacts(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	agent := bearer(ctx, f.agentToken)
	owner := bearer(ctx, ownerToken)

	// The request claims a person writing from a message; the Runner records the agent.
	put, err := f.client.PutArtifact(agent, &pb.PutArtifactRequest{CollectionId: f.mine.Id, Title: "Parser plan", Kind: "plan", Content: "# Parser plan\n1. lex", Source: &pb.ArtifactSource{Kind: "message", SessionId: "someone-else", MessageId: "9"}})
	if err != nil {
		t.Fatal("own workspace refused", err)
	}
	v := put.Versions[0]
	if v.Author.Kind != "agent" || v.Author.SessionId != f.conversation.ID || v.Source.Kind != "agent" || v.Source.SessionId != f.conversation.ID || v.Source.ConversationId != f.conversation.Saved.ConversationID {
		t.Fatalf("version not attributed to the agent: %+v %+v", v.Author, v.Source)
	}
	if put.OriginSessionId != f.conversation.ID || put.Kind != "plan" || put.ContentVersion != 1 {
		t.Fatalf("%+v", put)
	}
	next, err := f.client.PutArtifact(agent, &pb.PutArtifactRequest{CollectionId: f.mine.Id, ArtifactId: put.Id, Content: "# Parser plan\n1. lex\n2. parse"})
	if err != nil || next.ContentVersion != 2 || next.Title != "Parser plan" {
		t.Fatal(next, err)
	}
	if got, err := f.client.GetArtifact(agent, &pb.GetArtifactRequest{CollectionId: f.mine.Id, ArtifactId: put.Id, Version: 1}); err != nil || got.Content != "# Parser plan\n1. lex" {
		t.Fatal(got, err)
	}
	if list, err := f.client.ListArtifacts(agent, &pb.ListArtifactsRequest{CollectionId: f.mine.Id}); err != nil || len(list.Artifacts) != 1 || list.Artifacts[0].Content != "" {
		t.Fatal(list, err)
	}

	denied := func(name string, err error) {
		t.Helper()
		if status.Code(err) != codes.PermissionDenied {
			t.Fatalf("%s: %v", name, err)
		}
	}
	theirs, err := f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: f.theirs.Id, Title: "Release notes", Content: "notes"})
	if err != nil {
		t.Fatal(err)
	}
	_, err = f.client.ListArtifacts(agent, &pb.ListArtifactsRequest{CollectionId: f.theirs.Id})
	denied("list another workspace", err)
	_, err = f.client.GetArtifact(agent, &pb.GetArtifactRequest{CollectionId: f.theirs.Id, ArtifactId: theirs.Id})
	denied("get another workspace", err)
	_, err = f.client.PutArtifact(agent, &pb.PutArtifactRequest{CollectionId: f.theirs.Id, ArtifactId: theirs.Id, Content: "spoof"})
	denied("put another workspace", err)
	_, err = f.client.ListArtifacts(agent, &pb.ListArtifactsRequest{})
	denied("list without a workspace", err)
	// The artifact id alone never crosses workspaces.
	if _, err = f.client.GetArtifact(agent, &pb.GetArtifactRequest{CollectionId: f.mine.Id, ArtifactId: theirs.Id}); status.Code(err) != codes.NotFound {
		t.Fatal("read another workspace's artifact by id", err)
	}
	_, err = f.client.TrackArtifactFile(agent, &pb.TrackArtifactFileRequest{CollectionId: f.mine.Id, Path: "api/PLAN.md"})
	denied("track", err)
	_, err = f.client.UpdateArtifact(agent, &pb.UpdateArtifactRequest{CollectionId: f.mine.Id, ArtifactId: put.Id, Title: "x"})
	denied("update", err)
	_, err = f.client.DeleteArtifact(agent, &pb.DeleteArtifactRequest{CollectionId: f.mine.Id, ArtifactId: put.Id})
	denied("delete", err)
	_, err = f.client.SaveArtifactToRepository(agent, &pb.SaveArtifactToRepositoryRequest{CollectionId: f.mine.Id, ArtifactId: put.Id, Path: "api/PLAN.md"})
	denied("save to repository", err)
	_, err = f.client.CreateArtifactIssue(agent, &pb.CreateArtifactIssueRequest{CollectionId: f.mine.Id, ArtifactId: put.Id, Member: "api"})
	denied("issue", err)

	terminal := bearer(ctx, f.terminalToken)
	_, err = f.client.ListArtifacts(terminal, &pb.ListArtifactsRequest{CollectionId: f.mine.Id})
	denied("terminal list", err)
	_, err = f.client.PutArtifact(terminal, &pb.PutArtifactRequest{CollectionId: f.mine.Id, Title: "T", Content: "x"})
	denied("terminal put", err)
}

func TestArtifactOwnerFlows(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	owner := bearer(ctx, ownerToken)
	ws := f.mine.Id
	code := func(name string, err error, want codes.Code) {
		t.Helper()
		if status.Code(err) != want {
			t.Fatalf("%s: got %v, want %s", name, err, want)
		}
	}

	from, err := f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: ws, Title: "Plan", Kind: "plan", Content: "# Plan\nv1", Source: &pb.ArtifactSource{Kind: "message", SessionId: f.conversation.ID, ConversationId: "conv", MessageId: "12", TurnId: "t"}})
	if err != nil {
		t.Fatal(err)
	}
	if from.Versions[0].Author.Kind != "person" || from.Versions[0].Source.MessageId != "12" || from.OriginConversationId != "conv" {
		t.Fatalf("%+v", from)
	}
	_, err = f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: ws, Title: "Plan", Content: "x", Source: &pb.ArtifactSource{Kind: "file", Path: "api/x.md"}})
	code("owner claims a file source", err, codes.InvalidArgument)
	_, err = f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: ws, Title: "Plan", Content: "x", Source: &pb.ArtifactSource{Kind: "message"}})
	code("message without a message", err, codes.InvalidArgument)
	_, err = f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: ws, Title: "", Content: "x"})
	code("no title", err, codes.InvalidArgument)
	_, err = f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: ws, ArtifactId: "missing", Content: "x"})
	code("unknown artifact", err, codes.NotFound)
	_, err = f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: ws, Title: "T", Content: "x", Links: []*pb.ArtifactLink{{Kind: "mission", Id: "m", Role: "brief"}}})
	code("mission link", err, codes.FailedPrecondition)
	v2, err := f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: ws, ArtifactId: from.Id, Content: "# Plan\nv2", ExpectedVersion: 1, Source: &pb.ArtifactSource{Kind: "message", SessionId: f.conversation.ID, MessageId: "30"}})
	if err != nil || v2.ContentVersion != 2 {
		t.Fatal(v2, err)
	}
	_, err = f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: ws, ArtifactId: from.Id, Content: "# Plan\nstale", ExpectedVersion: 1})
	code("stale reviewed diff", err, codes.Aborted)
	if same, err := f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: ws, ArtifactId: from.Id, Content: "# Plan\nv2"}); err != nil || same.ContentVersion != 2 || len(same.Versions) != 2 {
		t.Fatal("retry made a version", same, err)
	}

	// Track a repository file; tracking it again returns the same artifact.
	c, err := f.bench.Collections.Get(ws)
	if err != nil {
		t.Fatal(err)
	}
	member, _ := filepath.Rel(c.Root, c.Members[0].Path)
	if err = os.MkdirAll(filepath.Join(c.Members[0].Path, "docs"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(c.Members[0].Path, "docs", "PLAN.md"), []byte("# Data source plan\nbody"), 0o644); err != nil {
		t.Fatal(err)
	}
	tracked, err := f.client.TrackArtifactFile(owner, &pb.TrackArtifactFileRequest{CollectionId: ws, Path: member + "/docs/PLAN.md", Kind: "plan"})
	if err != nil || tracked.Title != "Data source plan" || tracked.TrackedPath != member+"/docs/PLAN.md" || tracked.Versions[0].Source.Kind != "file" {
		t.Fatal(tracked, err)
	}
	again, err := f.client.TrackArtifactFile(owner, &pb.TrackArtifactFileRequest{CollectionId: ws, Path: member + "/docs/PLAN.md"})
	if err != nil || again.Id != tracked.Id {
		t.Fatal("tracked twice", again, err)
	}
	_, err = f.client.TrackArtifactFile(owner, &pb.TrackArtifactFileRequest{CollectionId: ws, Path: member + "/docs"})
	code("track a folder", err, codes.InvalidArgument)
	_, err = f.client.TrackArtifactFile(owner, &pb.TrackArtifactFileRequest{CollectionId: ws, Path: member + "/missing.md"})
	code("track a missing file", err, codes.NotFound)

	// Save to repository: a new file, never over an existing one unseen.
	saved, err := f.client.SaveArtifactToRepository(owner, &pb.SaveArtifactToRepositoryRequest{CollectionId: ws, ArtifactId: from.Id, Path: member + "/docs/plans/rollout.md", Track: true})
	if err != nil || saved.Path != member+"/docs/plans/rollout.md" || saved.Artifact.TrackedPath != saved.Path {
		t.Fatal(saved, err)
	}
	if data, _ := os.ReadFile(filepath.Join(c.Members[0].Path, "docs", "plans", "rollout.md")); string(data) != "# Plan\nv2" {
		t.Fatal(string(data))
	}
	_, err = f.client.SaveArtifactToRepository(owner, &pb.SaveArtifactToRepositoryRequest{CollectionId: ws, ArtifactId: tracked.Id, Path: member + "/docs/plans/rollout.md"})
	code("save over an existing file", err, codes.AlreadyExists)
	if !strings.Contains(status.Convert(err).Message(), "already exists at") {
		t.Fatal(err)
	}
	file, err := f.client.ReadWorkspaceFile(owner, &pb.WorkspaceFileRequest{CollectionId: ws, Path: member + "/docs/plans/rollout.md"})
	if err != nil {
		t.Fatal(err)
	}
	_, err = f.client.SaveArtifactToRepository(owner, &pb.SaveArtifactToRepositoryRequest{CollectionId: ws, ArtifactId: tracked.Id, Path: member + "/docs/plans/rollout.md", ExpectedFileVersion: "stale"})
	code("replace a changed file", err, codes.Aborted)
	if _, err = f.client.SaveArtifactToRepository(owner, &pb.SaveArtifactToRepositoryRequest{CollectionId: ws, ArtifactId: tracked.Id, Path: member + "/docs/plans/rollout.md", ExpectedFileVersion: file.Version}); err != nil {
		t.Fatal(err)
	}
	_, err = f.client.SaveArtifactToRepository(owner, &pb.SaveArtifactToRepositoryRequest{CollectionId: ws, ArtifactId: tracked.Id, Path: "../outside.md"})
	code("save outside the workspace", err, codes.FailedPrecondition)

	renamed, err := f.client.UpdateArtifact(owner, &pb.UpdateArtifactRequest{CollectionId: ws, ArtifactId: from.Id, Title: "Rollout", Kind: "spec", StopTracking: true})
	if err != nil || renamed.Title != "Rollout" || renamed.Kind != "spec" || renamed.TrackedPath != "" || renamed.Content != "# Plan\nv2" {
		t.Fatal(renamed, err)
	}
	_, err = f.client.CreateArtifactIssue(owner, &pb.CreateArtifactIssueRequest{CollectionId: ws, ArtifactId: from.Id, Member: "api"})
	code("issue without GitHub", err, codes.FailedPrecondition)
	if _, err = f.client.DeleteArtifact(owner, &pb.DeleteArtifactRequest{CollectionId: ws, ArtifactId: from.Id}); err != nil {
		t.Fatal(err)
	}
	_, err = f.client.GetArtifact(owner, &pb.GetArtifactRequest{CollectionId: ws, ArtifactId: from.Id})
	code("deleted", err, codes.NotFound)

	// Deleting the workspace deletes its artifacts, and none can be added to it.
	if _, err = f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: f.theirs.Id, Title: "Release notes", Content: "x"}); err != nil {
		t.Fatal(err)
	}
	if _, err = f.service.DeleteWorkspace(context.Background(), &pb.DeleteWorkspaceRequest{CollectionId: f.theirs.Id, ConfirmName: f.theirs.Name, DiscardChanges: true, StopSessions: true}); err != nil {
		t.Fatal(err)
	}
	if list, err := f.client.ListArtifacts(owner, &pb.ListArtifactsRequest{CollectionId: f.theirs.Id}); err != nil || len(list.Artifacts) != 0 {
		t.Fatal("artifacts outlived their workspace", list, err)
	}
	_, err = f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: f.theirs.Id, Title: "late", Content: "x"})
	code("put into a deleted workspace", err, codes.NotFound)
	_, err = f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: "unknown", Title: "T", Content: "x"})
	code("put into an unknown workspace", err, codes.NotFound)
}

// Retiring is how an agent consolidates: it archives, never deletes, only an
// artifact agents published in its own workspace, and only the person
// restores. A put that supersedes others retires them, or writes nothing.
func TestAgentRetiresAndSupersedesOnlyAgentPublishedArtifacts(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	agent := bearer(ctx, f.agentToken)
	owner := bearer(ctx, ownerToken)
	ws := f.mine.Id
	code := func(name string, err error, want codes.Code) {
		t.Helper()
		if status.Code(err) != want {
			t.Fatalf("%s: got %v, want %s", name, err, want)
		}
	}
	put := func(who context.Context, title string) *pb.Artifact {
		t.Helper()
		a, err := f.client.PutArtifact(who, &pb.PutArtifactRequest{CollectionId: ws, Title: title, Kind: "plan", Content: "# " + title})
		if err != nil {
			t.Fatal(err)
		}
		return a
	}
	listed := func(includeRetired bool) []string {
		t.Helper()
		list, err := f.client.ListArtifacts(agent, &pb.ListArtifactsRequest{CollectionId: ws, IncludeRetired: includeRetired})
		if err != nil {
			t.Fatal(err)
		}
		var titles []string
		for _, a := range list.Artifacts {
			titles = append(titles, a.Title)
		}
		slices.Sort(titles)
		return titles
	}
	milestones, left, limits := put(agent, "Milestone plan"), put(agent, "What's left"), put(agent, "Limitations")
	persons := put(owner, "My notes")

	// Retire one alone: archived with who and why, out of the default list, every version kept.
	retired, err := f.client.RetireArtifact(agent, &pb.RetireArtifactRequest{CollectionId: ws, ArtifactId: limits.Id, Reason: "folded into the plan"})
	if err != nil {
		t.Fatal(err)
	}
	if r := retired.Retired; r == nil || r.By.Kind != "agent" || r.By.SessionId != f.conversation.ID || r.Reason != "folded into the plan" || r.SupersededBy != "" || retired.Content != "# Limitations" {
		t.Fatalf("%+v", retired)
	}
	if got := listed(false); !slices.Equal(got, []string{"Milestone plan", "My notes", "What's left"}) {
		t.Fatal("default list shows a retired artifact:", got)
	}
	if got := listed(true); len(got) != 4 {
		t.Fatal("include_retired:", got)
	}
	if got, err := f.client.GetArtifact(agent, &pb.GetArtifactRequest{CollectionId: ws, ArtifactId: limits.Id}); err != nil || got.Retired == nil {
		t.Fatal("a retired artifact stays readable", got, err)
	}

	// Never the person's, nor another workspace's, nor a restore.
	_, err = f.client.RetireArtifact(agent, &pb.RetireArtifactRequest{CollectionId: ws, ArtifactId: persons.Id})
	code("retire the person's artifact", err, codes.PermissionDenied)
	edited := put(agent, "Edited plan")
	if _, err = f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: ws, ArtifactId: edited.Id, Content: "# Edited by the person"}); err != nil {
		t.Fatal(err)
	}
	_, err = f.client.RetireArtifact(agent, &pb.RetireArtifactRequest{CollectionId: ws, ArtifactId: edited.Id})
	code("retire an artifact the person edited", err, codes.PermissionDenied)
	theirs, err := f.client.PutArtifact(owner, &pb.PutArtifactRequest{CollectionId: f.theirs.Id, Title: "Release notes", Content: "notes"})
	if err != nil {
		t.Fatal(err)
	}
	_, err = f.client.RetireArtifact(agent, &pb.RetireArtifactRequest{CollectionId: f.theirs.Id, ArtifactId: theirs.Id})
	code("retire another workspace's artifact", err, codes.PermissionDenied)
	_, err = f.client.RetireArtifact(agent, &pb.RetireArtifactRequest{CollectionId: ws, ArtifactId: theirs.Id})
	code("retire another workspace's artifact by id", err, codes.NotFound)
	_, err = f.client.RetireArtifact(agent, &pb.RetireArtifactRequest{CollectionId: ws, ArtifactId: limits.Id, Restore: true})
	code("agent restores", err, codes.PermissionDenied)
	_, err = f.client.RetireArtifact(agent, &pb.RetireArtifactRequest{CollectionId: ws, ArtifactId: milestones.Id, SupersededBy: theirs.Id})
	code("superseded by another workspace's artifact", err, codes.InvalidArgument)

	// A merge: one put publishes the status page and retires the rest, pointing to it.
	_, err = f.client.PutArtifact(agent, &pb.PutArtifactRequest{CollectionId: ws, Title: "Status", Content: "# Status", Supersedes: []string{milestones.Id, persons.Id}})
	code("supersede the person's artifact", err, codes.PermissionDenied)
	_, err = f.client.PutArtifact(agent, &pb.PutArtifactRequest{CollectionId: ws, ArtifactId: left.Id, Content: "# Status", Supersedes: []string{left.Id}})
	code("supersede itself", err, codes.InvalidArgument)
	if got := listed(true); slices.Contains(got, "Status") || len(got) != 5 {
		t.Fatal("a refused merge published:", got)
	}
	statusPage, err := f.client.PutArtifact(agent, &pb.PutArtifactRequest{CollectionId: ws, Title: "Status", Kind: "plan", Content: "# Status\nAll of it", Supersedes: []string{milestones.Id, left.Id, limits.Id, milestones.Id}})
	if err != nil {
		t.Fatal(err)
	}
	for _, id := range []string{milestones.Id, left.Id, limits.Id} {
		a, err := f.client.GetArtifact(owner, &pb.GetArtifactRequest{CollectionId: ws, ArtifactId: id})
		if err != nil || a.Retired == nil || a.Retired.SupersededBy != statusPage.Id || a.Retired.By.Kind != "agent" {
			t.Fatalf("%s not superseded: %+v %v", id, a.GetRetired(), err)
		}
	}
	if got := listed(false); !slices.Equal(got, []string{"Edited plan", "My notes", "Status"}) {
		t.Fatal(got)
	}

	// The person restores (the merge card's Undo), and may retire their own.
	for _, id := range []string{milestones.Id, left.Id, limits.Id} {
		if a, err := f.client.RetireArtifact(owner, &pb.RetireArtifactRequest{CollectionId: ws, ArtifactId: id, Restore: true}); err != nil || a.Retired != nil {
			t.Fatal(a, err)
		}
	}
	if got := listed(false); len(got) != 6 {
		t.Fatal("restored artifacts are listed again:", got)
	}
	if a, err := f.client.RetireArtifact(owner, &pb.RetireArtifactRequest{CollectionId: ws, ArtifactId: persons.Id, SupersededBy: statusPage.Id}); err != nil || a.Retired.By.Kind != "person" || a.Retired.SupersededBy != statusPage.Id {
		t.Fatal(a, err)
	}
	terminal := bearer(ctx, f.terminalToken)
	_, err = f.client.RetireArtifact(terminal, &pb.RetireArtifactRequest{CollectionId: ws, ArtifactId: statusPage.Id})
	code("terminal retires", err, codes.PermissionDenied)
}

var _ github.Issues = (*github.Client)(nil)
