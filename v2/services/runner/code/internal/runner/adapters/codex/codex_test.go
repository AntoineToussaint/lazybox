package codex

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
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

func TestInputsDeliverGenericFileReferencesAlongsideNativeImages(t *testing.T) {
	op := agent.Operation{Text: "Read these", Files: []agent.File{{Name: "source.go", Path: "/resolved/attachment/source.go", MediaType: "text/plain", Size: 10}}, Images: []agent.Image{{MediaType: "image/png", Data: "image-data"}}}
	data, err := json.Marshal(inputs(op))
	if err != nil {
		t.Fatal(err)
	}
	var blocks []map[string]string
	if json.Unmarshal(data, &blocks) != nil || len(blocks) != 2 || !strings.Contains(blocks[0]["text"], "/resolved/attachment/source.go") || blocks[1]["type"] != "image" || blocks[1]["url"] != "data:image/png;base64,image-data" {
		t.Fatalf("missing generic or image input: %s", data)
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
