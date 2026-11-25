package transport

import (
	"context"
	"encoding/json"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// twoSubscriptions registers two isolated codex subscriptions on b.
func twoSubscriptions(t *testing.T, b *runner.Bench) rotatingFixture {
	t.Helper()
	root := filepath.Join(b.Config.State, "accounts")
	for _, id := range []string{"a", "b"} {
		if err := accounts.StoreProfile(b.Config.State, accounts.Profile{ID: id, Provider: "codex", Isolation: "environment", Home: filepath.Join(root, id, "codex")}); err != nil {
			t.Fatal(err)
		}
	}
	if err := b.ReloadProfiles(); err != nil {
		t.Fatal(err)
	}
	adapter := rotatingFixture{opened: make(chan *rotatingNative, 4)}
	b.Adapters["codex"] = adapter
	return adapter
}

func until(t *testing.T, what string, ok func() bool) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for !ok() {
		if time.Now().After(deadline) {
			t.Fatal(what)
		}
		time.Sleep(time.Millisecond)
	}
}

func delivered(t *testing.T, n *rotatingNative) agent.Operation {
	t.Helper()
	select {
	case op := <-n.Applied:
		return op
	case <-time.After(5 * time.Second):
		t.Fatal("nothing delivered")
	}
	return agent.Operation{}
}

func message(id, text string) *pb.ApplyRequest {
	return &pb.ApplyRequest{OperationId: id, Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: text}}}
}

// A conversation that started without automatic switching stops at a limit.
// Switching continues the SAME conversation (execution, journal, receipts) on
// the other subscription: the interrupted request through the checkpoint, and
// a message sent while it waited exactly once, after it. "Always switch
// automatically" is saved with the conversation.
func TestSwitchSubscriptionContinuesTheSameConversationAndSendsOnce(t *testing.T) {
	service, _ := collectionService(t)
	b := service.Bench
	adapter := twoSubscriptions(t, b)
	ctx := context.Background()
	opened, err := service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "a", Cwd: t.TempDir(), Options: &pb.RunnerOptions{NoRotation: true}})
	if err != nil {
		t.Fatal(err)
	}
	id := opened.Session.Id
	first := <-adapter.opened
	send := func(op *pb.ApplyRequest) *pb.Receipt {
		t.Helper()
		op.SessionId = id
		r, err := service.Apply(ctx, op)
		if err != nil {
			t.Fatal(err)
		}
		return r
	}
	send(message("ask", "Fix the flaky login test"))
	if op := delivered(t, first); op.ID != "ask" {
		t.Fatalf("%+v", op)
	}
	first.Events <- agent.Event{Type: "turn_started"}
	first.Events <- agent.Event{Type: "failure", Status: "exhausted", Text: "rate_limit", Account: &agent.AccountObservation{State: "exhausted", ResetAt: time.Now().Add(time.Hour).Unix()}}
	first.Events <- agent.Event{Type: "turn_ended", Status: "failed"}
	e, err := b.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	until(t, "limit not observed", func() bool { s := e.Engine.Snapshot(); return s.Fault == "exhausted" && !s.Busy })
	// A follow-up sent while waiting is accepted and waits; a client retry of
	// the same operation is the same receipt, never a second message.
	if r := send(message("follow-up", "Also check the CI logs")); r.Delivery != "accepted" {
		t.Fatalf("%+v", r)
	}
	send(message("follow-up", "Also check the CI logs"))
	if q := e.Engine.Snapshot().Queued; q != 1 || len(first.Applied) != 0 {
		t.Fatalf("queued %d; delivered to the exhausted subscription: %d", q, len(first.Applied))
	}
	if _, err = service.SwitchSubscription(ctx, &pb.SwitchSubscriptionRequest{SessionId: id, ProfileId: "missing"}); err == nil {
		t.Fatal("switched to an unknown subscription")
	}
	// "Always switch automatically" needs an account policy (an enabled
	// extension); the core switches only when the person asks.
	if _, err = service.SwitchSubscription(ctx, &pb.SwitchSubscriptionRequest{SessionId: id, ProfileId: "b", EnableRotation: true}); status.Code(err) != codes.FailedPrecondition || len(adapter.opened) != 0 {
		t.Fatalf("automatic switching turned on without an account policy: %v", err)
	}
	if err := b.Accounts.Observe("b", agent.AccountObservation{State: "available", Windows: []agent.UsageWindow{{Name: "seven_day", UsedPercent: 100, ResetAt: time.Now().Add(time.Hour).Unix()}}}); err != nil {
		t.Fatal(err)
	}
	if _, err = service.SwitchSubscription(ctx, &pb.SwitchSubscriptionRequest{SessionId: id, ProfileId: "b"}); status.Code(err) != codes.FailedPrecondition || len(adapter.opened) != 0 {
		t.Fatalf("manual switch accepted an exhausted window: %v", err)
	}
	if err := b.Accounts.Observe("b", agent.AccountObservation{State: "available", Windows: []agent.UsageWindow{{Name: "seven_day", UsedPercent: 99}}}); err != nil {
		t.Fatal(err)
	}
	enableAccountPolicy(t, b)
	switched, err := service.SwitchSubscription(ctx, &pb.SwitchSubscriptionRequest{SessionId: id, ProfileId: "b", EnableRotation: true})
	if err != nil {
		t.Fatal(err)
	}
	if switched.Session.Id != id || strings.Join(switched.Session.RotationProfileIds, ",") != "a" {
		t.Fatalf("not the same conversation, or automatic switching not on: %+v", switched.Session)
	}
	second := <-adapter.opened
	if !first.stopped.Load() || second.Open.ID != id {
		t.Fatal("two writers, or a new execution")
	}
	handoff := delivered(t, second)
	if !strings.Contains(handoff.Text, "Fix the flaky login test") || handoff.ID == "ask" || strings.Contains(handoff.Text, "Also check the CI logs") {
		t.Fatalf("handoff: %+v", handoff)
	}
	until(t, "switch not observed", func() bool { s := e.Engine.Snapshot(); return s.Profile == "b" && s.Fault == "" })
	if len(second.Applied) != 0 {
		t.Fatal("the queued message overtook the interrupted request")
	}
	second.Events <- agent.Event{Type: "turn_started"}
	second.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	if op := delivered(t, second); op.ID != "follow-up" || op.Text != "Also check the CI logs" {
		t.Fatalf("%+v", op)
	}
	second.Events <- agent.Event{Type: "turn_started"}
	second.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	time.Sleep(50 * time.Millisecond)
	if len(second.Applied) != 0 || len(first.Applied) != 0 {
		t.Fatal("a message was sent twice")
	}
	// The journal says where the conversation came from, for the client.
	sawSwitch := false
	for cursor := uint64(0); cursor < e.Retained(); cursor++ {
		record, err := e.Engine.Events(ctx, cursor)
		if err != nil {
			t.Fatal(err)
		}
		var ev agent.Event
		if json.Unmarshal(record.Value, &ev) == nil && ev.Type == "profile_changed" {
			sawSwitch = ev.Text == "b" && strings.Contains(string(ev.Data), `"from":"a"`)
		}
	}
	if !sawSwitch {
		t.Fatal("no profile_changed record from a to b")
	}
	saved, err := b.History.Get(ctx, id)
	if err != nil || saved.Options.NoRotation || strings.Join(saved.Options.RotationProfiles, ",") != "a" {
		t.Fatalf("automatic switching not saved with the conversation: %+v %v", saved.Options, err)
	}
	if _, err = service.SwitchSubscription(ctx, &pb.SwitchSubscriptionRequest{SessionId: id, ProfileId: "b"}); err == nil || len(second.Applied) != 0 {
		t.Fatal("a healthy conversation was asked to retry on its own subscription")
	}
	// Now automatic: once a has room again, the next limit on b switches back without anyone asking.
	// a recovers first: the switch is decided when b's limit is handled, from what is known then.
	if err = b.Accounts.Observe("a", agent.AccountObservation{State: "available"}); err != nil {
		t.Fatal(err)
	}
	second.Events <- agent.Event{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted", ResetAt: time.Now().Add(time.Hour).Unix()}}
	select {
	case <-adapter.opened:
	case <-time.After(5 * time.Second):
		t.Fatal("automatic switching was not turned on")
	}
}

// Turning automatic switching on for one conversation, or for every running
// conversation of the provider (which also takes its subscriptions back into
// Settings → Subscriptions), and off again.
func TestSetSessionRotationForOneAndForEveryConversation(t *testing.T) {
	service, _ := collectionService(t)
	b := service.Bench
	adapter := twoSubscriptions(t, b)
	ctx := context.Background()
	settings, err := service.GetRunnerSettings(ctx, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	settings.RotationExcludedProfileIds = []string{"b"}
	if _, err = service.SaveRunnerSettings(ctx, settings); err != nil {
		t.Fatal(err)
	}
	open := func(options *pb.RunnerOptions) string {
		r, err := service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "a", Cwd: t.TempDir(), Options: options})
		if err != nil {
			t.Fatal(err)
		}
		<-adapter.opened
		return r.Session.Id
	}
	// An old conversation: no pool, no opt-out. Another that opted out.
	legacy, mine, optedOut := open(&pb.RunnerOptions{RotationProfileIds: []string{}}), open(&pb.RunnerOptions{NoRotation: true}), open(&pb.RunnerOptions{NoRotation: true})
	if _, err = service.SetSessionRotation(ctx, &pb.SetSessionRotationRequest{SessionId: mine, Enabled: true}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("automatic switching turned on without an account policy: %v", err)
	}
	enableAccountPolicy(t, b)
	pool := func(id string) string {
		e, err := b.Get(id)
		if err != nil {
			t.Fatal(err)
		}
		return strings.Join(e.Options.RotationProfiles, ",")
	}
	r, err := service.SetSessionRotation(ctx, &pb.SetSessionRotationRequest{SessionId: mine, Enabled: true})
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(r.Session.RotationProfileIds, ","); got != "" {
		t.Fatalf("an excluded subscription joined the pool: %q", got)
	}
	if _, err = service.SetSessionRotation(ctx, &pb.SetSessionRotationRequest{SessionId: mine, Enabled: true, AllSessions: true}); err != nil {
		t.Fatal(err)
	}
	settings, _ = service.GetRunnerSettings(ctx, &pb.InfoRequest{})
	if len(settings.RotationExcludedProfileIds) != 0 {
		t.Fatalf("subscriptions still excluded: %v", settings.RotationExcludedProfileIds)
	}
	if pool(mine) != "b" || pool(legacy) != "b" || pool(optedOut) != "" {
		t.Fatalf("pools: mine %q legacy %q opted out %q", pool(mine), pool(legacy), pool(optedOut))
	}
	if _, err = service.SetSessionRotation(ctx, &pb.SetSessionRotationRequest{SessionId: mine}); err != nil {
		t.Fatal(err)
	}
	saved, _ := b.History.Get(ctx, mine)
	if pool(mine) != "" || !saved.Options.NoRotation {
		t.Fatal("automatic switching not turned off")
	}
}

// Only the owner moves a conversation between subscriptions: an agent cannot
// switch its own account or turn switching on, even for its own execution.
func TestAgentCredentialCannotSwitchSubscription(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	e, _, token := f.open(t)
	agentCtx := bearer(ctx, token)
	if _, err := f.client.SwitchSubscription(agentCtx, &pb.SwitchSubscriptionRequest{SessionId: e.ID, ProfileId: "codex-default"}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("agent switched its subscription: %v", err)
	}
	if _, err := f.client.SetSessionRotation(agentCtx, &pb.SetSessionRotationRequest{SessionId: e.ID, Enabled: true, AllSessions: true}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("agent changed automatic switching: %v", err)
	}
	if _, err := f.client.SetSessionRotation(bearer(ctx, ownerToken), &pb.SetSessionRotationRequest{SessionId: e.ID}); err != nil {
		t.Fatalf("owner refused: %v", err)
	}
}
