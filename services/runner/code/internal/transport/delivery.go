package transport

import (
	"context"
	"errors"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/delivery"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// deliveryOutcome resolves the one lookup a request names.
func (s *Service) deliveryOutcome(sessionID, missionID, taskID string) (delivery.Outcome, error) {
	switch {
	case sessionID != "" && missionID == "" && taskID == "":
		e, err := s.Bench.Get(sessionID)
		if err != nil {
			return delivery.Outcome{}, err
		}
		return s.Bench.DeliveryOutcome(e)
	case sessionID == "" && missionID != "" && taskID != "":
		return s.Bench.TaskDeliveryOutcome(missionID, taskID)
	}
	return delivery.Outcome{}, grpcError(codes.InvalidArgument, errors.New("look an outcome up by session, or by mission and task"))
}

// deliveryError keeps a status the lookup already chose (an invalid lookup).
func deliveryError(err error) error {
	if st, ok := status.FromError(err); ok && st.Code() != codes.Unknown {
		return err
	}
	return rpcError(err)
}

// GetDeliveryOutcome serves the same projection by conversation and by Mission task.
func (s *Service) GetDeliveryOutcome(_ context.Context, r *pb.GetDeliveryOutcomeRequest) (*pb.DeliveryOutcome, error) {
	o, err := s.deliveryOutcome(r.SessionId, r.MissionId, r.TaskId)
	if err != nil {
		return nil, deliveryError(err)
	}
	return wireDeliveryOutcome(o), nil
}

// PublishDeliveryOutcome publishes the outcome as a verdict snapshot in the
// workspace the work happens in.
func (s *Service) PublishDeliveryOutcome(ctx context.Context, r *pb.PublishDeliveryOutcomeRequest) (*pb.Artifact, error) {
	o, err := s.deliveryOutcome(r.SessionId, r.MissionId, r.TaskId)
	if err != nil {
		return nil, deliveryError(err)
	}
	workspace := s.Bench.DeliveryWorkspace(o)
	if workspace == "" {
		return nil, grpcError(codes.FailedPrecondition, errors.New("the outcome's workspace is gone"))
	}
	if r.ArtifactId != "" {
		previous, err := s.Bench.Artifacts.Get(workspace, r.ArtifactId, 0)
		if err != nil {
			return nil, artifactError(err)
		}
		if previous.Kind != "report" || previous.Format != "json" || previous.Versions[len(previous.Versions)-1].Schema != delivery.OutcomeSchema {
			return nil, grpcError(codes.InvalidArgument, errors.New("that artifact is not a delivery outcome"))
		}
	}
	author, source := artifacts.Author{Kind: "person"}, artifacts.Source{Kind: "edit"}
	if session := callerSession(ctx); session != "" {
		conversation, err := s.Bench.SessionConversation(session)
		if err != nil {
			return nil, artifactError(err)
		}
		author = artifacts.Author{Kind: "agent", SessionID: session}
		source = artifacts.Source{Kind: "agent", SessionID: session, ConversationID: conversation}
	}
	doc, err := s.Bench.PublishDeliveryOutcome(o, workspace, r.ArtifactId, author, source)
	if err != nil {
		return nil, artifactError(err)
	}
	return wireDocument(doc), nil
}

// RecordReviewCriterion is a contract-bound reviewer's verdict on one criterion.
func (s *Service) RecordReviewCriterion(_ context.Context, r *pb.RecordReviewCriterionRequest) (*pb.RecordReviewCriterionResponse, error) {
	o, check, err := s.Bench.RecordReviewCriterion(r.SessionId, r.Criterion, r.Result)
	if err != nil {
		if errors.Is(err, runner.ErrNotReviewer) {
			return nil, grpcError(codes.PermissionDenied, err)
		}
		return nil, rpcError(err)
	}
	return &pb.RecordReviewCriterionResponse{Note: o.Summary(check)}, nil
}

func wireDeliveryOutcome(o delivery.Outcome) *pb.DeliveryOutcome {
	w := &pb.DeliveryOutcome{Attempt: o.Attempt, ConversationId: o.Subject.Conversation, MissionId: o.Subject.Mission, TaskId: o.Subject.Task, Current: o.Current, Decision: o.Decision,
		Contract: &pb.DeliveryContract{ArtifactId: o.Contract.Artifact, Version: o.Contract.Version, Digest: o.Contract.Digest, Schema: o.Contract.Schema}}
	for _, c := range o.Contract.Criteria {
		w.Contract.Criteria = append(w.Contract.Criteria, &pb.DeliveryCriterion{Id: c.ID, Expected: c.Expected, Required: c.Required})
	}
	for _, c := range o.Candidates {
		wc := &pb.DeliveryCandidateOutcome{Candidate: c.Candidate, Current: c.Current, Verdict: c.Verdict}
		for _, criterion := range c.Criteria {
			wcr := &pb.DeliveryCriterionOutcome{Id: criterion.ID, Expected: criterion.Expected, Required: criterion.Required, State: criterion.State}
			for _, e := range criterion.Evidence {
				we := &pb.DeliveryEvidence{Id: e.ID, Source: e.Source, Reference: e.Reference, CommandId: e.Command, Assertion: e.Assertion, Prerequisite: e.Prerequisite, State: e.State, Output: e.Output}
				if e.ExitCode != nil {
					code := int32(*e.ExitCode)
					we.ExitCode = &code
				}
				wcr.Evidence = append(wcr.Evidence, we)
			}
			wc.Criteria = append(wc.Criteria, wcr)
		}
		w.Candidates = append(w.Candidates, wc)
	}
	return w
}
