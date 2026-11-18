package transport

import (
	"io"
	"os/exec"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/encoding/protojson"
)

// An agent may list its own workspace's repositories, ask to add one to it
// and wait for its own requests: nothing else. It can never add one, decline
// one, or reach another workspace; a terminal can do none of it.
func TestAgentCredentialMayOnlyAskForRepositoriesInItsOwnWorkspace(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	agent, owner, terminal := bearer(ctx, f.agentToken), bearer(ctx, ownerToken), bearer(ctx, f.terminalToken)
	denied := func(name string, err error) {
		t.Helper()
		if status.Code(err) != codes.PermissionDenied {
			t.Fatalf("%s: got %v, want PermissionDenied", name, err)
		}
	}
	// A second repository in the catalog, not in the workspace.
	path := t.TempDir()
	for _, args := range [][]string{{"init", "-b", "main"}, {"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial"}} {
		if out, err := exec.Command("git", append([]string{"-C", path}, args...)...).CombinedOutput(); err != nil {
			t.Fatal(err, string(out))
		}
	}
	module, err := f.client.AddLocalRepository(owner, &pb.AddLocalRepositoryRequest{Path: path, Name: "module-directory"})
	if err != nil {
		t.Fatal(err)
	}

	// Its own workspace: listed, with the branch an addition gets.
	list, err := f.client.ListWorkspaceRepositories(agent, &pb.ListWorkspaceRepositoriesRequest{CollectionId: f.mine.Id})
	if err != nil || len(list.Repositories) != 1 || list.Repositories[0].Name != "api" || list.Repositories[0].Path == "" || list.Branch == "" {
		t.Fatal(list, err)
	}
	_, err = f.client.ListWorkspaceRepositories(agent, &pb.ListWorkspaceRepositoriesRequest{CollectionId: f.theirs.Id})
	denied("list another workspace", err)
	_, err = f.client.ListWorkspaceRepositories(agent, &pb.ListWorkspaceRepositoriesRequest{})
	denied("list without a workspace", err)

	// Asking: only for itself.
	request, err := f.client.RequestRepository(agent, &pb.RequestRepositoryRequest{SessionId: f.conversation.ID, Query: "module directory", Reason: "shared tokenizer"})
	if err != nil || request.State != "pending" || len(request.Candidates) == 0 || request.Candidates[0].Identity != module.Repository.GitIdentity || request.CollectionId != f.mine.Id {
		t.Fatal(request, err)
	}
	_, err = f.client.RequestRepository(agent, &pb.RequestRepositoryRequest{SessionId: f.elsewhere.ID, Query: "module-directory"})
	denied("ask for another conversation", err)
	_, err = f.client.RequestRepository(agent, &pb.RequestRepositoryRequest{Query: "module-directory"})
	denied("ask without a conversation", err)
	// Already in the workspace: nothing is recorded.
	already, err := f.client.RequestRepository(agent, &pb.RequestRepositoryRequest{SessionId: f.conversation.ID, Query: "api"})
	if err != nil || !already.Already || already.Id != "" || already.Added[0].Name != "api" {
		t.Fatal(already, err)
	}

	// Waiting: only on its own requests.
	if got, err := f.client.WaitRepositoryRequest(agent, &pb.WaitRepositoryRequestRequest{RequestId: request.Id}); err != nil || got.State != "pending" {
		t.Fatal(got, err)
	}
	theirs, err := f.client.RequestRepository(owner, &pb.RequestRepositoryRequest{SessionId: f.elsewhere.ID, Query: "module-directory"})
	if err != nil {
		t.Fatal(err)
	}
	_, err = f.client.WaitRepositoryRequest(agent, &pb.WaitRepositoryRequestRequest{RequestId: theirs.Id})
	denied("wait on another conversation's request", err)
	_, err = f.client.WaitRepositoryRequest(agent, &pb.WaitRepositoryRequestRequest{RequestId: "repo-missing"})
	denied("wait on an unknown request", err)

	// Never answer or add: those are the owner's.
	_, err = f.client.DeclineRepositoryRequest(agent, &pb.DeclineRepositoryRequestRequest{RequestId: request.Id})
	denied("decline its own request", err)
	_, err = f.client.AddCollectionRepositories(agent, &pb.AddCollectionRepositoriesRequest{CollectionId: f.mine.Id, RequestKey: "self", Members: []*pb.WorktreeSpec{{RepositoryId: module.Repository.Id}}})
	denied("add a repository", err)
	remove := &pb.RemoveCollectionRepositoryRequest{CollectionId: f.mine.Id, MemberName: "api"}
	_, err = f.client.RemoveCollectionRepository(agent, remove)
	denied("remove a repository", err)
	_, err = f.client.RemoveCollectionRepository(terminal, remove)
	denied("terminal remove", err)
	stream, err := f.client.PrepareWorkspace(agent, &pb.PrepareWorkspaceRequest{RequestKey: "self", CollectionId: f.mine.Id, RepositoryIdentities: []string{module.Repository.GitIdentity}, RepositoryRequestId: request.Id})
	if err == nil {
		_, err = stream.Recv()
	}
	denied("prepare its own request", err)
	_, err = f.client.AddLocalRepository(agent, &pb.AddLocalRepositoryRequest{Path: path})
	denied("register a folder", err)

	// A terminal's credential can do none of it.
	_, err = f.client.ListWorkspaceRepositories(terminal, &pb.ListWorkspaceRepositoriesRequest{CollectionId: f.mine.Id})
	denied("terminal list", err)
	_, err = f.client.RequestRepository(terminal, &pb.RequestRepositoryRequest{SessionId: f.conversation.ID, Query: "module-directory"})
	denied("terminal ask", err)
	_, err = f.client.WaitRepositoryRequest(terminal, &pb.WaitRepositoryRequestRequest{RequestId: request.Id})
	denied("terminal wait", err)

	// The request waits in the conversation, like an approval.
	session, err := f.client.GetSession(owner, &pb.SessionRequest{SessionId: f.conversation.ID})
	if err != nil || len(session.Session.Pending) != 1 || session.Session.Pending[0].Type != "repository_request" || session.Session.Pending[0].RequestId != request.Id || session.Session.Pending[0].Text != "Add module-directory to this workspace" {
		t.Fatal(session, err)
	}
	var data pb.RepositoryRequest
	if err = protojson.Unmarshal(session.Session.Pending[0].ProviderDataJson, &data); err != nil || data.Reason != "shared tokenizer" || data.Branch == "" {
		t.Fatal("the pending entry carries the request", &data, err)
	}
	if other, err := f.client.GetSession(owner, &pb.SessionRequest{SessionId: f.elsewhere.ID}); err != nil || len(other.Session.Pending) != 1 || other.Session.Pending[0].RequestId != theirs.Id {
		t.Fatal("requests are per conversation", other, err)
	}

	// The owner adds it through PrepareWorkspace; the agent's wait has the worktree.
	add, err := f.client.PrepareWorkspace(owner, &pb.PrepareWorkspaceRequest{RequestKey: "answer", CollectionId: f.mine.Id, RepositoryIdentities: []string{module.Repository.GitIdentity}, RepositoryRequestId: request.Id})
	if err != nil {
		t.Fatal(err)
	}
	for {
		update, err := add.Recv()
		if err == io.EOF {
			break
		}
		if err != nil {
			t.Fatal(err)
		}
		if update.Collection != nil && len(update.Collection.Members) != 2 {
			t.Fatal(update.Collection)
		}
	}
	got, err := f.client.WaitRepositoryRequest(agent, &pb.WaitRepositoryRequestRequest{RequestId: request.Id, TimeoutSeconds: 5})
	if err != nil || got.State != "added" || len(got.Added) != 1 || got.Added[0].Name != "module-directory" || got.Added[0].Path == "" {
		t.Fatal(got, err)
	}
	if session, err = f.client.GetSession(owner, &pb.SessionRequest{SessionId: f.conversation.ID}); err != nil || len(session.Session.Pending) != 0 {
		t.Fatal("answered request still pending", session, err)
	}
	// The owner declines the other one; answering it twice fails.
	if declined, err := f.client.DeclineRepositoryRequest(owner, &pb.DeclineRepositoryRequestRequest{RequestId: theirs.Id, Reason: "not now"}); err != nil || declined.State != "declined" || declined.DeclineReason != "not now" {
		t.Fatal(declined, err)
	}
	if _, err = f.client.DeclineRepositoryRequest(owner, &pb.DeclineRepositoryRequestRequest{RequestId: theirs.Id}); status.Code(err) != codes.FailedPrecondition {
		t.Fatal(err)
	}
	if _, err = f.client.RequestRepository(agent, &pb.RequestRepositoryRequest{SessionId: f.conversation.ID, Query: "x\x00y"}); status.Code(err) != codes.InvalidArgument {
		t.Fatal("a malformed request is a usage error", err)
	}
}
