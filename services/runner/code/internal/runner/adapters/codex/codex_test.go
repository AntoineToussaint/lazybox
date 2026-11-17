package codex

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

func TestDefaultThreadParametersPreserveNativePermissions(t *testing.T) {
	params := threadParams(agent.Open{Cwd: "/fixture/collection"})
	for _, name := range []string{"approvalPolicy", "approvalsReviewer", "sandbox", "config", "model"} {
		if _, exists := params[name]; exists {
			t.Fatalf("default startup overrides native %s", name)
		}
	}
	if params["cwd"] != "/fixture/collection" {
		t.Fatal("collection cwd lost")
	}
}

func TestExplicitThreadPermissionChoice(t *testing.T) {
	for _, permission := range []string{"ask", "read-only"} {
		params := threadParams(agent.Open{Cwd: "/fixture", Permission: permission})
		expected := "workspace-write"
		if permission == "read-only" {
			expected = "read-only"
		}
		if params["sandbox"] != expected {
			t.Fatal("explicit permission choice lost", params)
		}
	}
}

func TestInputsDeliverTextImagesAndFilesInOrder(t *testing.T) {
	paste := filepath.Join(t.TempDir(), "paste.txt")
	if err := os.WriteFile(paste, []byte("panic: boom\ngoroutine 1\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	op := agent.Operation{Text: "Read these", Files: []agent.File{
		{Name: "shot.png", MediaType: "image/png", Path: "/r/shot.png", Size: 10, Delivery: agent.DeliverImage},
		{Name: "Pasted text.txt", MediaType: "text/plain", Path: paste, Size: 24, Delivery: agent.DeliverText},
		{Name: "report.pdf", Path: "/resolved/attachment/report.pdf", MediaType: "application/pdf", Size: 10, Delivery: agent.DeliverFile},
	}, Images: []agent.Image{{MediaType: "image/png", Data: "image-data"}}}
	data, err := json.Marshal(inputs(op))
	if err != nil {
		t.Fatal(err)
	}
	var blocks []map[string]string
	if json.Unmarshal(data, &blocks) != nil || len(blocks) != 4 {
		t.Fatalf("inputs: %s", data)
	}
	// turn/start takes text and images: the text first, then each attachment
	// in order, a pasted text as a fenced block and a PDF by its path.
	if blocks[0]["text"] != "Read these" || blocks[1]["type"] != "image" || blocks[1]["url"] != "data:image/png;base64,image-data" ||
		blocks[2]["type"] != "text" || blocks[2]["text"] != "Attached file \"Pasted text.txt\" (2 lines):\n```\npanic: boom\ngoroutine 1\n```" ||
		blocks[3]["type"] != "text" || !strings.Contains(blocks[3]["text"], `"/resolved/attachment/report.pdf"`) {
		t.Fatalf("inputs out of order or dropped: %s", data)
	}
}

func TestGatewaySelectsHTTPProviderAndPreservesSubscriptionAuth(t *testing.T) {
	args := strings.Join(GatewayArgs(&agent.Gateway{ID: "meter", URL: "https://meter.test/cost/session/"}), " ")
	for _, required := range []string{`model_provider="lazybox_gateway"`, `base_url="https://meter.test/cost/session"`, `wire_api="responses"`, `requires_openai_auth=true`, `supports_websockets=false`} {
		if !strings.Contains(args, required) {
			t.Fatalf("missing %s: %s", required, args)
		}
	}
	if strings.Contains(args, "OPENAI_BASE_URL") || strings.Contains(args, "/responses\"") {
		t.Fatal("gateway bypass or duplicate endpoint suffix")
	}
	args = strings.Join(GatewayArgs(&agent.Gateway{ID: "meter", URL: "https://meter.test", TokenEnv: "BENCH_GATEWAY_TOKEN"}), " ")
	if !strings.Contains(args, `env_key="BENCH_GATEWAY_TOKEN"`) || !strings.Contains(args, `requires_openai_auth=false`) {
		t.Fatal(args)
	}
	if len(GatewayArgs(nil)) != 0 {
		t.Fatal("direct mode mutated configuration")
	}
}

func TestNamedCustomPermissionProfileNeverFallsBackToBroaderPolicy(t *testing.T) {
	_, err := (Adapter{Binary: "must-not-start"}).Open(context.Background(), agent.Open{ProviderSettings: json.RawMessage(`{"_namedPermissionProfile":"custom-deny-rules","sandboxPolicy":{"type":"workspaceWrite"}}`)})
	if !errors.Is(err, agent.ErrUnsupported) {
		t.Fatal("unverified named policy was replaced", err)
	}
}

func TestYOLOIsExplicitAndContextIsNativeDeveloperInstructions(t *testing.T) {
	p := threadParams(agent.Open{Cwd: "/fixture", Permission: "yolo", Context: "Lazybox context"})
	if p["approvalPolicy"] != "never" || p["sandbox"] != "danger-full-access" || p["developerInstructions"] != "Lazybox context" {
		t.Fatal(p)
	}
	p = threadParams(agent.Open{Cwd: "/fixture", Permission: "inherit"})
	if _, ok := p["sandbox"]; ok {
		t.Fatal("inherit overrode sandbox", p)
	}
	if _, ok := p["approvalPolicy"]; ok {
		t.Fatal("inherit overrode approvals", p)
	}
}

func TestLivePermissionChangesApplyToFollowingTurns(t *testing.T) {
	s := &session{}
	if !s.Capabilities().LivePermission {
		t.Fatal("Codex turns accept new approval and sandbox policies")
	}
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "p", Kind: agent.Permission, Permission: "yolo"}); err != nil {
		t.Fatal(err)
	}
	if string(s.overrides["approvalPolicy"]) != `"never"` || string(s.overrides["sandboxPolicy"]) != `{"type":"dangerFullAccess"}` {
		t.Fatalf("YOLO policies not kept for turn/start: %s", s.overrides)
	}
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "q", Kind: agent.Permission, Permission: "read-only"}); err != nil {
		t.Fatal(err)
	}
	if string(s.overrides["sandboxPolicy"]) != `{"type":"readOnly"}` {
		t.Fatalf("later change must replace the earlier one: %s", s.overrides)
	}
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "r", Kind: agent.Permission, Permission: "inherit"}); err == nil {
		t.Fatal("unknown mode accepted")
	}
}

func TestInstructionUpdateIsItsOwnInputAheadOfTheMessage(t *testing.T) {
	items := inputs(agent.Operation{Kind: agent.Message, Text: "hello", Context: "new instructions"})
	data, _ := json.Marshal(items)
	if string(data) != `[{"text":"new instructions","type":"text"},{"text":"hello","type":"text"}]` {
		t.Fatalf("%s", data)
	}
}
