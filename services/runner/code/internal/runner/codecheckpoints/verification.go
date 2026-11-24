package codecheckpoints

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// VerificationCheckout materializes a captured tree in an independent Git
// repository under Runner state. It has its own index, refs and object storage.
// Tests may build and write here without changing the author's worktree.
// Its objects are copied, not borrowed (--shared, --reference): the
// verification sandbox can read only the checkout, so an alternate object
// store outside it would be unreadable to Git run by the test. The Runner
// removes each checkout once its command finishes (RemoveVerificationCheckout).
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

// RemoveVerificationCheckout deletes a checkout VerificationCheckout made,
// once the command that ran in it has finished: each is a full repository
// with whatever the test built, so keeping them fills the disk. dir may be
// the path VerificationCheckout returned or its canonical form (the command's
// working directory). Any other path is refused; a missing one is not an error.
func (m *Manager) RemoveVerificationCheckout(dir string) error {
	root := filepath.Join(m.root, "verification")
	if canonical, err := filepath.EvalSymlinks(root); err == nil {
		if rel, ok := verificationCheckout(canonical, dir); ok {
			return os.RemoveAll(filepath.Join(root, rel))
		}
	}
	if rel, ok := verificationCheckout(root, dir); ok {
		return os.RemoveAll(filepath.Join(root, rel))
	}
	return errors.New("not a verification checkout")
}

// verificationCheckout reports whether dir is root/<conversation>/check-*,
// and that relative path.
func verificationCheckout(root, dir string) (string, bool) {
	rel, err := filepath.Rel(root, filepath.Clean(dir))
	if err != nil {
		return "", false
	}
	parts := strings.Split(rel, string(filepath.Separator))
	if len(parts) != 2 || !validID.MatchString(parts[0]) || !strings.HasPrefix(parts[1], "check-") {
		return "", false
	}
	return rel, true
}
