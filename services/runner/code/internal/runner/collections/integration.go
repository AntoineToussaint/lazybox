package collections

import (
	"context"
	"fmt"
)

// MergeReviewed merges an immutable worker commit only when it descends from
// the approved base. The caller supplies a member of a fresh candidate.
func (m *Manager) MergeReviewed(ctx context.Context, member Member, base, head string) error {
	if _, err := m.git(ctx, member.Path, "merge-base", "--is-ancestor", base, head); err != nil {
		return fmt.Errorf("member %s no longer descends from its approved base: %w", member.Name, err)
	}
	_, err := m.git(ctx, member.Path, "-c", "user.name=Lazybox", "-c", "user.email=mission@localhost", "-c", "commit.gpgSign=false", "-c", "core.hooksPath=/dev/null", "merge", "--no-edit", "--no-stat", head)
	return err
}

// RetainCandidate keeps accepted objects reachable after candidate worktrees
// are archived. Each candidate gets its own refs, including duplicate repo members.
func (m *Manager) RetainCandidate(ctx context.Context, id string, heads map[string]string) error {
	ws, err := m.Get(id)
	if err != nil {
		return err
	}
	if len(ws.Members) != len(heads) {
		return fmt.Errorf("candidate membership changed")
	}
	for _, member := range ws.Members {
		head := heads[member.Name]
		if head == "" {
			return fmt.Errorf("missing tested revision for %s", member.Name)
		}
		if _, err = m.git(ctx, member.Path, "update-ref", "refs/lazybox/missions/"+id+"/"+member.Name, head); err != nil {
			return err
		}
	}
	return nil
}
