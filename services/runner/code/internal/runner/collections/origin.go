package collections

import (
	"errors"
	"regexp"
	"sort"
	"strings"
	"time"
	"unicode"
)

// Origin is the work item a workspace was started from: an issue, a ticket
// (PROJECTS_AND_INTEGRATIONS.md). Provider and Key identify it; the rest is a
// snapshot taken at the start, for the agent's briefing and for showing the
// workspace's origin while the provider cannot be reached.
//
// It is an optional field of manifest version 4: a record without one reads
// as before, and a Runner that predates it reads a record with one (and
// ignores the field). No version change is needed for an absent field.
type Origin struct {
	Provider string `json:"provider"`
	Key      string `json:"key"`
	// issue, pull_request, ticket…
	Kind  string `json:"kind,omitempty"`
	Title string `json:"title,omitempty"`
	URL   string `json:"url,omitempty"`
	// How a commit or pull request references it ("Fixes #812").
	ClosingReference string `json:"closingReference,omitempty"`
	// When the workspace was started from it.
	StartedAt time.Time `json:"startedAt,omitempty"`
}

var providerID = regexp.MustCompile(`^[a-z0-9][a-z0-9_.:-]{0,63}$`)

func (o *Origin) validate() error {
	if o == nil {
		return nil
	}
	o.Key, o.Title = strings.TrimSpace(o.Key), strings.TrimSpace(o.Title)
	if !providerID.MatchString(o.Provider) || o.Key == "" || len(o.Key) > 256 || strings.IndexFunc(o.Key, unicode.IsControl) >= 0 {
		return errors.New("a work item needs a provider and a key of at most 256 characters")
	}
	if len(o.Kind) > 32 || len(o.Title) > 512 || len(o.URL) > 2048 || len(o.ClosingReference) > 256 || strings.IndexFunc(o.Title+o.Kind+o.ClosingReference, unicode.IsControl) >= 0 {
		return errors.New("the work item's title, kind or reference is too long")
	}
	if o.URL != "" && !strings.HasPrefix(o.URL, "https://") {
		return errors.New("a work item's address must be https")
	}
	return nil
}

// Same reports whether o names the item provider/key.
func (o *Origin) Same(provider, key string) bool {
	return o != nil && o.Provider == provider && strings.EqualFold(o.Key, key)
}

// StartedFrom lists the workspaces (archived ones included, deleted ones not)
// started from provider/key, the most recent first.
func (m *Manager) StartedFrom(provider, key string) []Collection {
	out := []Collection{}
	for _, c := range m.List() {
		if c.Origin.Same(provider, key) {
			out = append(out, c)
		}
	}
	sort.SliceStable(out, func(i, j int) bool { return out[i].Origin.StartedAt.After(out[j].Origin.StartedAt) })
	return out
}

// Branches lists every branch a workspace on this Runner holds or held, so a
// new workspace can pick one that is not taken. The workspace of requestKey
// (a retry of the same start) is left out: its branch is its own.
func (m *Manager) Branches(requestKey string) map[string]bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	out := map[string]bool{}
	for _, c := range m.items {
		if requestKey != "" && c.RequestKey == requestKey {
			continue
		}
		for _, member := range c.Members {
			if member.Branch != "" {
				out[member.Branch] = true
			}
		}
	}
	return out
}
