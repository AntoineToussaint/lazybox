package codecheckpoints

import (
	"context"
	"errors"
	"fmt"
	"strings"
)

// CaptureReview retains the working tree under the review conversation's
// checkpoint refs. The refs are removed with that conversation.
func (m *Manager) CaptureReview(ctx context.Context, conversation string, repos []Repository) (Point, error) {
	unlock, err := m.lockConversation(conversation)
	if err != nil {
		return Point{}, err
	}
	defer unlock()
	release := m.lockRepositories(paths(repos))
	defer release()
	rec, err := m.load(conversation)
	if err != nil {
		return Point{}, err
	}
	point := m.take(ctx, rec, repos, "review-input", "Lazybox review input")
	for _, repo := range point.Repos {
		if !repo.ok() {
			deleteRefs(ctx, conversation, paths(repos), []string{point.Ref})
			return Point{}, fmt.Errorf("could not capture %s for review: %s", repo.Member, repo.Error)
		}
	}
	if err := m.save(rec); err != nil {
		deleteRefs(ctx, conversation, paths(repos), []string{point.Ref})
		return Point{}, err
	}
	return point, nil
}

// ReviewDiff describes the exact changed paths and the Git command for a
// captured review input. A comparison branch is resolved against captured HEAD.
func ReviewDiff(ctx context.Context, repo RepoPoint, base string, mergeBase bool) (string, []string, error) {
	if base == "" || strings.HasPrefix(base, "-") {
		return "", nil, errors.New("no valid comparison base for review")
	}
	base, err := git(ctx, repo.Path, "rev-parse", "--verify", "--end-of-options", base+"^{commit}")
	if err != nil {
		return "", nil, err
	}
	if mergeBase {
		base, err = git(ctx, repo.Path, "merge-base", base, repo.Head)
		if err != nil {
			return "", nil, err
		}
	}
	raw, cut, err := gitCall{dir: repo.Path}.run(ctx, "diff", "--name-only", "-z", base, repo.Commit, "--")
	if cut {
		return "", nil, errors.New("review changed-file list exceeds the checkpoint limit")
	}
	return base, splitNUL(raw), err
}

// ReviewFile reads a file from the captured tree, including untracked files.
func ReviewFile(ctx context.Context, repo RepoPoint, path string) ([]byte, error) {
	raw, cut, err := gitCall{dir: repo.Path}.run(ctx, "show", repo.Commit+":"+path)
	if cut {
		return nil, errors.New("review file exceeds the checkpoint limit")
	}
	return raw, err
}
