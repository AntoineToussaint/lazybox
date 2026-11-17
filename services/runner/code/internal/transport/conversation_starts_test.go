package transport

import (
	"strings"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// An agent asks to start a conversation for its own execution and waits on
// its own request; it can never answer one, and the request waits in its
// conversation as a pending entry, like an approval.
func TestAgentAsksToStartAConversationButNeverStartsOne(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	mine, _, myToken := f.open(t)
	other, _, otherToken := f.open(t)
	agent, owner, stranger := bearer(ctx, myToken), bearer(ctx, ownerToken), bearer(ctx, otherToken)

	request, err := f.client.RequestConversationStart(agent, &pb.RequestConversationStartRequest{SessionId: mine.ID, Title: "Review the modules", Brief: "Each module: findings."})
	if err != nil || request.State != "pending" || request.SessionId != mine.ID {
		t.Fatalf("request: %+v %v", request, err)
	}
	_, err = f.client.RequestConversationStart(agent, &pb.RequestConversationStartRequest{SessionId: other.ID, Title: "As another", Brief: "b"})
	expectCode(t, "asking as another execution", err, codes.PermissionDenied)
	_, err = f.client.WaitConversationStart(stranger, &pb.WaitConversationStartRequest{RequestId: request.Id})
	expectCode(t, "waiting on another's request", err, codes.PermissionDenied)
	if _, err = f.client.WaitConversationStart(agent, &pb.WaitConversationStartRequest{RequestId: request.Id}); err != nil {
		t.Fatalf("waiting on its own: %v", err)
	}
	_, err = f.client.AnswerConversationStart(agent, &pb.AnswerConversationStartRequest{RequestId: request.Id, Decision: "start"})
	expectCode(t, "an agent answering", err, codes.PermissionDenied)
	if msg := status.Convert(err).Message(); !strings.Contains(msg, "AnswerConversationStart is the person's") || strings.Contains(msg, "\n") {
		t.Fatalf("refusal: %q", msg)
	}
	_, err = f.client.RequestConversationStart(agent, &pb.RequestConversationStartRequest{SessionId: mine.ID, Title: "Nothing to do"})
	expectCode(t, "no brief", err, codes.InvalidArgument)

	// The person sees it where they see approvals.
	session, err := f.client.GetSession(owner, &pb.SessionRequest{SessionId: mine.ID})
	if err != nil {
		t.Fatal(err)
	}
	if len(session.Session.Pending) != 1 || session.Session.Pending[0].Type != "conversation_request" || session.Session.Pending[0].RequestId != request.Id || !strings.Contains(string(session.Session.Pending[0].ProviderDataJson), "Review the modules") {
		t.Fatalf("pending: %+v", session.Session.Pending)
	}
	declined, err := f.client.AnswerConversationStart(owner, &pb.AnswerConversationStartRequest{RequestId: request.Id, Decision: "decline", Reason: "later"})
	if err != nil || declined.State != "declined" || declined.DeclineReason != "later" {
		t.Fatalf("decline: %+v %v", declined, err)
	}
	if session, _ = f.client.GetSession(owner, &pb.SessionRequest{SessionId: mine.ID}); len(session.Session.Pending) != 0 {
		t.Fatalf("still pending: %+v", session.Session.Pending)
	}
}

// A phone that may respond starts a request as asked, or declines it;
// choosing another agent, model or workspace is the computer's.
func TestAPhoneAnswersAConversationRequestAsAsked(t *testing.T) {
	rule := deviceRPCs[pb.RunnerService_AnswerConversationStart_FullMethodName]
	for req, want := range map[*pb.AnswerConversationStartRequest]bool{
		{RequestId: "r", Decision: "start"}:                      true,
		{RequestId: "r", Decision: "start", Title: "Shorter"}:    true,
		{RequestId: "r", Decision: "decline", Reason: "not now"}: true,
		{RequestId: "r", Decision: "start", ProfileId: "codex"}:  false,
		{RequestId: "r", Decision: "start", Model: "gpt-5.4"}:    false,
		{RequestId: "r", Decision: "start", Workspace: "new"}:    false,
		{RequestId: "r", Decision: "always"}:                     false,
	} {
		if got := rule.allow(req); got != want {
			t.Errorf("%+v: %v", req, got)
		}
	}
}
