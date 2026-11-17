package items

import (
	"context"
	"errors"
	"fmt"
	"time"
)

// Provider brings items in from outside and keeps them fresh. GitHub is built
// in (github.go); Phase 3's extension providers implement the same contract
// (PROJECTS_AND_INTEGRATIONS.md, "The provider contract"). It is kept small:
// a provider produces data, and Lazybox decides what to do with it.
type Provider interface {
	// Name is the provider's id, as ItemRef.provider names it ("github").
	Name() string
	// Resolve maps a URL to an item without the network; false for a URL the
	// provider does not recognize.
	Resolve(url string) (Resolved, bool)
	// Refresh reads items again. Each item carries its last snapshot (a ref
	// alone the first time); etags holds each item's validator from its last
	// read, by Ref.ID. Items that did not change are left out of the result,
	// so an unchanged item costs no rate limit.
	Refresh(ctx context.Context, items []Item, etags map[string]string) (Refreshed, error)
}

// Refreshed is what a Refresh read.
type Refreshed struct {
	// Items changed since their validator, with their new validators by Ref.ID.
	Items []Item
	ETags map[string]string
	// Unchanged items answered "not modified".
	Unchanged []Ref
	// Failed items could not be read, and why; the others still count.
	Failed map[string]error
}

// BranchFinder is a provider that can say which pull requests come from a
// branch (GitHub), so a pull request an agent opened in any way is found.
type BranchFinder interface {
	// PullRequestsFor lists the pull requests whose head is branch in
	// repository. etag is the last answer's validator; notModified says
	// nothing changed since.
	PullRequestsFor(ctx context.Context, repository, branch, etag string) (found []Item, newETag string, notModified bool, err error)
}

// RateLimited is a provider asking Lazybox to wait (HTTP 403 or 429 with a
// rate limit); tracking pauses until Until.
type RateLimited struct{ Until time.Time }

func (e *RateLimited) Error() string {
	if e.Until.IsZero() {
		return "the provider's rate limit was reached; tracking waits before reading again"
	}
	return fmt.Sprintf("the provider's rate limit was reached; tracking resumes at %s", e.Until.Local().Format("15:04"))
}

// AsRateLimited unwraps a rate limit.
func AsRateLimited(err error) (*RateLimited, bool) {
	var limited *RateLimited
	ok := errors.As(err, &limited)
	return limited, ok
}
