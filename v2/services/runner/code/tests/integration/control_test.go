package integration

import (
	"context"
	"encoding/json"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/commands"
)

func TestRealCodexNativeControl(t *testing.T) {
	if os.Getenv("LAZYBOX_V2_REAL") != "1" {
		t.Skip("requires explicit live-provider opt-in")
	}
	if selected := os.Getenv("LAZYBOX_V2_REAL_PROVIDER"); selected != "" && selected != "codex" {
		t.Skip("different provider selected")
	}
	b, err := runner.New(runner.Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()
	plan, err := b.Open("codex-default", t.TempDir(), "", accounts.Options{Workflow: "plan"}, "direct", "", "")
	if err != nil {
		t.Fatal(err)
	}
	cursor := uint64(0)
	next := func(e *runner.Execution) agent.Event {
		t.Helper()
		r, err := e.Engine.Events(ctx, cursor)
		if err != nil {
			t.Fatal(err)
		}
		cursor = r.Sequence
		var ev agent.Event
		if err = json.Unmarshal(r.Value, &ev); err != nil {
			t.Fatal(err)
		}
		if ev.Type == "failure" {
			t.Fatalf("%s %s %s", ev.Status, ev.Text, ev.Data)
		}
		return ev
	}
	send := func(e *runner.Execution, text string) {
		t.Helper()
		if _, err := e.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Message, Text: text}); err != nil {
			t.Fatal(err)
		}
	}
	send(plan, "Use request_user_input to ask me one question about a color, with Blue and Red options. Wait for my answer and report the chosen color. Do not use other tools or delegate.")
	question := false
	var text strings.Builder
	for {
		ev := next(plan)
		if ev.Type == "question" {
			question = true
			var data struct {
				Questions []struct {
					ID string `json:"id"`
				} `json:"questions"`
			}
			if err = json.Unmarshal(ev.Data, &data); err != nil {
				t.Fatal(err)
			}
			answers := map[string][]string{}
			for _, q := range data.Questions {
				answers[q.ID] = []string{"Blue"}
			}
			if _, err = plan.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Answer, RequestID: ev.RequestID, Answers: answers}); err != nil {
				t.Fatal(err)
			}
			t.Log("native question answered by request identity")
		}
		if ev.Type == "message_delta" {
			text.WriteString(ev.Text)
		}
		if ev.Type == "turn_ended" {
			break
		}
	}
	if !question || !strings.Contains(strings.ToLower(text.String()), "blue") {
		t.Fatalf("question/answer not demonstrated: %s", text.String())
	}
	send(plan, "Use request_user_input to ask me one new question and wait for my answer. Do not use other tools or delegate.")
	for {
		if next(plan).Type == "question" {
			break
		}
	}
	send(plan, "Ignore the unanswered question and reply exactly QUEUED-CONTINUATION. No tools or delegation.")
	if _, err = plan.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Interrupt}); err != nil {
		t.Fatal(err)
	}
	interrupted := false
	text.Reset()
	for {
		ev := next(plan)
		if ev.Type == "turn_ended" {
			if ev.Status == "interrupted" {
				interrupted = true
			} else if ev.Status == "completed" {
				break
			}
		}
		if ev.Type == "message_delta" {
			text.WriteString(ev.Text)
		}
	}
	if !interrupted || !strings.Contains(text.String(), "QUEUED-CONTINUATION") {
		t.Fatalf("interrupt/queue not demonstrated: interrupted=%v %s", interrupted, text.String())
	}
	t.Log("native question cancellation, active-turn interruption and queued continuation verified")
	_ = plan.Engine.Close()
	execution, err := b.Open("codex-default", t.TempDir(), "", accounts.Options{}, "direct", "", "")
	if err != nil {
		t.Fatal(err)
	}
	cursor = 0
	send(execution, "Use your built-in shell command tool to write exactly NATIVE-APPROVAL into a new file named approval.txt in the current working directory. Do not use lazybox_command, other tools, or delegation. Then reply DONE.")
	nativeApproval := false
	for {
		ev := next(execution)
		if ev.Type == "approval" {
			if ev.Text != "item/commandExecution/requestApproval" && ev.Text != "item/fileChange/requestApproval" {
				t.Fatalf("expected a native provider approval, got %s", ev.Text)
			}
			nativeApproval = true
			if _, err = execution.Engine.Apply(ctx, agent.Operation{ID: commands.ID(), Kind: agent.Approve, RequestID: ev.RequestID, Decision: "accept"}); err != nil {
				t.Fatal(err)
			}
		}
		if ev.Type == "turn_ended" {
			break
		}
	}
	if !nativeApproval {
		t.Fatal("native provider approval was not demonstrated")
	}
	data, err := os.ReadFile(execution.Cwd + "/approval.txt")
	if err != nil || strings.TrimSpace(string(data)) != "NATIVE-APPROVAL" {
		t.Fatalf("native approval did not execute: %q %v", data, err)
	}
	t.Log("native command approval replied through actual app-server approval protocol")
}
