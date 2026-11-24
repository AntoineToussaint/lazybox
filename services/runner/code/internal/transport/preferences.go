package transport

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/preferences"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	petname "github.com/dustinkirkland/golang-petname"
	"google.golang.org/grpc/codes"
	"slices"
	"sort"
	"strings"
)

// GetClaudeAuth reports how Claude agents authenticate; never the key.
func (s *Service) GetClaudeAuth(context.Context, *pb.InfoRequest) (*pb.ClaudeAuth, error) {
	auth, err := s.Bench.ClaudeAuth()
	if err != nil {
		return nil, rpcError(err)
	}
	return wireClaudeAuth(auth), nil
}

// SetClaudeApiKey saves or forgets the Anthropic API key.
func (s *Service) SetClaudeApiKey(_ context.Context, r *pb.SetClaudeApiKeyRequest) (*pb.ClaudeAuth, error) {
	auth, err := s.Bench.SetClaudeAPIKey(strings.TrimSpace(r.ApiKey), r.Clear)
	r.ApiKey = ""
	if err != nil {
		return nil, grpcError(codes.InvalidArgument, err)
	}
	return wireClaudeAuth(auth), nil
}

func wireClaudeAuth(a runner.ClaudeAuth) *pb.ClaudeAuth {
	return &pb.ClaudeAuth{ApiKeySet: a.APIKeySet, ApiKeyHint: a.APIKeyHint, CloudProvider: a.CloudProvider}
}

func (s *Service) GetRunnerSettings(context.Context, *pb.InfoRequest) (*pb.RunnerSettings, error) {
	d, rates := s.Bench.Preferences.SnapshotRates()
	w := &pb.RunnerSettings{Revision: d.Revision, Naming: d.Naming, Permission: d.Permission, Context: d.Context, ClaudeContext: d.ClaudeContext, CodexContext: d.CodexContext, ProcessBackend: d.ProcessBackend, RotationExcludedProfileIds: d.RotationExcluded, ClaudeAiConnectors: d.ClaudeAIConnectors, PlainTitles: d.PlainTitles, SuggestedName: petname.Generate(3, "-"),
		ClaudeSubscriptionSignIn: d.ClaudeSubscriptionSignIn, ClaudeSignInCarriedOver: d.ClaudeSignInCarriedOver, Notices: d.Notices, AutomaticSwitchingProviders: s.Bench.AutomaticSwitchingProviders(),
		Review: &pb.ReviewSettings{Reviewer: d.Review.Reviewer, Template: d.Review.Template, AutoStart: d.Review.AutoStart, DefaultTemplate: reviews.DefaultTemplate, Loop: firstNonEmptyString(d.Review.Loop, reviews.LoopAsk), MaxRounds: uint32(reviews.ClampRounds(d.Review.MaxRounds)), ReviewerMode: d.Review.ReviewerModeOf(), ReviewerModel: d.Review.ReviewerModel}}
	if ext := s.Bench.Extensions(); ext != nil {
		if notice := ext.DiskPressureNotice(); notice != "" {
			w.Notices = append(w.Notices, notice)
		}
	}
	for key, r := range rates {
		provider, model, _ := strings.Cut(key, "/")
		_, custom := d.Prices[key]
		w.Prices = append(w.Prices, &pb.ModelPrice{Provider: provider, Model: model, Input: r.Input, Output: r.Output, CacheRead: r.CacheRead, CacheWrite5M: r.CacheWrite5m, CacheWrite1H: r.CacheWrite1h, MaxInput: r.MaxInput, Version: r.Version, Custom: custom})
	}
	sort.Slice(w.Prices, func(i, j int) bool {
		return w.Prices[i].Provider+"/"+w.Prices[i].Model < w.Prices[j].Provider+"/"+w.Prices[j].Model
	})
	return w, nil
}
func settingsError(err error) error {
	if errors.Is(err, preferences.ErrConflict) {
		return grpcError(codes.Aborted, err)
	}
	return rpcError(err)
}
func (s *Service) SaveRunnerSettings(ctx context.Context, r *pb.RunnerSettings) (*pb.RunnerSettings, error) {
	// A client that does not know the review settings keeps them.
	review := s.Bench.Preferences.Snapshot().Review
	if r.Review != nil {
		review = preferences.Review{Reviewer: r.Review.Reviewer, Template: strings.TrimSpace(r.Review.Template), AutoStart: r.Review.AutoStart, Loop: r.Review.Loop, MaxRounds: int(r.Review.MaxRounds), ReviewerMode: r.Review.ReviewerMode, ReviewerModel: strings.TrimSpace(r.Review.ReviewerModel)}
		// The other agent or the same one names no reviewer: a stale one is not kept.
		if review.ReviewerMode == preferences.ReviewerOther || review.ReviewerMode == preferences.ReviewerSame {
			review.Reviewer, review.ReviewerModel = "", ""
		}
	}
	// A save dismisses notices it leaves out and never adds one; the
	// migration that raised them is the Runner's, not the client's.
	current := s.Bench.Preferences.Snapshot()
	notices := []string{}
	for _, id := range current.Notices {
		if slices.Contains(r.Notices, id) {
			notices = append(notices, id)
		}
	}
	err := s.Bench.Preferences.Save(preferences.Data{Revision: r.Revision, Naming: r.Naming, Permission: r.Permission, Context: r.Context, ClaudeContext: r.ClaudeContext, CodexContext: r.CodexContext, ProcessBackend: r.ProcessBackend, RotationExcluded: r.RotationExcludedProfileIds, ClaudeAIConnectors: r.ClaudeAiConnectors, PlainTitles: r.PlainTitles,
		ClaudeSubscriptionSignIn: r.ClaudeSubscriptionSignIn, ClaudeSignInCarriedOver: current.ClaudeSignInCarriedOver && r.ClaudeSubscriptionSignIn, Notices: notices, Migration: current.Migration, Review: review})
	if err != nil {
		return nil, settingsError(err)
	}
	return s.GetRunnerSettings(ctx, &pb.InfoRequest{})
}
func (s *Service) SetModelPrice(ctx context.Context, r *pb.SetModelPriceRequest) (*pb.RunnerSettings, error) {
	p := r.Price
	if p == nil {
		return nil, rpcError(errors.New("model price required"))
	}
	rate := telemetry.Rate{Input: p.Input, Output: p.Output, CacheRead: p.CacheRead, CacheWrite5m: p.CacheWrite5M, CacheWrite1h: p.CacheWrite1H, MaxInput: p.MaxInput}
	if err := s.Bench.Preferences.SetRate(r.Revision, p.Provider+"/"+p.Model, rate, r.Remove); err != nil {
		return nil, settingsError(err)
	}
	return s.GetRunnerSettings(ctx, &pb.InfoRequest{})
}

func firstNonEmptyString(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}
