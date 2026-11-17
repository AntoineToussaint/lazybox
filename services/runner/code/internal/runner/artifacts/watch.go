package artifacts

import (
	"context"
	"time"
)

// ReadFile returns a tracked workspace file's text. problem explains why the
// file cannot be followed right now (removed, not text, too large) without it
// being a failure of the Runner; err is any other failure to read it.
type ReadFile func(ctx context.Context, workspace, path string) (text, problem string, err error)

// Watch polls every tracked file until ctx ends. A change becomes a version
// once the same new content has been seen for at least debounce, so an agent
// writing a file in several steps produces one version, not one per write.
func (s *Store) Watch(ctx context.Context, read ReadFile, interval, debounce time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		s.Poll(ctx, read, debounce)
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
	}
}

type tracked struct {
	workspace, id, path, digest string
}

// Poll checks every tracked file once.
func (s *Store) Poll(ctx context.Context, read ReadFile, debounce time.Duration) {
	s.mu.Lock()
	var files []tracked
	for ws, items := range s.items {
		for _, a := range items {
			if a.TrackedPath != "" {
				files = append(files, tracked{ws, a.ID, a.TrackedPath, a.Versions[len(a.Versions)-1].Digest})
			}
		}
	}
	s.mu.Unlock()
	for _, f := range files {
		if ctx.Err() != nil {
			return
		}
		text, problem, err := read(ctx, f.workspace, f.path)
		if err != nil && problem == "" {
			if ctx.Err() != nil {
				return
			}
			problem = "the file cannot be read: " + err.Error()
		}
		if problem == "" {
			if verr := validContent(text); verr != nil {
				problem = "the file is empty or not text"
			}
		}
		s.observe(f, text, problem, debounce)
	}
}

func (s *Store) observe(f tracked, text, problem string, debounce time.Duration) {
	s.mu.Lock()
	defer s.mu.Unlock()
	key := f.workspace + "/" + f.id
	a := s.items[f.workspace][f.id]
	// Untracked, retargeted or deleted while the file was being read.
	if a == nil || a.TrackedPath != f.path {
		return
	}
	if problem != "" {
		delete(s.pending, key)
		s.setProblem(a, problem)
		return
	}
	digest := Digest(text)
	if digest == a.Versions[len(a.Versions)-1].Digest {
		delete(s.pending, key)
		s.setProblem(a, "")
		return
	}
	now := s.now()
	seen, ok := s.pending[key]
	if !ok || seen.digest != digest {
		s.pending[key] = sighting{digest: digest, since: now}
		return
	}
	if now.Sub(seen.since) < debounce {
		return
	}
	delete(s.pending, key)
	next := clone(a)
	next.TrackingProblem = ""
	if err := s.append(&next, text, Author{Kind: "file"}, Source{Kind: "file", Path: f.path}); err != nil {
		s.setProblem(a, err.Error())
		return
	}
	*a = next
}

// setProblem records (or clears) why the file is not followed, persisting
// only a change.
func (s *Store) setProblem(a *Artifact, problem string) {
	if a.TrackingProblem == problem {
		return
	}
	next := clone(a)
	next.TrackingProblem = problem
	if err := s.save(&next); err != nil {
		return // not recorded; the next poll tries again
	}
	*a = next
}
