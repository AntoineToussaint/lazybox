package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

func (s *Service) RunReviewVerification(ctx context.Context, r *pb.RunReviewVerificationRequest) (*pb.ReviewVerificationCheck, error) {
	check, err := s.Bench.RunReviewVerification(ctx, r.SessionId, reviews.VerificationCheck{Name: r.Name, Member: r.Member, Expected: r.Expected, Argv: r.Argv, Regression: r.Regression})
	if err != nil {
		return nil, rpcError(err)
	}
	return wireVerification(check), nil
}

func wireVerification(check reviews.VerificationCheck) *pb.ReviewVerificationCheck {
	out := &pb.ReviewVerificationCheck{Name: check.Name, Member: check.Member, Expected: check.Expected, Argv: check.Argv, Regression: check.Regression}
	for _, result := range check.Runs {
		r := &pb.ReviewVerificationExecution{Round: uint32(result.Round), Base: result.Base, Candidate: result.Candidate, BaseCommandId: result.BaseCommand, CandidateCommandId: result.CandidateCommand, State: result.State, Detail: result.Detail}
		if result.BaseExit != nil {
			code := int32(*result.BaseExit)
			r.BaseExit = &code
		}
		if result.CandidateExit != nil {
			code := int32(*result.CandidateExit)
			r.CandidateExit = &code
		}
		out.Runs = append(out.Runs, r)
	}
	return out
}
