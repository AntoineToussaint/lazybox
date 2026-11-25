package codex

import (
	"context"
	"encoding/json"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

func toolNames(purpose string) []string {
	var names []string
	for _, t := range dynamicTools(purpose, false) {
		names = append(names, t["name"].(string))
	}
	return names
}

// Only the help assistant gets the documentation and settings tools; a coding agent keeps the
// managed-command tool and never gets them; other assistants get no tools.
func TestDynamicToolsFollowThePurpose(t *testing.T) {
	if got := strings.Join(toolNames("help"), ","); got != agent.DocsSearchTool+","+agent.DocsReadTool+","+agent.SettingsReadTool+","+agent.SettingsUpdateTool {
		t.Fatalf("help tools: %s", got)
	}
	for _, purpose := range []string{"", "coding"} {
		if got := strings.Join(toolNames(purpose), ","); got != agent.CommandTool+","+agent.ReviewFindingTool {
			t.Fatalf("%q tools: %s", purpose, got)
		}
		if toolOffered(purpose, agent.SettingsUpdateTool) || toolOffered(purpose, agent.SettingsReadTool) {
			t.Fatalf("%q is offered the settings tools", purpose)
		}
	}
	for _, purpose := range []string{"planning", "summary"} {
		if tools := dynamicTools(purpose, false); tools != nil {
			t.Fatalf("%s gets tools: %v", purpose, tools)
		}
	}
	if toolOffered("help", agent.CommandTool) {
		t.Fatal("help assistant is offered managed commands")
	}
	if toolApproval(agent.SettingsUpdateTool, nil) == "" || toolApproval(agent.CommandTool, nil) == "" {
		t.Fatal("a tool with effects runs without approval")
	}
	// Reading the documentation has nothing to approve, and is offered only to help.
	if toolApproval(agent.DocsSearchTool, nil) != "" || toolApproval(agent.DocsReadTool, nil) != "" || toolOffered("", agent.DocsReadTool) || toolOffered("planning", agent.DocsSearchTool) {
		t.Fatal("documentation tools")
	}
	for _, tool := range dynamicTools("help", false)[:2] {
		var schema map[string]any
		if err := json.Unmarshal(tool["inputSchema"].(json.RawMessage), &schema); err != nil {
			t.Fatalf("%s schema: %v", tool["name"], err)
		}
	}
	var schema map[string]any
	if err := json.Unmarshal([]byte(settingsChangeSchema), &schema); err != nil {
		t.Fatal(err)
	}
}

func openHelp(t *testing.T, tools func(context.Context, string, json.RawMessage) (string, error)) agent.Session {
	t.Helper()
	self, _ := os.Executable()
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Purpose: "help", Tools: tools, Host: &agent.Hosting{Root: fixtureRoot(t)}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = s.Close() })
	if _, err = s.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "settings"}); err != nil {
		t.Fatal(err)
	}
	return s
}

// Reading the settings has no effect, so it runs without an approval card.
func TestHelpAssistantReadsSettingsWithoutApproval(t *testing.T) {
	t.Setenv("FIXTURE_TOOL_NAME", agent.SettingsReadTool)
	s := openHelp(t, func(_ context.Context, name string, _ json.RawMessage) (string, error) {
		return `{"revision":3,"called":"` + name + `"}`, nil
	})
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	for {
		ev, err := s.Receive(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if ev.Type == "approval" || ev.Type == "request_resolved" {
			t.Fatalf("reading settings asked for approval: %+v", ev)
		}
		if ev.Type == "message_delta" {
			if ev.Text != `success=true {"revision":3,"called":"lazybox_get_settings"}` {
				t.Fatalf("read reply: %q", ev.Text)
			}
			return
		}
	}
}

// A settings update is always put to the user, titled for what it does, and a
// declined one never reaches the Runner.
func TestHelpAssistantSettingsUpdateIsAlwaysApproved(t *testing.T) {
	t.Setenv("FIXTURE_TOOL_NAME", agent.SettingsUpdateTool)
	ran := make(chan string, 1)
	s := openHelp(t, func(_ context.Context, name string, _ json.RawMessage) (string, error) {
		ran <- name
		return "{}", nil
	})
	approval := receive(t, s, "approval")
	if approval.Text != "Change Lazybox settings" || approval.RequestID != "n:41" {
		t.Fatalf("settings approval: %+v", approval)
	}
	select {
	case name := <-ran:
		t.Fatalf("%s ran before approval", name)
	default:
	}
	if _, err := s.Apply(context.Background(), agent.Operation{ID: "no", Kind: agent.Approve, RequestID: "n:41", Decision: "decline"}); err != nil {
		t.Fatal(err)
	}
	if reply := receive(t, s, "message_delta"); reply.Text != "success=false The user declined this settings change; nothing was saved." {
		t.Fatalf("declined reply: %q", reply.Text)
	}
	select {
	case name := <-ran:
		t.Fatalf("declined %s ran", name)
	default:
	}
}

// The help assistant cannot reach managed commands even if the app-server
// calls the tool by name.
func TestHelpAssistantCannotCallManagedCommands(t *testing.T) {
	ran := make(chan string, 1)
	s := openHelp(t, func(_ context.Context, name string, _ json.RawMessage) (string, error) {
		ran <- name
		return "{}", nil
	})
	if reply := receive(t, s, "message_delta"); !strings.HasPrefix(reply.Text, "success=false") {
		t.Fatalf("managed command call: %q", reply.Text)
	}
	select {
	case name := <-ran:
		t.Fatalf("help assistant ran %s", name)
	default:
	}
}

func TestReviewCommandAllowed(t *testing.T) {
	for action, want := range map[string]bool{"start": false, "interactive": false, "start_action": false, "read": true, "wait": true, "verify": true} {
		if got := agent.ReviewCommandAllowed(json.RawMessage(`{"action":"` + action + `"}`)); got != want {
			t.Errorf("%s: got %t", action, got)
		}
	}
}
