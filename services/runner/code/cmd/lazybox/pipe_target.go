package main

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

// `lazybox run` and `lazybox log` from a person's own terminal: the Runner is
// the one the power verbs find (an installed app's service, Working, Dev:
// endpoint.go), and the workspace is named, or is the one the current
// directory is in. Inside an agent, a workspace terminal or a hook nothing
// changes: those carry their own scoped endpoint and identity.

// ownerEndpoint is the endpoint file to publish through: the one given, or,
// in a person's own terminal, the registered Runner's.
func ownerEndpoint(getenv func(string) string, explicit, runner string) string {
	if explicit != "" || scopedEnvironment(getenv) {
		return explicit
	}
	t, err := resolveTarget(getenv, "", runner)
	if err != nil {
		return ""
	}
	return t.EndpointPath
}

// resolveWorkspace turns --collection (an id or a workspace's name) into an
// id; with neither, the workspace whose folder or worktree holds cwd, if any.
func resolveWorkspace(ctx context.Context, c genconnect.RunnerServiceClient, given, cwd string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	list, err := c.ListCollections(ctx, connect.NewRequest(&pb.ListCollectionsRequest{}))
	if err != nil {
		return given, err
	}
	if given != "" {
		var named []*pb.Collection
		for _, col := range list.Msg.Collections {
			if col.Id == given {
				return given, nil
			}
			if strings.EqualFold(col.Name, given) {
				named = append(named, col)
			}
		}
		switch len(named) {
		case 0:
			return given, nil // the Runner says what is wrong with it
		case 1:
			return named[0].Id, nil
		}
		return "", fmt.Errorf("%d workspaces are named %q; pass the id instead (lazybox ls)", len(named), given)
	}
	if cwd == "" {
		return "", nil
	}
	if resolved, err := filepath.EvalSymlinks(cwd); err == nil {
		cwd = resolved
	}
	best, length := "", 0
	for _, col := range list.Msg.Collections {
		paths := []string{col.Root}
		for _, m := range col.Members {
			paths = append(paths, m.Path)
		}
		for _, p := range paths {
			if p == "" {
				continue
			}
			if resolved, err := filepath.EvalSymlinks(p); err == nil {
				p = resolved
			}
			if (cwd == p || strings.HasPrefix(cwd, p+string(os.PathSeparator))) && len(p) > length {
				best, length = col.Id, len(p)
			}
		}
	}
	return best, nil
}
