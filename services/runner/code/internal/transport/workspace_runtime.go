package transport

import (
	"context"
	"fmt"
	"sort"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	wr "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/workspaceruntime"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// The workspace runtime RPCs are the owner's alone: none is on the agent or
// terminal allowlist (auth.go). An agent starts an action through its
// lazybox_command tool, which checks its session's permission.

func (s *Service) GetWorkspaceRuntime(ctx context.Context, r *pb.WorkspaceRuntimeRequest) (*pb.WorkspaceRuntime, error) {
	return s.workspaceRuntime(ctx, r.CollectionId)
}

func (s *Service) workspaceRuntime(ctx context.Context, id string) (*pb.WorkspaceRuntime, error) {
	v, err := s.Bench.WorkspaceRuntime(ctx, id)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireRuntime(v), nil
}

func (s *Service) ApproveWorkspaceScripts(ctx context.Context, r *pb.ApproveWorkspaceScriptsRequest) (*pb.WorkspaceRuntime, error) {
	if err := s.Bench.ApproveWorkspaceScripts(ctx, r.CollectionId, r.Member, r.Hash, r.RunSetup); err != nil {
		return nil, rpcError(err)
	}
	return s.workspaceRuntime(ctx, r.CollectionId)
}

func (s *Service) StartWorkspaceAction(ctx context.Context, r *pb.WorkspaceActionRequest) (*pb.WorkspaceActionResult, error) {
	stopped, err := s.Bench.StartWorkspaceAction(ctx, r.CollectionId, r.Action, r.Restart)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.actionResult(ctx, r.CollectionId, stopped)
}

func (s *Service) StopWorkspaceAction(ctx context.Context, r *pb.WorkspaceActionRequest) (*pb.WorkspaceActionResult, error) {
	if err := s.Bench.StopWorkspaceAction(ctx, r.CollectionId, r.Action); err != nil {
		return nil, rpcError(err)
	}
	return s.actionResult(ctx, r.CollectionId, nil)
}

func (s *Service) actionResult(ctx context.Context, id string, stopped []runner.StoppedElsewhere) (*pb.WorkspaceActionResult, error) {
	v, err := s.workspaceRuntime(ctx, id)
	if err != nil {
		return nil, err
	}
	out := &pb.WorkspaceActionResult{Runtime: v}
	for _, x := range stopped {
		out.StoppedElsewhere = append(out.StoppedElsewhere, &pb.StoppedElsewhere{CollectionId: x.CollectionID, WorkspaceName: x.WorkspaceName, Action: x.Action})
	}
	return out, nil
}

func (s *Service) RunWorkspaceSetup(ctx context.Context, r *pb.RunWorkspaceSetupRequest) (*pb.WorkspaceRuntime, error) {
	if _, err := s.Bench.RunWorkspaceSetup(ctx, r.CollectionId, r.Member); err != nil {
		return nil, rpcError(err)
	}
	return s.workspaceRuntime(ctx, r.CollectionId)
}

func (s *Service) GetWorkspaceRuntimeSettings(ctx context.Context, _ *pb.InfoRequest) (*pb.WorkspaceRuntimeSettings, error) {
	return s.wireRuntimeSettings(s.Bench.RuntimeSettings()), nil
}

func (s *Service) SaveWorkspaceRuntimeSettings(ctx context.Context, r *pb.WorkspaceRuntimeSettings) (*pb.WorkspaceRuntimeSettings, error) {
	next := wr.Settings{Revision: r.Revision, PortStart: int(r.PortRangeStart), PortEnd: int(r.PortRangeEnd), Repositories: map[string]wr.Override{}}
	for _, o := range r.Repositories {
		if o == nil {
			continue
		}
		override := wr.Override{Config: o.Config, DefaultAction: o.DefaultAction, SkipSetup: o.SkipSetup}
		if _, err := override.Parsed(); err != nil {
			name := o.Name
			if name == "" {
				name = o.RepositoryId
			}
			return nil, grpcError(codes.InvalidArgument, fmt.Errorf("your configuration for %s: %w", name, err))
		}
		next.Repositories[o.RepositoryId] = override
	}
	saved, err := s.Bench.SaveRuntimeSettings(next)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.wireRuntimeSettings(saved), nil
}

// wireRuntimeSettings lists every registered repository, with or without an
// override, so Settings can offer each one.
func (s *Service) wireRuntimeSettings(v wr.Settings) *pb.WorkspaceRuntimeSettings {
	out := &pb.WorkspaceRuntimeSettings{Revision: v.Revision, PortRangeStart: uint32(v.PortStart), PortRangeEnd: uint32(v.PortEnd)}
	names := map[string]string{}
	if s.Bench.Collections != nil {
		for _, repo := range s.Bench.Collections.Repositories() {
			name := repo.Name
			if name == "" {
				name = repo.ID
			}
			names[repo.ID] = name
		}
	}
	for id := range v.Repositories {
		if _, ok := names[id]; !ok {
			names[id] = id
		}
	}
	ids := make([]string, 0, len(names))
	for id := range names {
		ids = append(ids, id)
	}
	sort.Slice(ids, func(i, j int) bool {
		return names[ids[i]] < names[ids[j]] || names[ids[i]] == names[ids[j]] && ids[i] < ids[j]
	})
	for _, id := range ids {
		o := v.Repositories[id]
		w := &pb.RepositoryRuntimeOverride{RepositoryId: id, Name: names[id], Config: o.Config, DefaultAction: o.DefaultAction, SkipSetup: o.SkipSetup}
		if _, err := o.Parsed(); err != nil {
			w.Error = err.Error()
		}
		out.Repositories = append(out.Repositories, w)
	}
	return out
}

func wireRunState(r runner.RunState) *pb.WorkspaceRunState {
	w := &pb.WorkspaceRunState{State: r.State, CommandId: r.CommandID, Signal: r.Signal, Error: r.Error, StartedAtUnixMillis: r.StartedAt, LastLine: r.LastLine}
	if r.ExitCode != nil {
		code := int32(*r.ExitCode)
		w.ExitCode = &code
	}
	return w
}

func wireRuntime(v runner.RuntimeView) *pb.WorkspaceRuntime {
	out := &pb.WorkspaceRuntime{CollectionId: v.CollectionID, PortsError: v.PortsError, DefaultAction: v.DefaultAction}
	if v.PortBase > 0 {
		out.Ports = &pb.WorkspacePorts{Base: uint32(v.PortBase), Count: wr.PortCount}
	}
	for _, m := range v.Members {
		problem := m.Error
		if m.OverrideError != "" {
			problem = strings.TrimSpace(problem + "\n" + m.OverrideError)
		}
		out.Sources = append(out.Sources, &pb.WorkspaceScriptSource{Member: m.Name, RepositoryId: m.RepositoryID, Path: m.Path, Error: problem, Approval: m.Approval, Hash: sourceHash(m), Content: m.Content, Changed: m.Changed, Overridden: m.Override.Config != ""})
	}
	for _, a := range v.Actions {
		out.Actions = append(out.Actions, &pb.WorkspaceAction{Key: a.Key, Member: a.Member, Name: a.Name, Run: a.Run, Cwd: a.Cwd, Default: a.Key == v.DefaultAction, Concurrent: a.Concurrent, Url: a.URL, Port: uint32(a.Port), Source: a.Source, Blocked: a.Blocked, RunState: wireRunState(a.State), RunningElsewhere: a.RunningElsewhere})
	}
	for _, s := range v.Setups {
		out.Setups = append(out.Setups, &pb.WorkspaceSetup{Member: s.Member, Run: s.Run, Source: s.Source, PendingApproval: s.PendingApproval, RunState: wireRunState(s.State), CopiedFiles: s.Files.Copied, MissingFiles: s.Files.Missing})
	}
	return out
}

// sourceHash is what an approval must name; none for a file with nothing to run.
func sourceHash(m runner.WorkspaceMember) string {
	if m.Path == "" || m.Config.Empty() {
		return ""
	}
	return m.Config.Hash()
}
