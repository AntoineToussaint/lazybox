package collections

import (
	"context"
	"time"
)

type RevisionEvidence struct {
	Name                 string `json:"name"`
	Path                 string `json:"path"`
	Head                 string `json:"head,omitempty"`
	Status               string `json:"status,omitempty"`
	Error                string `json:"error,omitempty"`
	StatusMayBeTruncated bool   `json:"statusMayBeTruncated,omitempty"`
}

// Evidence inspects current worktrees without refreshing their index or invoking
// configured fsmonitor helpers. BaseCommit alone is not a current revision.
func (m *Manager) Evidence(ctx context.Context, id string) ([]RevisionEvidence, error) {
	collection, err := m.Get(id)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	result := make([]RevisionEvidence, 0, len(collection.Members))
	for _, member := range collection.Members {
		evidence := RevisionEvidence{Name: member.Name, Path: member.Path}
		evidence.Head, err = m.git(ctx, member.Path, "rev-parse", "--verify", "HEAD")
		if err != nil {
			evidence.Error = "HEAD inspection failed"
			result = append(result, evidence)
			continue
		}
		evidence.Status, err = m.git(ctx, member.Path, "--no-optional-locks", "-c", "core.fsmonitor=false", "status", "--porcelain=v1", "--untracked-files=normal")
		if err != nil {
			evidence.Error = "working-tree inspection failed"
		}
		evidence.StatusMayBeTruncated = len(evidence.Status) >= (64<<10)-1
		result = append(result, evidence)
	}
	return result, nil
}
