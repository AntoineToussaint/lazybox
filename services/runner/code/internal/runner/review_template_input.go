package runner

import (
	"context"
	"errors"
	"fmt"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/codecheckpoints"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
)

func (b *Bench) captureReviewInput(ctx context.Context, conversation string, workspace collections.Collection, compare string, heads map[string]string) (reviewScope, error) {
	out := reviewScope{label: "Changes"}
	var repos []codecheckpoints.Repository
	bases := map[string]string{}
	for _, member := range workspace.Members {
		if member.Ready {
			repos = append(repos, codecheckpoints.Repository{Member: member.Name, Path: member.Path})
			bases[member.Name] = member.BaseCommit
		}
	}
	if len(repos) == 0 {
		return out, errors.New("this workspace has no repository ready to review")
	}
	point, err := b.Checkpoints.CaptureReview(ctx, conversation, repos)
	if err != nil {
		return out, err
	}
	var commands []string
	for _, repo := range point.Repos {
		if heads != nil {
			_, changed, err := codecheckpoints.ReviewDiff(ctx, repo, repo.Head, false)
			if err != nil || len(changed) > 0 || repo.Head != heads[repo.Member] || repo.SkippedCount > 0 {
				_ = b.Checkpoints.Delete(ctx, conversation)
				return out, fmt.Errorf("candidate %s changed during review capture", repo.Member)
			}
		}
		base := firstOf(compare, bases[repo.Member])
		if compare == ":uncommitted" {
			base = repo.Head
		}
		base, files, err := codecheckpoints.ReviewDiff(ctx, repo, base, compare != "" && compare != ":uncommitted")
		if err != nil {
			_ = b.Checkpoints.Delete(ctx, conversation)
			return out, fmt.Errorf("compare %s for review: %w", repo.Member, err)
		}
		out.input = append(out.input, reviews.ReviewInput{Member: repo.Member, Base: base, Head: repo.Head, Commit: repo.Commit, Skipped: repo.SkippedCount})
		commands = append(commands, fmt.Sprintf("    git -C %s diff %s %s --", shellQuote(repo.Path), base, repo.Commit))
		for _, file := range files {
			out.files = append(out.files, repo.Member+"/"+file)
		}
		if repo.SkippedCount > 0 {
			commands = append(commands, fmt.Sprintf("\n%s: %d paths exceeded snapshot limits. Include that gap in your summary.", repo.Member, repo.SkippedCount))
		}
	}
	out.diff = "Review the captured workspace changes with the following commands. Read file context from the captured commit using git show <captured commit>:<path>. The live workspace can change during this review.\n\n" + strings.Join(commands, "\n")
	return out, nil
}
