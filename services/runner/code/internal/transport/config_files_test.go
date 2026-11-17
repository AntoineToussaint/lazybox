package transport

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// The config files reach the app as JSON with their problems by line; the
// app's edits keep comments; a breaking edit is refused and nothing written.
func TestConfigFilesRPCs(t *testing.T) {
	f := newPowerFixture(t, "")
	ctx := f.owner(t)
	dir := filepath.Dir(f.service.Bench.Actions().Path())
	got, err := f.client.GetConfigFiles(ctx, &pb.GetConfigFilesRequest{})
	if err != nil || got.Directory != dir || len(got.Files) != 2 || got.Files[0].Name != "keys" || got.Files[0].Exists {
		t.Fatalf("empty: %+v %v", got, err)
	}
	if err = os.WriteFile(filepath.Join(dir, "keys.yaml"), []byte("# mine\nkeys:\n  global:\n    palette.open: Mod+y\n    nope.open: x\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	_ = os.MkdirAll(filepath.Join(dir, "layouts"), 0o700)
	_ = os.WriteFile(filepath.Join(dir, "layouts", "wide.yaml"), []byte("name: Wide\nlayout: {panels: [chat]}\n"), 0o600)
	loaded, _ := f.client.GetConfigFiles(ctx, &pb.GetConfigFilesRequest{})
	keys := loaded.Files[0]
	if loaded.Revision <= got.Revision || !keys.Exists || len(keys.Issues) != 1 || keys.Issues[0].Line != 5 || !strings.Contains(keys.Json, `"action":"palette.open"`) || len(loaded.Files) != 3 || loaded.Files[2].Name != "layouts/wide" || !strings.Contains(loaded.Files[2].Json, `"kinds":["chat"]`) {
		t.Fatalf("loaded: %+v", loaded)
	}
	// An edit keeps the person's comment; the existing problem does not block it.
	if _, err = f.client.UpdateConfigFile(ctx, &pb.UpdateConfigFileRequest{Name: "keys", Edits: []*pb.ConfigEdit{{Path: []string{"keys", "global", "inbox.open"}, Json: `"Mod+i"`}}}); err != nil {
		t.Fatal(err)
	}
	data, _ := os.ReadFile(filepath.Join(dir, "keys.yaml"))
	if !strings.Contains(string(data), "# mine") || !strings.Contains(string(data), "inbox.open: Mod+i") {
		t.Fatalf("written:\n%s", data)
	}
	// A conflicting edit is refused with the reason, and nothing is written.
	_, err = f.client.UpdateConfigFile(ctx, &pb.UpdateConfigFileRequest{Name: "keys", Edits: []*pb.ConfigEdit{{Path: []string{"keys", "global", "files.find"}, Json: `"Mod+y"`}}})
	if status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "already") {
		t.Fatalf("conflict: %v", err)
	}
	if after, _ := os.ReadFile(filepath.Join(dir, "keys.yaml")); string(after) != string(data) {
		t.Fatal("refused edit was written")
	}
	// A layout imported whole, then removed.
	imported, err := f.client.UpdateConfigFile(ctx, &pb.UpdateConfigFileRequest{Name: "layouts/review", Content: "name: Review\nlayout:\n  split: row\n  children: [{panels: [changes]}, {panels: [chat]}]\n"})
	if err != nil || len(imported.Files) != 4 {
		t.Fatalf("import: %+v %v", imported, err)
	}
	var layout struct{ Name string }
	_ = json.Unmarshal([]byte(imported.Files[2].Json), &layout)
	if layout.Name != "Review" {
		t.Fatalf("layout: %+v", imported.Files[2])
	}
	if removed, err := f.client.UpdateConfigFile(ctx, &pb.UpdateConfigFileRequest{Name: "layouts/review", Remove: true}); err != nil || len(removed.Files) != 3 {
		t.Fatalf("remove: %+v %v", removed, err)
	}
	if _, err = f.client.UpdateConfigFile(ctx, &pb.UpdateConfigFileRequest{Name: "keys"}); status.Code(err) != codes.InvalidArgument {
		t.Fatalf("empty update: %v", err)
	}
}

// `lazybox do`: the registry action goes to the window with its argument;
// an unknown one is refused with a suggestion.
func TestDoRequestsReachTheWindow(t *testing.T) {
	f := newPowerFixture(t, "")
	ctx := f.owner(t)
	watch, err := f.client.WatchUiRequests(ctx, &pb.WatchUiRequestsRequest{Surface: "web"})
	if err != nil {
		t.Fatal(err)
	}
	_, _ = watch.Recv()
	if _, err = f.client.PostUiRequest(ctx, &pb.UiRequest{Kind: "do", ActionId: "layout.maximise"}); status.Code(err) != codes.NotFound || !strings.Contains(err.Error(), "did you mean layout.maximize") {
		t.Fatalf("unknown: %v", err)
	}
	if _, err = f.client.PostUiRequest(ctx, &pb.UiRequest{Kind: "do", ActionId: "layout.maximize", Argument: "x"}); status.Code(err) != codes.InvalidArgument {
		t.Fatalf("argument: %v", err)
	}
	posted, err := f.client.PostUiRequest(ctx, &pb.UiRequest{Kind: "do", ActionId: "settings.open", Argument: "keyboard"})
	if err != nil {
		t.Fatal(err)
	}
	if got, err := watch.Recv(); err != nil || got.Id != posted.Id || got.Kind != "do" || got.ActionId != "settings.open" || got.Argument != "keyboard" {
		t.Fatalf("delivered %+v %v", got, err)
	}
}

// A hook with respond may ask the window only for actions marked safe, and
// waits only on its own requests; with read only, not at all.
func TestHookMayDoOnlySafeActions(t *testing.T) {
	out := filepath.Join(t.TempDir(), "endpoint.json")
	release := filepath.Join(t.TempDir(), "release")
	t.Setenv("HOOK_OUT", out)
	t.Setenv("HOOK_RELEASE", release)
	f := newPowerFixture(t, `hooks:
  - id: arranger
    on: turn.done
    grants: [respond]
    shell: 'cp "$LAZYBOX_V2_ENDPOINT" "$HOOK_OUT.tmp" && mv "$HOOK_OUT.tmp" "$HOOK_OUT"; while [ ! -e "$HOOK_RELEASE" ]; do sleep 0.05; done'
`)
	defer os.WriteFile(release, nil, 0o600)
	owner := f.owner(t)
	watch, err := f.client.WatchUiRequests(owner, &pb.WatchUiRequestsRequest{Surface: "desktop"})
	if err != nil {
		t.Fatal(err)
	}
	_, _ = watch.Recv()
	if _, err = f.client.TestHook(owner, &pb.TestHookRequest{HookId: "arranger"}); err != nil {
		t.Fatal(err)
	}
	var endpoint struct{ URL, Token string }
	until(t, "hook never received its endpoint", func() bool {
		data, err := os.ReadFile(out)
		return err == nil && json.Unmarshal(data, &endpoint) == nil
	})
	hook := bearer(timeout(t), endpoint.Token)
	posted, err := f.client.PostUiRequest(hook, &pb.UiRequest{Kind: "do", ActionId: "layout.maximize"})
	if err != nil {
		t.Fatal("a safe action was refused:", err)
	}
	if got, _ := watch.Recv(); got.Id != posted.Id {
		t.Fatalf("delivered %+v", got)
	}
	if _, err = f.client.CompleteUiRequest(owner, &pb.CompleteUiRequestRequest{Id: posted.Id, Detail: "Maximized"}); err != nil {
		t.Fatal(err)
	}
	if done, err := f.client.WaitUiRequest(hook, &pb.WaitUiRequestRequest{Id: posted.Id, TimeoutMs: 1000}); err != nil || done.Detail != "Maximized" {
		t.Fatalf("wait own: %+v %v", done, err)
	}
	for _, refused := range []*pb.UiRequest{{Kind: "do", ActionId: "settings.open"}, {Kind: "do", ActionId: "chat.stop"}, {Kind: "open", SessionId: f.session}, {Kind: "shot"}} {
		if _, err = f.client.PostUiRequest(hook, refused); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s %s: %v, want PermissionDenied", refused.Kind, refused.ActionId, err)
		}
	}
	// The owner's requests are not the hook's to read.
	mine, err := f.client.PostUiRequest(owner, &pb.UiRequest{Kind: "shot"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = f.client.WaitUiRequest(hook, &pb.WaitUiRequestRequest{Id: mine.Id, TimeoutMs: 10}); err == nil {
		t.Fatal("a hook waited on the owner's request")
	}
	// Config files are the owner's.
	if _, err = f.client.GetConfigFiles(hook, &pb.GetConfigFilesRequest{}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("config: %v", err)
	}
}
