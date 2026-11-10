package runner

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"time"
	"unicode"

	"github.com/AntoineToussaint/lazybox/v2/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/repositorycatalog"
	"github.com/AntoineToussaint/lazybox/v2/internal/sources/github"
)

type githubCatalogSource struct{ b *Bench }

func (s githubCatalogSource) Describe() repositorycatalog.SourceState {
	v := s.b.GitHub.Status()
	detail := "Repositories visible to this personal token."
	if v.AuthMethod != "personal_token" {
		detail = "GitHub App access is limited to its installed repositories."
	}
	state := v.State
	if !s.b.Config.RepositoryCatalog.GitHub() {
		state = "disabled"
		detail = "GitHub discovery is disabled in Runner configuration."
	}
	return repositorycatalog.SourceState{ID: "github", Key: fmt.Sprintf("%d:%s:%s", v.UserID, v.AuthMethod, v.App.ClientID), State: state, Detail: detail}
}
func (s githubCatalogSource) Discover(ctx context.Context) ([]repositorycatalog.Entry, error) {
	rows := []repositorycatalog.Entry{}
	limit := s.b.Config.RepositoryCatalog.PageLimit()
	err := s.b.GitHub.WithAuthorization(ctx, func(token, method string) error {
		installations := []int64{0}
		if method != "personal_token" {
			installations = nil
			for page := 1; ; page++ {
				if page > limit {
					return errors.New("GitHub installation discovery is incomplete: page limit reached")
				}
				items, more, err := s.b.GitHubAPI.Installations(ctx, token, page)
				if err != nil {
					return err
				}
				for _, item := range items {
					installations = append(installations, item.ID)
				}
				if !more {
					break
				}
			}
		}
		for _, id := range installations {
			for page := 1; ; page++ {
				if page > limit {
					return errors.New("GitHub repository discovery is incomplete: page limit reached")
				}
				var items []github.Repository
				var more bool
				var err error
				if method == "personal_token" {
					items, more, err = s.b.GitHubAPI.UserRepositories(ctx, token, page)
				} else {
					items, more, err = s.b.GitHubAPI.Repositories(ctx, token, id, page)
				}
				if err != nil {
					return err
				}
				for _, item := range items {
					if !item.Disabled {
						rows = append(rows, repositorycatalog.Entry{Identity: "github:" + strings.ToLower(item.FullName), Name: item.FullName, GitHubID: item.ID})
					}
				}
				if !more {
					break
				}
			}
		}
		return nil
	})
	if err != nil {
		return nil, errors.New("GitHub discovery failed. Check connection access and retry; saved results are retained.")
	}
	return rows, nil
}
func (b *Bench) RepositoryCatalogSnapshot(ctx context.Context, refresh bool) ([]repositorycatalog.Entry, []repositorycatalog.SourceState, error) {
	ctx, cancel := context.WithTimeout(ctx, 90*time.Second)
	defer cancel()
	remote, state, err := b.RepositoryCatalog.Read(ctx, githubCatalogSource{b}, refresh)
	if err != nil {
		return nil, nil, err
	}
	local := []repositorycatalog.Entry{}
	registered := []repositorycatalog.Entry{}
	localState := repositorycatalog.SourceState{ID: "local", State: "ready", Detail: "Saved local discovery results."}
	for _, root := range b.RepositoryDiscovery.List().Roots {
		if root.ScannedAt > localState.UpdatedAt {
			localState.UpdatedAt = root.ScannedAt
		}
		if root.Truncated || root.Skipped > 0 {
			localState.State = "partial"
			localState.Detail = "Some folders were skipped or the scan limit was reached. Choose a narrower folder to find more repositories."
		}
		for _, r := range root.Candidates {
			name := r.Name
			if strings.HasPrefix(r.GitIdentity, "github:") {
				name = strings.TrimPrefix(r.GitIdentity, "github:")
			}
			local = append(local, repositorycatalog.Entry{Identity: r.GitIdentity, Name: name, LocalPath: r.Path})
		}
	}
	if b.Collections != nil {
		for _, r := range b.Collections.Repositories() {
			identity := b.Collections.RepositoryIdentity(r.ID)
			name := r.Name
			if name == "" {
				name = r.ID
			}
			if strings.HasPrefix(identity, "github:") {
				name = strings.TrimPrefix(identity, "github:")
			}
			registered = append(registered, repositorycatalog.Entry{Identity: identity, Name: name, RepositoryID: r.ID, LocalPath: r.Path, GitHubID: r.GitHubID})
		}
	}
	return repositorycatalog.Merge(registered, local, remote), []repositorycatalog.SourceState{state, localState}, nil
}

// PrepareWorkspace owns source materialization; clients submit stable identities,
// never temporary UI paths. Successful sources are reused when the request retries.
func (b *Bench) PrepareWorkspace(ctx context.Context, key, workspaceID, name string, identities []string, progress func(string, string) error) (collections.Collection, error) {
	if b.Collections == nil {
		return collections.Collection{}, errors.New("workspace storage unavailable")
	}
	if strings.TrimSpace(key) == "" {
		return collections.Collection{}, errors.New("request key is required")
	}
	if len(identities) > 64 {
		return collections.Collection{}, errors.New("select at most 64 repositories")
	}
	if workspaceID == "" && (strings.TrimSpace(name) == "" || len(name) > 128 || strings.IndexFunc(name, unicode.IsControl) >= 0) {
		return collections.Collection{}, errors.New("workspace name must contain 1–128 bytes")
	}
	if workspaceID != "" {
		if _, err := b.Collections.Get(workspaceID); err != nil {
			return collections.Collection{}, err
		}
	}
	rows, _, err := b.RepositoryCatalogSnapshot(ctx, false)
	if err != nil {
		return collections.Collection{}, err
	}
	byID := map[string]repositorycatalog.Entry{}
	for _, row := range rows {
		byID[row.Identity] = row
	}
	seen := map[string]bool{}
	for _, id := range identities {
		if _, ok := byID[id]; !ok {
			return collections.Collection{}, fmt.Errorf("repository %s is no longer in the catalog; refresh and retry", id)
		}
		if seen[id] {
			return collections.Collection{}, errors.New("duplicate repository selection")
		}
		seen[id] = true
	}
	members := []collections.MemberSpec{}
	for _, id := range identities {
		row := byID[id]
		if err = progress(id, "Preparing "+row.Name); err != nil {
			return collections.Collection{}, err
		}
		repoID := row.RepositoryID
		if repoID == "" {
			var repo collections.Repository
			if row.LocalPath != "" {
				repo, err = b.Collections.AddLocalRepository(ctx, row.LocalPath, "")
			} else if row.GitHubID > 0 {
				err = b.GitHub.WithCredential(ctx, func(token string) error {
					remote, e := b.GitHubAPI.Repository(ctx, token, row.GitHubID)
					if e != nil {
						return e
					}
					if "github:"+strings.ToLower(remote.FullName) != id {
						return errors.New("repository identity changed; refresh and retry")
					}
					repo, e = b.RepositoryImporter.ImportWithProgress(ctx, remote, token, func(message string) error { return progress(id, row.Name+" · "+message) })
					return e
				})
			} else {
				err = errors.New("repository has no usable source")
			}
			if err != nil {
				return collections.Collection{}, fmt.Errorf("could not prepare %s: %w", row.Name, err)
			}
			repoID = repo.ID
		}
		members = append(members, collections.MemberSpec{RepositoryID: repoID, BaseRef: "HEAD"})
	}
	if err = progress("", "Creating worktrees"); err != nil {
		return collections.Collection{}, err
	}
	if workspaceID != "" {
		return b.Collections.AddRepositories(ctx, workspaceID, key, members)
	}
	return b.Collections.Create(ctx, key, collections.Spec{Name: strings.TrimSpace(name), Members: members})
}
