package transport

import (
	"context"
	"errors"
	"fmt"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"maps"
	"slices"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// Sandboxed panel extensions (EXTENSIONS.md). Every RPC here is the owner's:
// none is on the agent, terminal, device or hook allowlists, and an
// extension's own credential reaches none of them (extension_auth.go).

var catalogEpoch = time.Now().UnixNano()

func catalogToken(snap power.ExtensionsSnapshot) string {
	return fmt.Sprintf("%x-%x", catalogEpoch, snap.Revision)
}

func (s *Service) ListExtensions(_ context.Context, r *pb.ListExtensionsRequest) (*pb.Extensions, error) {
	snap, _ := s.Bench.Extensions().Reload()
	if token := catalogToken(snap); token == r.KnownCatalog {
		return &pb.Extensions{CatalogToken: token}, nil
	}
	return wireExtensions(snap), nil
}

func (s *Service) GetCloneProfile(ctx context.Context, r *pb.GetCloneProfileRequest) (*pb.CloneProfile, error) {
	p, suggestions, err := s.Bench.CloneProfile()
	if err != nil {
		return nil, rpcError(err)
	}
	var questions []string
	if r.LearnFromHistory {
		suggestions, questions, err = s.Bench.LearnCloneStyle(ctx)
		if err != nil {
			return nil, rpcError(err)
		}
	}
	profile := wireCloneProfile(p)
	profile.Suggestions, profile.Questions = suggestions, questions
	return profile, nil
}

func wireCloneProfile(p runner.CloneProfile) *pb.CloneProfile {
	out := &pb.CloneProfile{Style: p.Style, Testing: p.Testing, AskWhen: p.AskWhen, Revision: p.Revision}
	for _, id := range slices.Sorted(maps.Keys(p.Remembered)) {
		saved := p.Remembered[id]
		var values []string
		for _, key := range slices.Sorted(maps.Keys(saved.Answers)) {
			values = append(values, saved.Answers[key]...)
		}
		out.RememberedAnswers = append(out.RememberedAnswers, &pb.RememberedCloneAnswer{Id: id, Workspace: saved.Workspace, Question: saved.Question, Answer: strings.Join(values, ", ")})
	}
	return out
}

func (s *Service) SaveCloneProfile(_ context.Context, r *pb.SaveCloneProfileRequest) (*pb.CloneProfile, error) {
	p := r.GetProfile()
	if p == nil {
		return nil, grpcError(codes.InvalidArgument, errors.New("profile required"))
	}
	saved, err := s.Bench.SaveCloneProfile(runner.CloneProfile{Style: p.Style, Testing: p.Testing, AskWhen: p.AskWhen, Revision: p.Revision}, r.ForgetAnswers, r.ForgetAnswerIds)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireCloneProfile(saved), nil
}

func (s *Service) ApproveExtension(_ context.Context, r *pb.ApproveExtensionRequest) (*pb.Extensions, error) {
	snap, err := s.Bench.Extensions().Approve(r.Id, r.Grants, r.Hosts)
	if err != nil {
		return nil, extensionError(err)
	}
	return wireExtensions(snap), nil
}

func (s *Service) SetExtensionEnabled(_ context.Context, r *pb.SetExtensionEnabledRequest) (*pb.Extensions, error) {
	snap, err := s.Bench.Extensions().SetEnabled(r.Id, r.Enabled)
	if err != nil {
		return nil, extensionError(err)
	}
	return wireExtensions(snap), nil
}

func (s *Service) RemoveExtension(_ context.Context, r *pb.RemoveExtensionRequest) (*pb.Extensions, error) {
	snap, err := s.Bench.Extensions().Remove(r.Id)
	if err != nil {
		return nil, extensionError(err)
	}
	return wireExtensions(snap), nil
}

func (s *Service) InstallExampleExtension(_ context.Context, r *pb.InstallExampleExtensionRequest) (*pb.Extensions, error) {
	snap, err := s.Bench.Extensions().InstallExample(r.Example)
	if err != nil {
		return nil, extensionError(err)
	}
	return wireExtensions(snap), nil
}

func (s *Service) OpenExtensionPanel(_ context.Context, r *pb.OpenExtensionPanelRequest) (*pb.ExtensionPanelSession, error) {
	session, err := s.Bench.Extensions().Open(r.Id, r.Panel, r.Origin)
	if err != nil {
		return nil, extensionError(err)
	}
	return &pb.ExtensionPanelSession{Url: session.URL, ContentOrigin: session.ContentOrigin, Credential: session.Credential, Grants: session.Grants, Hosts: session.Hosts, Digest: session.Digest, Protocol: power.ExtensionProtocol}, nil
}

func extensionError(err error) error {
	var refused *power.ExtensionError
	switch {
	case errors.Is(err, power.ErrNoExtension):
		return grpcError(codes.NotFound, errors.New("there is no such extension in the extensions folder"))
	case errors.As(err, &refused):
		return grpcError(codes.FailedPrecondition, err)
	}
	return rpcError(err)
}

func wireExtensions(snap power.ExtensionsSnapshot) *pb.Extensions {
	out := &pb.Extensions{CatalogToken: catalogToken(snap), Directory: snap.Dir, Revision: snap.Revision, Protocol: power.ExtensionProtocol}
	for _, e := range snap.Extensions {
		m := e.Manifest
		w := &pb.Extension{Id: e.ID, Name: m.Name, Version: m.Version, Description: m.Description, Author: m.Author, Homepage: m.Homepage, Path: e.Path, State: e.State,
			Listing: &pb.ExtensionListing{Category: m.Category, Keywords: m.Keywords, Caption: m.Caption, PreviewJpeg: e.PreviewJPEG}, Grants: m.Grants, Hosts: m.Hosts, ApprovedGrants: e.ApprovedGrants, ApprovedHosts: e.ApprovedHosts, Error: e.Err, Digest: e.Digest}
		if !e.ApprovedAt.IsZero() && e.ApprovedAt.UnixMilli() > 0 {
			w.ApprovedAtUnixMillis = e.ApprovedAt.UnixMilli()
		}
		for _, p := range m.Panels {
			w.Panels = append(w.Panels, &pb.ExtensionPanel{Id: p.ID, Title: p.Title, Icon: p.Icon, Entry: p.Entry, Description: p.Description})
		}
		if p := m.AccountPolicy; p != nil {
			w.AccountPolicy = &pb.ExtensionAccountPolicy{Providers: p.Providers, SwitchOnLimit: p.SwitchOnLimit, Accounts: p.Accounts}
		}
		for _, i := range e.Issues {
			w.Issues = append(w.Issues, &pb.UserActionIssue{Path: i.Path, Line: uint32(i.Line), Message: i.Message, Severity: i.Severity})
		}
		out.Extensions = append(out.Extensions, w)
	}
	for _, x := range power.ExtensionExamples() {
		installed := slices.ContainsFunc(snap.Extensions, func(e power.ExtensionInfo) bool { return e.ID == x.ID })
		out.Examples = append(out.Examples, &pb.ExtensionExample{Id: x.ID, Name: x.Name, Description: x.Description, Installed: installed, Listing: &pb.ExtensionListing{Category: x.Category, Keywords: x.Keywords, Caption: x.Caption, PreviewJpeg: x.PreviewJPEG}})
	}
	return out
}
