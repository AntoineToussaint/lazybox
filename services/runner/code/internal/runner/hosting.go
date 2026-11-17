package runner

import (
	"context"
	"fmt"
	"log"
	"sync"
	"sync/atomic"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

// ProcessHosting is the Runner configuration of how agents, managed commands
// and terminals are hosted so they outlive the Runner. See SESSION_HOSTS.md.
type ProcessHosting struct {
	// Backend is "sessionhost" (the default; also "") or "tmux". Settings →
	// Runner can choose another; that choice wins over this one.
	Backend string `json:"backend,omitempty"`
	// Tmux is the tmux executable; "tmux" on the Runner's PATH when empty.
	Tmux string `json:"tmux,omitempty"`
	// TmuxConfig is the file the Runner's private tmux server loads; none when
	// empty, so a personal tmux.conf cannot change how hosts behave.
	TmuxConfig string `json:"tmuxConfig,omitempty"`
}

func validBackend(name string) bool {
	return name == "" || name == sessionhost.BackendSessionHost || name == sessionhost.BackendTmux
}

// Validate rejects an unknown backend.
func (p ProcessHosting) Validate() error {
	if !validBackend(p.Backend) {
		return fmt.Errorf("unknown process backend %q: use %q or %q", p.Backend, sessionhost.BackendSessionHost, sessionhost.BackendTmux)
	}
	return nil
}

// hosting is the Runner's Backend: it starts each new host with the backend
// selected now (Settings → Runner, else the Runner configuration), so a change
// applies to the next process without a restart, and running processes keep
// the backend they started with. When tmux is selected but cannot host a
// process, the process still starts, in a session host, and the reason is kept
// for the Processes view: an optional backend never stops an agent starting.
//
// Every other operation is shared by every backend (sessionhost.Backend).
type hosting struct {
	sessionhost.Local
	config ProcessHosting
	tmux   sessionhost.Tmux
	// preferred is the person's choice in Settings, "" for none.
	preferred atomic.Pointer[func() string]

	mu sync.Mutex
	// fallback says why the last process that should have used tmux did not.
	fallback string
	// checked caches tmux -V for the Processes view, which polls.
	checked                time.Time
	tmuxVersion, tmuxError string
}

// tmuxCheck is how long a tmux -V answer is reused: installing tmux shows up
// within it, and a polling view does not start a process every few seconds.
const tmuxCheck = 30 * time.Second

func newHosting(config ProcessHosting) *hosting {
	return &hosting{config: config, tmux: sessionhost.Tmux{Path: config.Tmux, Config: config.TmuxConfig}}
}

// Selected is the backend new processes should use.
func (h *hosting) Selected() string {
	if f := h.preferred.Load(); f != nil {
		if choice := (*f)(); choice != "" {
			return choice
		}
	}
	if h.config.Backend != "" {
		return h.config.Backend
	}
	return sessionhost.BackendSessionHost
}

// Name is the backend new processes use.
func (h *hosting) Name() string { return h.Selected() }

func (h *hosting) Launch(ctx context.Context, root string, opts sessionhost.Options) (*sessionhost.Client, error) {
	if h.Selected() == sessionhost.BackendTmux {
		c, err := h.tmux.Launch(ctx, root, opts)
		if err == nil {
			h.setFallback("")
			return c, nil
		}
		if ctx.Err() != nil {
			return nil, err
		}
		reason := fmt.Sprintf("tmux could not host the last new process, so it runs in a session host: %v", err)
		h.setFallback(reason)
		log.Printf("process hosting: %s", reason)
	}
	return h.Local.Launch(ctx, root, opts)
}

func (h *hosting) setFallback(reason string) {
	h.mu.Lock()
	h.fallback = reason
	h.mu.Unlock()
}

// HostingState is what the Processes view and Settings show about hosting.
type HostingState struct {
	// Selected is the backend chosen for new processes; Configured the Runner
	// configuration's (what "Default" means in Settings).
	Selected, Configured string
	// TmuxVersion is tmux's -V, when it is usable; TmuxProblem says otherwise.
	TmuxVersion, TmuxProblem string
	// Fallback says why the last process meant for tmux did not get it.
	Fallback string
}

func (h *hosting) state(ctx context.Context) HostingState {
	s := HostingState{Selected: h.Selected(), Configured: h.config.Backend}
	if s.Configured == "" {
		s.Configured = sessionhost.BackendSessionHost
	}
	h.mu.Lock()
	stale := time.Since(h.checked) > tmuxCheck
	h.mu.Unlock()
	if stale {
		version, err := h.tmux.Version(ctx)
		h.mu.Lock()
		h.checked, h.tmuxVersion, h.tmuxError = time.Now(), version, ""
		if err != nil {
			h.tmuxVersion, h.tmuxError = "", err.Error()
		}
		h.mu.Unlock()
	}
	h.mu.Lock()
	s.TmuxVersion, s.TmuxProblem = h.tmuxVersion, h.tmuxError
	if s.Selected == sessionhost.BackendTmux {
		s.Fallback = h.fallback
	}
	h.mu.Unlock()
	return s
}
