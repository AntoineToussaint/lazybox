package main

import (
	"bytes"
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// checkConsole accepts or refuses a run's criterion and answers its finish with a delivery note.
// unavailable answers that many finishes as a restarting Runner would; old is a
// Runner that ignores the criterion.
type checkConsole struct {
	pipeClient
	request     *pb.RegisterPipeRequest
	refuse      error
	note        string
	unavailable int
	finishes    int
	old         bool
}

func (c *checkConsole) RegisterPipe(_ context.Context, r *connect.Request[pb.RegisterPipeRequest]) (*connect.Response[pb.CommandStatus], error) {
	c.request = r.Msg
	if c.refuse != nil {
		return nil, c.refuse
	}
	status := &pb.CommandStatus{Id: "command", Check: r.Msg.Check + r.Msg.Needs}
	if c.old {
		status.Check = ""
	}
	return connect.NewResponse(status), nil
}
func (c *checkConsole) FinishPipe(_ context.Context, r *connect.Request[pb.FinishPipeRequest]) (*connect.Response[pb.CommandStatus], error) {
	c.finished = r.Msg
	c.finishes++
	if c.finishes <= c.unavailable {
		return nil, connect.NewError(connect.CodeUnavailable, errors.New("the Runner is restarting"))
	}
	return connect.NewResponse(&pb.CommandStatus{Delivery: c.note}), nil
}

func TestCheckRunSendsItsCriterionAndReportsWhatWasRecorded(t *testing.T) {
	c := &checkConsole{note: "check api: passed on candidate:1 (spec catalog v3)"}
	var stdout bytes.Buffer
	if err := runCommand(context.Background(), c, nil, runOptions{session: "execution", outputDir: t.TempDir(), check: "api"}, []string{"true"}, &stdout, &bytes.Buffer{}); err != nil {
		t.Fatal(err)
	}
	if c.request.Check != "api" || c.request.Needs != "" {
		t.Fatalf("registered %+v", c.request)
	}
	if !strings.Contains(stdout.String(), "[lazybox] check api: passed on candidate:1 (spec catalog v3)\n") {
		t.Fatalf("the agent was not told what was recorded: %q", stdout.String())
	}
	c = &checkConsole{}
	if err := runCommand(context.Background(), c, nil, runOptions{session: "execution", outputDir: t.TempDir(), needs: "generate"}, []string{"true"}, &bytes.Buffer{}, &bytes.Buffer{}); err != nil || c.request.Needs != "generate" {
		t.Fatal(c.request, err)
	}
}

// Evidence that cannot be recorded is refused before the command runs.
func TestCheckRunIsRefusedBeforeItRuns(t *testing.T) {
	dir := t.TempDir()
	marker := filepath.Join(dir, "ran")
	argv := []string{"touch", marker}
	for name, tc := range map[string]struct {
		console     console
		unavailable error
		o           runOptions
		want        string
	}{
		"no bound spec":  {&checkConsole{refuse: errors.New("no spec is bound to this conversation")}, nil, runOptions{session: "execution", check: "api"}, "no spec is bound"},
		"no Runner":      {nil, errors.New("no Runner endpoint"), runOptions{session: "execution", check: "api"}, "no Runner endpoint"},
		"workspace run":  {&checkConsole{}, nil, runOptions{collection: "workspace", needs: "generate"}, "from its agent"},
		"no destination": {&checkConsole{}, nil, runOptions{check: "api"}, "no Lazybox session"},
	} {
		t.Run(name, func(t *testing.T) {
			tc.o.outputDir = dir
			err := runCommand(context.Background(), tc.console, tc.unavailable, tc.o, argv, &bytes.Buffer{}, &bytes.Buffer{})
			if err == nil || !strings.HasPrefix(err.Error(), "not run: ") || !strings.Contains(err.Error(), tc.want) {
				t.Fatalf("got %v", err)
			}
			if _, statErr := os.Stat(marker); statErr == nil {
				t.Fatal("the command ran")
			}
		})
	}
}

type bindingClient struct{ request *pb.BindDeliveryRequest }

func (c *bindingClient) BindDelivery(_ context.Context, r *connect.Request[pb.BindDeliveryRequest]) (*connect.Response[pb.DeliveryBinding], error) {
	c.request = r.Msg
	return connect.NewResponse(&pb.DeliveryBinding{ArtifactId: r.Msg.ArtifactId, Version: r.Msg.Version, Attempt: "conversation:c/2", Criteria: []string{"[api, required] An authorized client reads the item"}}), nil
}

func TestDeliveryBindPinsAVersionedSpec(t *testing.T) {
	c := &bindingClient{}
	var stdout bytes.Buffer
	if err := deliveryCommand(context.Background(), c, "execution", []string{"bind", "spec-catalog@3"}, &stdout, &bytes.Buffer{}); err != nil {
		t.Fatal(err)
	}
	if c.request.SessionId != "execution" || c.request.ArtifactId != "spec-catalog" || c.request.Version != 3 {
		t.Fatal(c.request)
	}
	if out := stdout.String(); !strings.Contains(out, "Bound spec spec-catalog v3") || !strings.Contains(out, "- [api, required]") {
		t.Fatal(out)
	}
	for _, args := range [][]string{{"bind", "spec-catalog"}, {"bind", "spec-catalog@0"}, {"bind", "@3"}, {"unbind", "spec@1"}, {"bind"}} {
		if err := deliveryCommand(context.Background(), &bindingClient{}, "execution", args, &bytes.Buffer{}, &bytes.Buffer{}); err == nil {
			t.Fatalf("accepted %v", args)
		}
	}
	if err := deliveryCommand(context.Background(), &bindingClient{}, "", []string{"bind", "spec@1"}, &bytes.Buffer{}, &bytes.Buffer{}); err == nil {
		t.Fatal("bound with no session")
	}
}

// An older Runner ignores --check: the run would prove nothing, so it is not run.
func TestCheckRunIsRefusedByARunnerThatRecordsNoChecks(t *testing.T) {
	dir := t.TempDir()
	marker := filepath.Join(dir, "ran")
	c := &checkConsole{old: true}
	err := runCommand(context.Background(), c, nil, runOptions{session: "execution", outputDir: dir, check: "api"}, []string{"touch", marker}, &bytes.Buffer{}, &bytes.Buffer{})
	if err == nil || !strings.Contains(err.Error(), "records no delivery checks") {
		t.Fatal(err)
	}
	if _, statErr := os.Stat(marker); statErr == nil {
		t.Fatal("the command ran")
	}
	if c.finished == nil || c.finished.CaptureError == "" {
		t.Fatal("the registered pipe was left open")
	}
}

// A check's completion is offered again while the Runner restarts.
func TestCheckRunRetriesItsCompletion(t *testing.T) {
	defer func(d time.Duration) { finishRetry = d }(finishRetry)
	finishRetry = 30 * time.Second
	c := &checkConsole{unavailable: 2, note: "check api: passed on candidate:1 (spec catalog v3)"}
	var stdout bytes.Buffer
	if err := runCommand(context.Background(), c, nil, runOptions{session: "execution", outputDir: t.TempDir(), check: "api"}, []string{"true"}, &stdout, &bytes.Buffer{}); err != nil {
		t.Fatal(err)
	}
	if c.finishes != 3 || !strings.Contains(stdout.String(), "check api: passed") {
		t.Fatalf("%d finishes: %q", c.finishes, stdout.String())
	}
	plain := &checkConsole{unavailable: 1}
	_ = runCommand(context.Background(), plain, nil, runOptions{session: "execution", outputDir: t.TempDir()}, []string{"true"}, &bytes.Buffer{}, &bytes.Buffer{})
	if plain.finishes != 1 {
		t.Fatalf("a run with no check retried %d times", plain.finishes)
	}
}

// A check run the CLI interrupts reports a signal, so it asserts nothing.
func TestInterruptedCheckRunReportsItsSignal(t *testing.T) {
	c := &checkConsole{}
	ctx, cancel := context.WithCancel(context.Background())
	go func() { time.Sleep(300 * time.Millisecond); cancel() }()
	// The command traps the interrupt and exits 1, as test runners do.
	_ = runCommand(ctx, c, nil, runOptions{session: "execution", outputDir: t.TempDir(), check: "api"}, []string{"sh", "-c", "trap 'exit 1' INT TERM; sleep 5 & wait"}, &bytes.Buffer{}, &bytes.Buffer{})
	if c.finished == nil || c.finished.Signal == "" {
		t.Fatalf("finish carried no signal: %+v", c.finished)
	}
}

func TestCheckRunCannotGoToTheBackground(t *testing.T) {
	err := runVerb(context.Background(), "", []string{"--check", "api", "--background", "--", "true"}, &bytes.Buffer{}, &bytes.Buffer{})
	if err == nil || !strings.Contains(err.Error(), "cannot go --background") {
		t.Fatal(err)
	}
}
