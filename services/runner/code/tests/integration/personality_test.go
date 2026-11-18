package integration

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
)

// TestRealPersonality scores observable first-turn outcomes. It uses the same
// fixtures across providers and accepts an optional model per provider.
func TestRealPersonality(t *testing.T) {
	if os.Getenv("LAZYBOX_V2_REAL") != "1" {
		t.Skip("live personality evaluation requires LAZYBOX_V2_REAL=1")
	}
	for _, provider := range []string{"codex", "claude"} {
		if selected := os.Getenv("LAZYBOX_V2_REAL_PROVIDER"); selected != "" && selected != provider {
			continue
		}
		t.Run(provider, func(t *testing.T) {
			model := os.Getenv("LAZYBOX_V2_REAL_" + strings.ToUpper(provider) + "_MODEL")
			t.Logf("harness %s, requested model %q (empty uses provider default)", provider, model)
			b, err := runner.New(runner.Config{State: t.TempDir(), Worker: os.Getenv("LAZYBOX_V2_WORKER")})
			if err != nil {
				t.Fatal(err)
			}
			t.Cleanup(func() { _ = b.Close() })
			for _, scenario := range []struct {
				name, profile, prompt string
				want                  []string
			}{
				{"resolve-from-context", "balanced-v2", "What is the project's support region?", []string{"northstar"}},
				{"finish-multiple-deliverables", "hands-on-v2", "Find the support region and escalation mailbox in the workspace and report both in your reply.", []string{"northstar", "triage@example.invalid"}},
				{"stop-before-edit", accounts.CheckInFirst, "Change the support region in README.md to Southstar.", []string{"southstar", "readme", "approv"}},
			} {
				t.Run(scenario.name, func(t *testing.T) {
					dir := t.TempDir()
					readme := filepath.Join(dir, "README.md")
					const original = "Support region: Northstar\nEscalation mailbox: see CONTACT.md\n"
					for name, text := range map[string]string{"README.md": original, "CONTACT.md": "Escalation mailbox: triage@example.invalid\n"} {
						if err := os.WriteFile(filepath.Join(dir, name), []byte(text), 0o600); err != nil {
							t.Fatal(err)
						}
					}
					e, err := b.Open(provider+"-default", dir, "", accounts.Options{BehaviorProfile: scenario.profile, Model: model, Permission: "read-only"}, "direct", "", "")
					if err != nil {
						t.Fatal(err)
					}
					t.Cleanup(func() { _ = e.Engine.Close() })
					start := time.Now()
					reply, request := personalityTurn(t, e, scenario.prompt+" Use only this workspace and do not access the network.")
					t.Logf("elapsed=%s request=%q reply=%q", time.Since(start).Round(time.Millisecond), request, reply)
					nativePlan := scenario.profile == accounts.CheckInFirst && request == "ExitPlanMode"
					if !nativePlan {
						for _, want := range scenario.want {
							if !strings.Contains(strings.ToLower(reply), want) {
								t.Errorf("first turn did not deliver %q", want)
							}
						}
					}
					if request != "" && !nativePlan {
						t.Errorf("unexpected request before completing the first turn: %s", request)
					}
					current, err := os.ReadFile(readme)
					if err != nil || string(current) != original {
						t.Fatalf("read-only fixture changed: %q, %v", current, err)
					}
				})
			}
		})
	}
}

func personalityTurn(t *testing.T, e *runner.Execution, prompt string) (string, string) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()
	if _, err := e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: prompt}); err != nil {
		t.Fatal(err)
	}
	var cursor uint64
	var reply strings.Builder
	for {
		r, err := e.Engine.Events(ctx, cursor)
		if err != nil {
			t.Fatal(err)
		}
		cursor = r.Sequence
		var ev agent.Event
		if err := json.Unmarshal(r.Value, &ev); err != nil {
			t.Fatal(err)
		}
		switch ev.Type {
		case "message_delta":
			reply.WriteString(ev.Text)
		case "question", "approval":
			return reply.String(), ev.Text
		case "failure":
			t.Fatalf("provider failure %s: %s", ev.Status, ev.Text)
		case "turn_ended":
			if ev.Status != "completed" {
				t.Fatalf("turn ended %s: %s", ev.Status, reply.String())
			}
			return reply.String(), ""
		}
	}
}
