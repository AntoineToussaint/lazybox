package main

import (
	"bytes"
	"context"
	"errors"
	"os"
	"strings"
	"testing"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// workspaceConsole stands in for the Runner behind a workspace terminal's
// credential: it records what was registered and can refuse a share.
type workspaceConsole struct {
	recordingConsole
	registered  []*pb.RegisterPipeRequest
	refuseShare error
}

func (c *workspaceConsole) RegisterPipe(_ context.Context, r *connect.Request[pb.RegisterPipeRequest]) (*connect.Response[pb.CommandStatus], error) {
	c.registered = append(c.registered, r.Msg)
	if r.Msg.ShareWith != "" && c.refuseShare != nil {
		return nil, c.refuseShare
	}
	return connect.NewResponse(&pb.CommandStatus{Id: "workspace-run"}), nil
}

// goTestOutput is far more than an agent should read, with one failure.
func goTestOutput() string {
	var b strings.Builder
	for i := range 3000 {
		b.WriteString("=== RUN   TestPass")
		b.WriteString(strings.Repeat("x", i%7))
		b.WriteString("\n--- PASS: TestPass (0.00s)\n")
	}
	b.WriteString("=== RUN   TestBroken\n    parser_test.go:42: want 3, got 4\n--- FAIL: TestBroken (0.00s)\nFAIL\nFAIL\texample.com/parser\t0.2s\n")
	return b.String()
}

func TestLogFromAWorkspaceTerminalShowsEverythingAndKeepsTheCompactView(t *testing.T) {
	c := &workspaceConsole{}
	text := goTestOutput()
	var stdout, stderr bytes.Buffer
	dir := t.TempDir()
	if err := publish(context.Background(), c, pipeTarget{collection: "workspace-1"}, "go test ./...", dir, strings.NewReader(text), &stdout, &stderr); err != nil {
		t.Fatal(err)
	}
	if stdout.String() != text {
		t.Fatal("the person's terminal must receive every byte")
	}
	if c.published.String() != text {
		t.Fatal("the console must receive every byte")
	}
	if len(c.registered) != 1 || c.registered[0].SessionId != "" || c.registered[0].CollectionId != "workspace-1" || c.registered[0].ShareWith != "" {
		t.Fatalf("a terminal run belongs to its workspace: %+v", c.registered)
	}
	r := c.finished.GetReport()
	if r == nil || !r.Piped || r.OutputBytes != uint64(len(text)) || r.Filter != "go-test" {
		t.Fatalf("report: %+v", r)
	}
	if r.AgentBytes != uint64(len(r.AgentView)) || r.AgentBytes*10 > r.OutputBytes || !strings.Contains(r.AgentView, "want 3, got 4") {
		t.Fatalf("the agent view must be compact and keep the failure (%d of %d bytes): %q", r.AgentBytes, r.OutputBytes, r.AgentView)
	}
	if !strings.HasPrefix(r.AgentView, "[lazybox] go test ./... → piped from a terminal, exit status unknown") || !strings.Contains(r.AgentView, r.OutputPath) || !strings.HasPrefix(r.OutputPath, dir) {
		t.Fatalf("the view must lead with the outcome and name the full output: %q", r.AgentView)
	}
	if saved, err := os.ReadFile(r.OutputPath); err != nil || string(saved) != text {
		t.Fatalf("full output file: %v", err)
	}
	if !strings.Contains(stderr.String(), `"go test ./..." is in the workspace console`) {
		t.Fatalf("the person is told where the run went: %q", stderr.String())
	}
}

func TestLogFromAWorkspaceTerminalNeverSwallowsTheProducersOutput(t *testing.T) {
	text := strings.Repeat("producer output\n", 4000)
	for _, stage := range []string{"register", "publish"} {
		t.Run(stage, func(t *testing.T) {
			c := &pipeClient{}
			if stage == "register" {
				c.registerError = errors.New("registration failed")
			} else {
				c.publishError = errors.New("publication failed")
			}
			var stdout, stderr bytes.Buffer
			err := publish(context.Background(), c, pipeTarget{collection: "workspace-1"}, "", t.TempDir(), strings.NewReader(text), &stdout, &stderr)
			if stdout.String() != text || err == nil {
				t.Fatalf("output swallowed or failure hidden: %v", err)
			}
			if stage == "publish" && (c.finished == nil || c.finished.CaptureError == "") {
				t.Fatalf("capture completion not reported: %+v", c.finished)
			}
		})
	}
}

func TestLogShareToAnUnknownConversationStillPublishesUnshared(t *testing.T) {
	c := &workspaceConsole{refuseShare: errors.New(`no conversation "parsr" in this workspace; choose one of: "parser fix" (c1)`)}
	var stdout, stderr bytes.Buffer
	if err := publish(context.Background(), c, pipeTarget{collection: "workspace-1", to: "parsr"}, "make", t.TempDir(), strings.NewReader("built\n"), &stdout, &stderr); err != nil {
		t.Fatal(err)
	}
	if stdout.String() != "built\n" || c.published.String() != "built\n" {
		t.Fatal("output lost")
	}
	if len(c.registered) != 2 || c.registered[0].ShareWith != "parsr" || c.registered[1].ShareWith != "" {
		t.Fatalf("expected a refused share, then an unshared run: %+v", c.registered)
	}
	if !strings.Contains(stderr.String(), `not shared: no conversation "parsr"`) || !strings.Contains(stderr.String(), `"parser fix"`) {
		t.Fatalf("the person must learn why and what to pick: %q", stderr.String())
	}
}

func TestLogSharesWithTheNamedConversation(t *testing.T) {
	c := &workspaceConsole{}
	var stderr bytes.Buffer
	if err := publish(context.Background(), c, pipeTarget{collection: "workspace-1", to: "parser fix"}, "make", t.TempDir(), strings.NewReader("built\n"), &bytes.Buffer{}, &stderr); err != nil {
		t.Fatal(err)
	}
	if len(c.registered) != 1 || c.registered[0].ShareWith != "parser fix" {
		t.Fatalf("%+v", c.registered)
	}
	if !strings.Contains(stderr.String(), `message box of "parser fix" for you to send`) {
		t.Fatalf("%q", stderr.String())
	}
}

func TestAnAgentCannotShareItsOwnRun(t *testing.T) {
	c := &workspaceConsole{}
	var stdout bytes.Buffer
	err := publish(context.Background(), c, pipeTarget{session: "execution", to: "other"}, "x", "", strings.NewReader("kept\n"), &stdout, &bytes.Buffer{})
	if err == nil || stdout.String() != "kept\n" || len(c.registered) != 0 {
		t.Fatalf("%v %q %+v", err, stdout.String(), c.registered)
	}
	marker := t.TempDir() + "/ran"
	err = runCommand(context.Background(), c, nil, runOptions{session: "execution", to: "other", outputDir: t.TempDir()}, []string{"sh", "-c", "touch " + marker}, &bytes.Buffer{}, &bytes.Buffer{})
	if _, statErr := os.Stat(marker); err == nil || !os.IsNotExist(statErr) || len(c.registered) != 0 {
		t.Fatal("run --to from an agent must be refused before running", err)
	}
}

func TestRunFromAWorkspaceTerminalStreamsToThePersonAndKeepsTheCompactView(t *testing.T) {
	c := &workspaceConsole{}
	var stdout, stderr bytes.Buffer
	err := runCommand(context.Background(), c, nil, runOptions{collection: "workspace-1", outputDir: t.TempDir()}, []string{"sh", "-c", "echo visible; echo broken >&2; exit 3"}, &stdout, &stderr)
	var exit processExit
	if !errors.As(err, &exit) || exit != 3 {
		t.Fatalf("exit status not preserved: %v", err)
	}
	if stdout.String() != "visible\n" || !strings.HasPrefix(stderr.String(), "broken\n") {
		t.Fatalf("the person sees the command's own streams, not the agent view: %q %q", stdout.String(), stderr.String())
	}
	if len(c.registered) != 1 || c.registered[0].CollectionId != "workspace-1" || c.registered[0].SessionId != "" {
		t.Fatalf("%+v", c.registered)
	}
	r := c.finished.GetReport()
	if r == nil || r.Piped || r.ExitCode != 3 || !strings.HasPrefix(r.AgentView, "[lazybox] sh -c echo visible; echo broken >&2; exit 3 → exit 3") || !strings.Contains(r.AgentView, "broken") {
		t.Fatalf("report: %+v", r)
	}
	if !strings.Contains(stderr.String(), "is in the workspace console") {
		t.Fatalf("%q", stderr.String())
	}
}

func TestRunShareToAnUnknownConversationRunsNothing(t *testing.T) {
	c := &workspaceConsole{refuseShare: errors.New("no conversation")}
	marker := t.TempDir() + "/ran"
	err := runCommand(context.Background(), c, nil, runOptions{collection: "workspace-1", to: "nope", outputDir: t.TempDir()}, []string{"sh", "-c", "touch " + marker}, &bytes.Buffer{}, &bytes.Buffer{})
	if err == nil || !strings.Contains(err.Error(), "nothing was run") {
		t.Fatal(err)
	}
	if _, statErr := os.Stat(marker); !os.IsNotExist(statErr) {
		t.Fatal("the command ran although its share was refused")
	}
}
