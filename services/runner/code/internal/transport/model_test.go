package transport

import (
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// Switching a conversation's model is the owner's: its own agent cannot pick
// a model for itself, nor can a shell or a paired phone. The owner's switch
// reaches the agent, says itself in the conversation and is what the session
// reports from then on.
func TestModelSwitchIsTheOwnersAlone(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	fake := &agenttest.Adapter{LiveModel: true}
	f.bench.Adapters["codex"] = fake
	e, err := f.bench.OpenEmptyWorkspace("codex-default", accounts.Options{Model: "first-model"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	agentCredential := agentToken(t, fake.Session.Open.Env)
	switchTo := func(id, model string) *pb.ApplyRequest {
		return &pb.ApplyRequest{SessionId: e.ID, OperationId: id, Operation: &pb.ApplyRequest_Model{Model: &pb.ModelChange{Model: model, Effort: "high"}}}
	}
	_, err = f.client.Apply(bearer(ctx, agentCredential), switchTo("agent", "agent-model"))
	expectCode(t, "agent switched its own model", err, codes.PermissionDenied)
	_, err = f.client.RestartWithModel(bearer(ctx, agentCredential), &pb.RestartWithModelRequest{SessionId: e.ID, Model: "agent-model"})
	expectCode(t, "agent restarted itself on another model", err, codes.PermissionDenied)

	owner := bearer(ctx, ownerToken)
	_, err = f.client.Apply(owner, switchTo("bad", "two words"))
	expectCode(t, "a model name with a space", err, codes.InvalidArgument)
	_, err = f.client.RestartWithModel(owner, &pb.RestartWithModelRequest{SessionId: e.ID})
	expectCode(t, "a restart without a model", err, codes.InvalidArgument)
	receipt, err := f.client.Apply(owner, switchTo("owner", "second-model"))
	if err != nil || receipt.Delivery == "failed" {
		t.Fatalf("owner's switch refused: %+v %v", receipt, err)
	}
	select {
	case op := <-fake.Session.Applied:
		if op.Kind != agent.Model || op.Model != "second-model" || op.Effort != "high" {
			t.Fatalf("delivered as %+v", op)
		}
	case <-ctx.Done():
		t.Fatal("switch never reached the agent")
	}
	session, err := f.client.GetSession(owner, &pb.SessionRequest{SessionId: e.ID})
	if err != nil {
		t.Fatal(err)
	}
	if session.Session.Model != "second-model" || session.Session.Effort != "high" || !session.Session.Capabilities.LiveModel {
		t.Fatalf("session does not report the switch: model %q effort %q live %v", session.Session.Model, session.Session.Effort, session.Session.Capabilities.LiveModel)
	}
}

// A paired phone answers and sends; choosing the model is a setting.
func TestDeviceCannotSwitchTheModel(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	e, _, _ := f.open(t)
	_, phone := f.pair(t, devices.GrantRespond, devices.GrantStart)
	_, err := f.client.Apply(bearer(ctx, phone), &pb.ApplyRequest{SessionId: e.ID, OperationId: "phone-model", Operation: &pb.ApplyRequest_Model{Model: &pb.ModelChange{Model: "opus"}}})
	expectCode(t, "device switched the model", err, codes.PermissionDenied)
	_, err = f.client.RestartWithModel(bearer(ctx, phone), &pb.RestartWithModelRequest{SessionId: e.ID, Model: "opus"})
	expectCode(t, "device restarted the agent on another model", err, codes.PermissionDenied)
	if _, err = f.client.Apply(bearer(ctx, phone), &pb.ApplyRequest{SessionId: e.ID, OperationId: "phone-message", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "still allowed"}}}); err != nil {
		t.Fatal("a responder's message was refused", err)
	}
}
