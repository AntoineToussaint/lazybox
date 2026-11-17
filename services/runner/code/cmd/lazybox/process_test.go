package main

import (
	"bytes"
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/process"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

func noRunner() (genconnect.RunnerServiceClient, error) {
	return nil, errors.New("this command must not need a Runner")
}

func runProcess(t *testing.T, args ...string) (string, error) {
	t.Helper()
	var out, errOut bytes.Buffer
	err := processVerb(context.Background(), args, &out, &errOut, noRunner)
	return out.String() + errOut.String(), err
}

// In a repository, `lazybox process validate` reads .lazybox/process.yaml and
// fails with each problem at its line.
func TestProcessValidateReadsTheRepositoryFile(t *testing.T) {
	dir := t.TempDir()
	if err := os.MkdirAll(filepath.Join(dir, ".lazybox", "roles"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, ".lazybox", "roles", "worker.md"), []byte("build"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, process.RepositoryFile), []byte("version: 1\nautonomy: 5\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Chdir(dir)
	out, err := runProcess(t, "validate")
	if err == nil || !strings.Contains(out, ".lazybox/process.yaml:2:11: autonomy: autonomy 5 is above your ceiling (4)") || !strings.HasPrefix(out, "preset solo-pair ← repository .lazybox/process.yaml\n") {
		t.Fatalf("%v\n%s", err, out)
	}
	if strings.Contains(out, "worker.md does not exist") {
		t.Fatalf("an instruction file in the checkout was reported missing:\n%s", out)
	}

	// The person's own ceiling lets it through, where isolation exists.
	user := filepath.Join(t.TempDir(), "process.yaml")
	if err = os.WriteFile(user, []byte("version: 1\nceiling: {autonomy: 5}\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(dir, process.RepositoryFile), []byte("version: 1\nautonomy: 5\nrequires: {isolation: container}\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if out, err = runProcess(t, "validate", "--user", user, "--isolation", "container"); err != nil || !strings.Contains(out, "valid · sha256:") {
		t.Fatalf("%v\n%s", err, out)
	}
}

func TestProcessShowExplainSchemaEvents(t *testing.T) {
	t.Chdir(t.TempDir())
	out, err := runProcess(t, "show", "--preset", "reviewed-team")
	if err != nil || !strings.HasPrefix(out, "# yaml-language-server: $schema="+process.SchemaFile+"\nversion: 1\nname: reviewed-team\n") {
		t.Fatalf("%v\n%s", err, out)
	}
	if out, err = runProcess(t, "show", "--json"); err != nil || !strings.Contains(out, `"name": "solo-pair"`) {
		t.Fatalf("%v\n%s", err, out)
	}
	if out, err = runProcess(t, "explain", "task.reported", "--tasks", "2"); err != nil || !strings.HasPrefix(out, "On task.reported, in order:\n 1. start verifier  [rules[0]]") {
		t.Fatalf("%v\n%s", err, out)
	}
	if _, err = runProcess(t, "explain"); err == nil {
		t.Fatal("explain without an event")
	}
	if _, err = runProcess(t, "explain", "task.exploded"); err == nil {
		t.Fatal("explained an unknown event")
	}
	if out, err = runProcess(t, "schema"); err != nil || out != string(process.Schema()) {
		t.Fatalf("schema: %v", err)
	}
	if out, err = runProcess(t, "events"); err != nil || !strings.Contains(out, "verdict.recorded") || !strings.Contains(out, "stall") {
		t.Fatalf("%v\n%s", err, out)
	}
	if _, err = runProcess(t, "frobnicate"); err == nil {
		t.Fatal("unknown subcommand accepted")
	}
}

type processClient struct {
	genconnect.RunnerServiceClient
	got     *pb.OrchestrationProcessRequest
	explain string
}

func (c *processClient) GetOrchestrationProcess(_ context.Context, r *connect.Request[pb.OrchestrationProcessRequest]) (*connect.Response[pb.OrchestrationProcess], error) {
	c.got = r.Msg
	return connect.NewResponse(&pb.OrchestrationProcess{
		Preset: "solo-pair", Yaml: "version: 1\n", Notice: "Several repositories in this workspace have a process file (a, b); choose the one that applies.",
		Layers: []*pb.OrchestrationProcessLayer{{Kind: "preset", Source: "solo-pair"}, {Kind: "repository", Source: "a/.lazybox/process.yaml"}},
		Issues: []*pb.OrchestrationProcessIssue{{Severity: "error", Path: "autonomy", Message: "too high", Source: "a/.lazybox/process.yaml", Line: 2, Column: 11}},
	}), nil
}

func (c *processClient) ExplainOrchestrationProcess(_ context.Context, r *connect.Request[pb.ExplainOrchestrationProcessRequest]) (*connect.Response[pb.OrchestrationProcessExplanation], error) {
	c.got, c.explain = r.Msg.Process, r.Msg.EventType
	return connect.NewResponse(&pb.OrchestrationProcessExplanation{Text: "On timer, in order:\n"}), nil
}

// With --workspace the same commands ask the Runner.
func TestProcessAgainstTheRunner(t *testing.T) {
	client := &processClient{}
	dial := func() (genconnect.RunnerServiceClient, error) { return client, nil }
	var out bytes.Buffer
	err := processVerb(context.Background(), []string{"validate", "--workspace", "w1", "--repository", "a", "--tasks", "3"}, &out, &out, dial)
	if err == nil || client.got.CollectionId != "w1" || client.got.Repository != "a" || client.got.Tasks != 3 {
		t.Fatalf("%v %+v", err, client.got)
	}
	want := "Several repositories in this workspace have a process file (a, b); choose the one that applies.\npreset solo-pair ← repository a/.lazybox/process.yaml\na/.lazybox/process.yaml:2:11: autonomy: too high\n"
	if out.String() != want {
		t.Fatalf("got\n%s\nwant\n%s", out.String(), want)
	}
	out.Reset()
	if err = processVerb(context.Background(), []string{"explain", "timer", "--workspace", "w1"}, &out, &out, dial); err != nil || client.explain != "timer" || out.String() != "On timer, in order:\n" {
		t.Fatalf("%v %q %q", err, client.explain, out.String())
	}
}
