package runner

import (
	"context"
	"errors"
	"fmt"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/workitems"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Starting a workspace from a work item (PROJECTS_AND_INTEGRATIONS.md): the
// providers this Runner has, a workspace prepared with the item as its origin,
// and the line the agent's briefing gains for it.

// ItemProviders are the work item providers of this Runner, GitHub first.
// GitHub is offered while it is connected.
func (b *Bench) ItemProviders() []workitems.Provider {
	g := workitems.GitHub{Reader: b.GitHubIssues}
	if b.GitHub != nil && b.GitHub.Status().State == "connected" {
		g.WithCredential = b.GitHub.WithCredential
	}
	return []workitems.Provider{g}
}

// ItemProvider returns the provider with that id; "" is the first one.
func (b *Bench) ItemProvider(id string) (workitems.Provider, error) {
	for _, p := range b.ItemProviders() {
		if id == "" || p.ID() == id {
			return p, nil
		}
	}
	return nil, fmt.Errorf("no work item provider %q on this Runner", id)
}

// ResolveItem turns a pasted link or key into the item it names.
func (b *Bench) ResolveItem(reference string) (*pb.ItemRef, error) {
	reference = strings.TrimSpace(reference)
	if reference == "" || len(reference) > 2048 {
		return nil, errors.New("paste an issue's link, or write owner/name#number")
	}
	for _, p := range b.ItemProviders() {
		if key, ok := p.Resolve(reference); ok {
			return &pb.ItemRef{Provider: p.ID(), Key: key}, nil
		}
	}
	return nil, errors.New("not a link Lazybox can start from: paste a GitHub issue's link, or write owner/name#number")
}

// PrepareWorkspaceFromItem is PrepareWorkspace for a new workspace started from
// an item, which becomes its origin. Without repositories it uses the item's
// own; without a branch, the item's (workitems.Branch); without a name, the
// item's title. Reading the item only fills in what was left out and the
// origin's snapshot: when the provider cannot be reached the start goes on
// with the reference alone.
func (b *Bench) PrepareWorkspaceFromItem(ctx context.Context, key, name, branch string, identities []string, starts map[string]string, ref *pb.ItemRef, progress func(string, string) error) (collections.Collection, error) {
	if b.Collections == nil {
		return collections.Collection{}, errors.New("workspace storage unavailable")
	}
	if ref == nil || strings.TrimSpace(ref.Key) == "" {
		return collections.Collection{}, errors.New("which item does the workspace start from?")
	}
	provider, err := b.ItemProvider(ref.Provider)
	if err != nil {
		return collections.Collection{}, err
	}
	origin := &collections.Origin{Provider: provider.ID(), Key: ref.Key}
	item := &pb.Item{Ref: &pb.ItemRef{Provider: provider.ID(), Key: ref.Key}}
	if detail, err := provider.Get(ctx, ref.Key, 0); err == nil && detail.Item != nil {
		item = detail.Item
		origin.Kind, origin.Title, origin.URL, origin.ClosingReference = item.Kind, item.Title, item.Url, item.ClosingReference
	} else if err = progress("", fmt.Sprintf("Could not read %s from %s; starting from its reference alone", ref.Key, provider.Name())); err != nil {
		return collections.Collection{}, err
	}
	if len(identities) == 0 && item.Repository != "" {
		identities = []string{item.Repository}
	}
	if strings.TrimSpace(branch) == "" {
		branch = workitems.Branch(item, b.Collections.Branches(key))
	}
	if strings.TrimSpace(name) == "" {
		name = item.Title
		if name == "" {
			name = ref.Key
		}
		if len(name) > 128 {
			name = strings.TrimSpace(name[:125]) + "…"
		}
	}
	return b.prepareWorkspace(ctx, key, "", name, branch, identities, starts, "", origin, progress)
}

// originBriefing is the workspace briefing's line about where the work came
// from, or "" for a workspace that was not started from an item.
func originBriefing(c collections.Collection) string {
	if c.Origin == nil {
		return ""
	}
	return workitems.Briefing(c.Origin.Provider, c.Origin.Key, c.Origin.Kind, c.Origin.ClosingReference)
}
