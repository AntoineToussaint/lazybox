package codecheckpoints

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
)

// VerificationCheckout materializes a captured tree in an independent Git
// repository under Runner state. It has its own index, refs and object storage.
// Tests may build and write here without changing the author's worktree.
func (m *Manager) VerificationCheckout(ctx context.Context, conversation, source, revision string) (string, error) {
	if !validID.MatchString(conversation) {
		return "", fmt.Errorf("invalid verification conversation")
	}
	commit, err := git(ctx, source, "rev-parse", "--verify", "--end-of-options", revision+"^{commit}")
	if err != nil {
		return "", err
	}
	root := filepath.Join(m.root, "verification", conversation)
	if err := os.MkdirAll(root, 0700); err != nil {
		return "", err
	}
	dir, err := os.MkdirTemp(root, "check-")
	if err != nil {
		return "", err
	}
	complete := false
	defer func() {
		if !complete {
			_ = os.RemoveAll(dir)
		}
	}()
	if _, err = git(ctx, dir, "init", "--quiet"); err != nil {
		return "", err
	}
	if _, err = git(ctx, dir, "fetch", "--no-tags", "--", source, commit); err != nil {
		return "", err
	}
	if _, err = git(ctx, dir, "checkout", "--detach", commit); err != nil {
		return "", err
	}
	complete = true
	return dir, nil
}
