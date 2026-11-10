package transport

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"os"
)

func (s *Service) wireProfile(p accounts.Profile) *pb.Profile {
	observation := s.Bench.Accounts.Observation(p.ID)
	return &pb.Profile{Id: p.ID, Provider: p.Provider, Label: p.Label, Managed: p.Managed, Isolation: p.Isolation, Availability: wireAccount(&observation)}
}
func wireEnrollment(flow runner.Enrollment) *pb.SubscriptionEnrollment {
	return &pb.SubscriptionEnrollment{Id: flow.ID, State: flow.State, ProfileId: flow.ProfileID, Provider: flow.Provider, Label: flow.Label, SignInUrl: flow.URL, UserCode: flow.UserCode, Error: flow.Error}
}
func (s *Service) AddSubscription(ctx context.Context, p *pb.AddSubscriptionRequest) (*pb.SubscriptionEnrollment, error) {
	flow, err := s.Bench.AddSubscription(p.RequestKey, p.Provider, p.Label, p.SubscriptionToken)
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
		return &pb.SubscriptionEnrollment{Id: p.EnrollmentId, State: "expired", Error: "This sign-in request is no longer tracked. Check saved accounts before trying again."}, nil
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
