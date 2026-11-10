package transport

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/preferences"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/telemetry"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	petname "github.com/dustinkirkland/golang-petname"
	"google.golang.org/grpc/codes"
	"sort"
	"strings"
)

func (s *Service) GetRunnerSettings(context.Context, *pb.InfoRequest) (*pb.RunnerSettings, error) {
	d, rates := s.Bench.Preferences.SnapshotRates()
	w := &pb.RunnerSettings{Revision: d.Revision, Naming: d.Naming, Permission: d.Permission, Context: d.Context, ClaudeContext: d.ClaudeContext, CodexContext: d.CodexContext, SuggestedName: petname.Generate(3, "-")}
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
	err := s.Bench.Preferences.Save(preferences.Data{Revision: r.Revision, Naming: r.Naming, Permission: r.Permission, Context: r.Context, ClaudeContext: r.ClaudeContext, CodexContext: r.CodexContext})
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
