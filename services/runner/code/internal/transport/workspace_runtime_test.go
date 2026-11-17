package transport

import (
	"context"
	"strings"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// The workspace runtime runs code on the person's machine: approving
// scripts, starting and stopping actions, re-running setup and changing its
// settings are the owner's alone. Neither the agent credential of a
// conversation in that very workspace nor its terminal's credential may call
// them, even naming their own workspace.
func TestWorkspaceRuntimeRPCsAreTheOwnersAlone(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	id := f.mine.Id
	calls := map[string]func(context.Context) error{
		"GetWorkspaceRuntime": func(ctx context.Context) error {
			_, err := f.client.GetWorkspaceRuntime(ctx, &pb.WorkspaceRuntimeRequest{CollectionId: id})
			return err
		},
		"ApproveWorkspaceScripts": func(ctx context.Context) error {
			_, err := f.client.ApproveWorkspaceScripts(ctx, &pb.ApproveWorkspaceScriptsRequest{CollectionId: id, Member: "api", Hash: "x", RunSetup: true})
			return err
		},
		"StartWorkspaceAction": func(ctx context.Context) error {
			_, err := f.client.StartWorkspaceAction(ctx, &pb.WorkspaceActionRequest{CollectionId: id, Action: "api/web"})
			return err
		},
		"StopWorkspaceAction": func(ctx context.Context) error {
			_, err := f.client.StopWorkspaceAction(ctx, &pb.WorkspaceActionRequest{CollectionId: id, Action: "api/web"})
			return err
		},
		"RunWorkspaceSetup": func(ctx context.Context) error {
			_, err := f.client.RunWorkspaceSetup(ctx, &pb.RunWorkspaceSetupRequest{CollectionId: id, Member: "api"})
			return err
		},
		"GetWorkspaceRuntimeSettings": func(ctx context.Context) error {
			_, err := f.client.GetWorkspaceRuntimeSettings(ctx, &pb.InfoRequest{})
			return err
		},
		"SaveWorkspaceRuntimeSettings": func(ctx context.Context) error {
			_, err := f.client.SaveWorkspaceRuntimeSettings(ctx, &pb.WorkspaceRuntimeSettings{Revision: 1, PortRangeStart: 20000, PortRangeEnd: 29999})
			return err
		},
	}
	for name, call := range calls {
		for who, token := range map[string]string{"agent": f.agentToken, "terminal": f.terminalToken} {
			if err := call(bearer(ctx, token)); status.Code(err) != codes.PermissionDenied {
				t.Errorf("%s with the %s credential of this workspace: %v, want PermissionDenied", name, who, err)
			}
		}
		if err := call(ctx); status.Code(err) != codes.Unauthenticated {
			t.Errorf("%s without a credential: %v, want Unauthenticated", name, err)
		}
		if err := call(bearer(ctx, ownerToken)); status.Code(err) == codes.PermissionDenied || status.Code(err) == codes.Unauthenticated {
			t.Errorf("%s as the owner: %v", name, err)
		}
	}
}

// Settings list every registered repository so each can be configured, and
// an override that does not parse is refused with the repository's name and
// the line, leaving the saved settings unchanged.
func TestWorkspaceRuntimeSettingsListRepositoriesAndRefuseInvalidYAML(t *testing.T) {
	s, _ := collectionService(t)
	ctx := context.Background()
	got, err := s.GetWorkspaceRuntimeSettings(ctx, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Repositories) != 1 || got.Repositories[0].RepositoryId != "api" || got.PortRangeStart != 20000 {
		t.Fatalf("settings: %+v", got)
	}
	got.Repositories[0].Config = "actions:\n  web:\n    rnu: make web\n"
	if _, err = s.SaveWorkspaceRuntimeSettings(ctx, got); status.Code(err) != codes.InvalidArgument || !strings.Contains(err.Error(), "line 3") {
		t.Fatalf("an invalid override: %v", err)
	}
	got.Repositories[0].Config, got.Repositories[0].DefaultAction = "actions:\n  web: make web\n", "web"
	saved, err := s.SaveWorkspaceRuntimeSettings(ctx, got)
	if err != nil {
		t.Fatal(err)
	}
	if saved.Revision != got.Revision+1 || saved.Repositories[0].DefaultAction != "web" {
		t.Fatalf("saved: %+v", saved)
	}
	if _, err = s.SaveWorkspaceRuntimeSettings(ctx, got); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("a stale save: %v", err)
	}
	// Your own action needs no approval: the repository commits nothing.
	c := createTestCollection(t, s)
	v, err := s.GetWorkspaceRuntime(ctx, &pb.WorkspaceRuntimeRequest{CollectionId: c.Id})
	if err != nil {
		t.Fatal(err)
	}
	if len(v.Actions) != 2 || v.Actions[0].Source != "settings" || v.Actions[0].Blocked != "" || v.DefaultAction != "api/web" || v.Ports == nil || v.Ports.Count != 10 {
		t.Fatalf("runtime: %+v", v)
	}
	for _, src := range v.Sources {
		if src.Approval != "none" || src.Path != "" || !src.Overridden {
			t.Fatalf("source: %+v", src)
		}
	}
}
