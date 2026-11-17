package transport

import (
	"context"
	"slices"
	"testing"
	"time"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// A `lazybox run` follows its pipe: the person moves it to the background
// (owner only), it reports that and its port, Processes lists it with Stop,
// and a stop reaches the run through its next FollowPipe. An agent's
// credential follows, lists and stops its own commands only.
func TestBackgroundRunsAreFollowedMovedListedAndStoppedOnlyByTheirOwners(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	mine, _, token := f.open(t)
	other, _, otherToken := f.open(t)
	agent, owner := bearer(ctx, token), bearer(ctx, ownerToken)

	run, err := f.client.RegisterPipe(agent, &pb.RegisterPipeRequest{SessionId: mine.ID, Title: "npm run dev"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = f.client.BackgroundCommand(owner, &pb.CommandRequest{CommandId: run.Id}); status.Code(err) != codes.FailedPrecondition {
		t.Fatal("a pipe no lazybox run follows cannot move to the background", err)
	}
	if reply, err := f.client.FollowPipe(agent, &pb.FollowPipeRequest{CommandId: run.Id, Pid: 4242}); err != nil || reply.Stop || reply.Background {
		t.Fatal("first follow", reply, err)
	}
	listed := commandIn(t, f, owner, mine.ID, run.Id)
	if !listed.CanBackground || listed.Background || listed.Pid != 4242 || listed.StartedAtUnixMillis == 0 {
		t.Fatalf("a followed foreground run can move to the background: %+v", listed)
	}
	// The agent cannot move it: that is the person's.
	if _, err = f.client.BackgroundCommand(agent, &pb.CommandRequest{CommandId: run.Id}); status.Code(err) != codes.PermissionDenied {
		t.Fatal("an agent credential moved a run to the background", err)
	}
	// A waiting FollowPipe is woken by the person's request.
	woken := make(chan *pb.FollowPipeResponse, 1)
	go func() {
		reply, _ := f.client.FollowPipe(agent, &pb.FollowPipeRequest{CommandId: run.Id, Pid: 4242, WaitMs: 20000})
		woken <- reply
	}()
	time.Sleep(100 * time.Millisecond)
	if _, err = f.client.BackgroundCommand(owner, &pb.CommandRequest{CommandId: run.Id}); err != nil {
		t.Fatal(err)
	}
	select {
	case reply := <-woken:
		if reply == nil || !reply.Background || reply.Stop {
			t.Fatalf("the run was not asked to move: %+v", reply)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("FollowPipe was not woken by BackgroundCommand")
	}
	if _, err = f.client.FollowPipe(agent, &pb.FollowPipeRequest{CommandId: run.Id, Background: true, Pid: 4242, Ports: []uint32{5173}}); err != nil {
		t.Fatal(err)
	}
	listed = commandIn(t, f, owner, mine.ID, run.Id)
	if !listed.Background || listed.CanBackground || !slices.Equal(listed.Ports, []uint32{5173}) {
		t.Fatalf("background run with its port: %+v", listed)
	}
	inventory, err := f.client.ListProcesses(owner, &pb.ListProcessesRequest{})
	if err != nil {
		t.Fatal(err)
	}
	var process *pb.HostedProcess
	for _, p := range inventory.Processes {
		if p.CommandId == run.Id {
			process = p
		}
	}
	if process == nil || process.Id != "run:"+run.Id || process.Backend != "run" || process.Kind != "command" || process.SessionId != mine.ID || !slices.Equal(process.Ports, []uint32{5173}) {
		t.Fatalf("Processes lists the background run: %+v", process)
	}

	// Another conversation's agent can neither follow, list nor stop it.
	theirs := bearer(ctx, otherToken)
	if _, err = f.client.FollowPipe(theirs, &pb.FollowPipeRequest{CommandId: run.Id}); status.Code(err) != codes.PermissionDenied {
		t.Fatal("followed another session's run", err)
	}
	if _, err = f.client.CancelCommand(theirs, &pb.CommandRequest{CommandId: run.Id}); status.Code(err) != codes.PermissionDenied {
		t.Fatal("stopped another session's run", err)
	}
	if _, err = f.client.ListCommands(theirs, &pb.ListCommandsRequest{SessionId: mine.ID}); status.Code(err) != codes.PermissionDenied {
		t.Fatal("listed another session's commands", err)
	}
	if list, err := f.client.ListCommands(theirs, &pb.ListCommandsRequest{SessionId: other.ID}); err != nil || len(list.Commands) != 0 {
		t.Fatal("its own list", list, err)
	}

	// Its own agent stops it (`lazybox stop ID`), through Processes the
	// person would too: the run hears it on its next FollowPipe.
	if _, err = f.client.CancelCommand(agent, &pb.CommandRequest{CommandId: run.Id}); err != nil {
		t.Fatal("own stop refused", err)
	}
	reply, err := f.client.FollowPipe(agent, &pb.FollowPipeRequest{CommandId: run.Id, Background: true, Pid: 4242, WaitMs: 20000})
	if err != nil || !reply.Stop {
		t.Fatal("the stop did not reach the run", reply, err)
	}
	if _, err = f.client.FinishPipe(agent, &pb.FinishPipeRequest{CommandId: run.Id, Signal: "terminated", Report: &pb.CommandReport{ExitCode: -1}}); err != nil {
		t.Fatal(err)
	}
	listed = commandIn(t, f, owner, mine.ID, run.Id)
	if !listed.Done || listed.Signal != "terminated" || listed.Background || len(listed.Ports) != 0 {
		t.Fatalf("stopped run: %+v", listed)
	}
	if _, err = f.client.StopProcess(owner, &pb.StopProcessRequest{Id: "run:" + run.Id}); err != nil {
		t.Fatal("stopping a finished run is a no-op", err)
	}
	if _, err = f.client.StopProcess(owner, &pb.StopProcessRequest{Id: "run:missing"}); status.Code(err) != codes.NotFound {
		t.Fatal("an unknown run", err)
	}
}

func commandIn(t *testing.T, f *authorityFixture, owner context.Context, session, id string) *pb.CommandStatus {
	t.Helper()
	list, err := f.client.ListCommands(owner, &pb.ListCommandsRequest{SessionId: session})
	if err != nil {
		t.Fatal(err)
	}
	for _, c := range list.Commands {
		if c.Id == id {
			return c
		}
	}
	t.Fatalf("command %s not listed", id)
	return nil
}

// A terminal's own `lazybox run` follows and stops its run; nobody else's.
func TestTerminalCredentialFollowsAndStopsOnlyItsOwnRuns(t *testing.T) {
	f := newTerminalFixture(t)
	ctx := timeout(t)
	shell := bearer(ctx, f.terminalToken)
	run, err := f.client.RegisterPipe(shell, &pb.RegisterPipeRequest{CollectionId: f.mine.Id, Title: "make dev"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = f.client.FollowPipe(shell, &pb.FollowPipeRequest{CommandId: run.Id, Background: true}); err != nil {
		t.Fatal(err)
	}
	if _, err = f.client.CancelCommand(shell, &pb.CommandRequest{CommandId: run.Id}); err != nil {
		t.Fatal(err)
	}
	agentsRun, err := f.client.RegisterPipe(bearer(ctx, f.agentToken), &pb.RegisterPipeRequest{SessionId: f.conversation.ID, Title: "agent's"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = f.client.CancelCommand(shell, &pb.CommandRequest{CommandId: agentsRun.Id}); status.Code(err) != codes.PermissionDenied {
		t.Fatal("a terminal stopped an agent's run", err)
	}
	if _, err = f.client.FollowPipe(shell, &pb.FollowPipeRequest{CommandId: agentsRun.Id}); status.Code(err) != codes.PermissionDenied {
		t.Fatal("a terminal followed an agent's run", err)
	}
}

// The person's queued messages are on the session, and one can be sent now:
// the turn is interrupted and that message goes next.
func TestQueuedMessagesAreListedAndSentNow(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	e, fake, _ := f.open(t)
	owner := bearer(ctx, ownerToken)
	send := func(id, text string) {
		t.Helper()
		if _, err := f.client.Apply(owner, &pb.ApplyRequest{SessionId: e.ID, OperationId: id, Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: text}}}); err != nil {
			t.Fatal(err)
		}
	}
	send("first", "start the server")
	<-fake.Session.Applied
	send("second", "also check the logs")
	got, err := f.client.GetSession(owner, &pb.SessionRequest{SessionId: e.ID})
	if err != nil {
		t.Fatal(err)
	}
	if q := got.Session.QueuedMessages; len(q) != 1 || q[0].OperationId != "second" || q[0].Text != "also check the logs" || q[0].QueuedAtUnixMillis == 0 || got.Session.Queued != 1 {
		t.Fatalf("queued messages: %+v", got.Session.QueuedMessages)
	}
	deliver := func(id, how string) error {
		_, err := f.client.Apply(owner, &pb.ApplyRequest{SessionId: e.ID, OperationId: "deliver-" + id + how, Operation: &pb.ApplyRequest_DeliverQueued{DeliverQueued: &pb.DeliverQueued{OperationId: id, How: how}}})
		return err
	}
	if err = deliver("missing", "now"); status.Code(err) != codes.FailedPrecondition {
		t.Fatal("an unknown message", err)
	}
	if err = deliver("second", "steer"); err == nil {
		t.Fatal("this provider cannot steer")
	}
	if err = deliver("second", "now"); err != nil {
		t.Fatal(err)
	}
	select {
	case op := <-fake.Session.Applied:
		if op.Kind != "interrupt" {
			t.Fatalf("send now interrupts first: %+v", op)
		}
	case <-ctx.Done():
		t.Fatal("no interrupt")
	}
}
