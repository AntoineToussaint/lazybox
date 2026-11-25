package transport

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"os"

	"google.golang.org/grpc/codes"
)

func (s *Service) wireProfile(p accounts.Profile) *pb.Profile {
	observation := s.Bench.Accounts.Observation(p.ID)
	return &pb.Profile{Id: p.ID, Provider: p.Provider, Label: p.Label, Managed: p.Managed, Isolation: p.Isolation, Availability: wireAccount(&observation), TokenImported: p.TokenFile != ""}
}

// RemoveSubscription forgets a subscription saved in Lazybox.
func (s *Service) RemoveSubscription(ctx context.Context, r *pb.RemoveSubscriptionRequest) (*pb.InfoResponse, error) {
	if err := s.Bench.RemoveSubscription(r.ProfileId); err != nil {
		return nil, rpcError(err)
	}
	return s.Info(ctx, &pb.InfoRequest{})
}
func wireEnrollment(flow runner.Enrollment) *pb.SubscriptionEnrollment {
	return &pb.SubscriptionEnrollment{Id: flow.ID, State: flow.State, ProfileId: flow.ProfileID, Provider: flow.Provider, Label: flow.Label, SignInUrl: flow.URL, UserCode: flow.UserCode, Error: flow.Error, SignInMethod: flow.Method, ErrorCode: flow.ErrorCode}
}
func (s *Service) AddSubscription(ctx context.Context, p *pb.AddSubscriptionRequest) (*pb.SubscriptionEnrollment, error) {
	flow, err := s.Bench.AddSubscription(p.RequestKey, p.Provider, p.Label, p.SubscriptionToken, p.SignInMethod)
	p.SubscriptionToken = ""
	if err != nil {
		return nil, rpcError(err)
	}
	return wireEnrollment(flow), nil
}
func (s *Service) GetSubscriptionEnrollment(ctx context.Context, p *pb.SubscriptionEnrollmentRequest) (*pb.SubscriptionEnrollment, error) {
	var flow runner.Enrollment
	var err error
	if p.EnrollmentId != "" {
		flow, err = s.Bench.Enrollment(p.EnrollmentId)
	} else {
		flow, err = s.Bench.EnrollmentForRequest(p.Provider, p.RequestKey)
	}
	if errors.Is(err, os.ErrNotExist) {
		return &pb.SubscriptionEnrollment{Id: p.EnrollmentId, State: "expired", Error: runner.ExpiredSignIn}, nil
	}
	if err != nil {
		return nil, rpcError(err)
	}
	return wireEnrollment(flow), nil
}
func (s *Service) CancelSubscriptionEnrollment(ctx context.Context, p *pb.SubscriptionEnrollmentRequest) (*pb.SubscriptionEnrollment, error) {
	flow, err := s.Bench.CancelEnrollment(p.EnrollmentId)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireEnrollment(flow), nil
}
func (s *Service) RefreshSubscription(ctx context.Context, p *pb.RefreshSubscriptionRequest) (*pb.Profile, error) {
	profile, err := s.Bench.RefreshSubscription(ctx, p.ProfileId)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.wireProfile(profile), nil
}

func (s *Service) ClearSubscriptionLimit(_ context.Context, p *pb.ClearSubscriptionLimitRequest) (*pb.Profile, error) {
	profile, err := s.Bench.ClearSubscriptionLimit(p.ProfileId)
	if errors.Is(err, os.ErrNotExist) {
		return nil, grpcError(codes.NotFound, errors.New("saved subscription not found"))
	}
	if errors.Is(err, accounts.ErrNotLimited) {
		return nil, grpcError(codes.FailedPrecondition, err)
	}
	if err != nil {
		return nil, rpcError(err)
	}
	return s.wireProfile(profile), nil
}

func (s *Service) RenameSubscription(_ context.Context, r *pb.RenameSubscriptionRequest) (*pb.Profile, error) {
	profile, err := s.Bench.RenameSubscription(r.ProfileId, r.Label)
	if errors.Is(err, os.ErrNotExist) {
		return nil, grpcError(codes.NotFound, errors.New("saved subscription not found"))
	}
	if errors.Is(err, accounts.ErrSubscriptionLabel) {
		return nil, grpcError(codes.InvalidArgument, err)
	}
	if err != nil {
		return nil, rpcError(err)
	}
	return s.wireProfile(profile), nil
}
