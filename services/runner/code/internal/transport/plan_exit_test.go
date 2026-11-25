package transport

import (
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Approving the plan with a plain Allow takes the conversation out of plan
// mode, so the next approval can offer Full access again.
func TestPlainAllowOfThePlanLeavesPlanMode(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	fake := &agenttest.Adapter{LivePermission: true}
	f.bench.Adapters["claude"] = fake
	e, err := f.bench.OpenEmptyWorkspace("claude-default", accounts.Options{Workflow: "plan"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	fake.Session.Events <- agent.Event{Type: "approval", RequestID: "plan", Text: "ExitPlanMode"}
	request := ""
	for end := time.Now().Add(5 * time.Second); request == "" && time.Now().Before(end); time.Sleep(10 * time.Millisecond) {
		for id := range e.Engine.Snapshot().Pending {
			request = id
		}
	}
	if request == "" {
		t.Fatal("plan approval never pending")
	}
	approve := &pb.ApplyRequest{SessionId: e.ID, OperationId: "allow-plan", Operation: &pb.ApplyRequest_Approval{Approval: &pb.Approval{RequestId: request, Decision: pb.Decision_DECISION_ACCEPT}}}
	if _, err = f.client.Apply(bearer(ctx, ownerToken), approve); err != nil {
		t.Fatal(err)
	}
	session, err := f.client.GetSession(bearer(ctx, ownerToken), &pb.SessionRequest{SessionId: e.ID})
	if err != nil {
		t.Fatal(err)
	}
	if session.GetSession().GetWorkflow() != "execute" {
		t.Fatalf("the conversation is still %q after its plan was approved", session.GetSession().GetWorkflow())
	}
}
