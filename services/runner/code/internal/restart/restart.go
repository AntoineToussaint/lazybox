// Package restart lets the owner ask the launcher that supervises a Runner to
// start it again.
//
// The Runner never restarts itself. It records the request in a file the
// supervising launcher named (LAZYBOX_V2_RESTART_REQUEST) and then stops
// through its ordinary shutdown, the same one a supervisor's SIGTERM or
// Ctrl+C reaches: agents, managed commands and terminals are detached, never
// stopped, exactly as on any other restart. The launcher sees the Runner exit,
// finds the request, and starts the Runner again. Without a supervisor nothing
// would start it again, so the request is unavailable.
package restart

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"syscall"
	"time"
)

// Environment names the file a supervising launcher reads after the Runner
// exits. Its presence is the launcher's promise to start the Runner again.
const Environment = "LAZYBOX_V2_RESTART_REQUEST"

// Version identifies the request file's format for the launcher.
const Version = 1

// Request is the record a launcher reads to tell a requested restart from any
// other exit.
type Request struct {
	Version     int       `json:"version"`
	PID         int       `json:"pid"`
	RequestedAt time.Time `json:"requestedAt"`
}

// Supervisor is a launcher that will start this Runner again.
type Supervisor struct {
	// Path is where the launcher reads the request.
	Path string
	// Stop begins this process's ordinary shutdown.
	Stop func()

	mu        sync.Mutex
	requested bool
}

// FromEnvironment returns the supervisor the launcher declared, or nil when
// none did. keepAgents must hold too: a Runner that stops its agents on exit
// would not keep the promise a restart makes to the person asking for it.
func FromEnvironment(keepAgents bool) (*Supervisor, error) {
	path := os.Getenv(Environment)
	if path == "" || !keepAgents {
		return nil, nil
	}
	if !filepath.IsAbs(path) {
		return nil, fmt.Errorf("%s must be an absolute path", Environment)
	}
	return &Supervisor{Path: filepath.Clean(path), Stop: Terminate}, nil
}

// Terminate delivers SIGTERM to this process: the signal the generated main
// already shuts down on, so a requested restart takes the one shutdown path a
// launcher's own stop takes rather than a second one beside it.
func Terminate() {
	_ = syscall.Kill(os.Getpid(), syscall.SIGTERM)
}

// Request records the restart and begins the shutdown. A second request while
// the first is under way is accepted and does nothing more.
func (s *Supervisor) Request() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.requested {
		return nil
	}
	if err := write(s.Path, Request{Version: Version, PID: os.Getpid(), RequestedAt: time.Now().UTC()}); err != nil {
		return err
	}
	s.requested = true
	// The caller is still answering its RPC; shutdown drains in-flight
	// requests, so the answer reaches the client before the process exits.
	go s.Stop()
	return nil
}

// write replaces the request atomically, so a launcher never reads half of it.
func write(path string, request Request) error {
	data, err := json.Marshal(request)
	if err != nil {
		return err
	}
	file, err := os.CreateTemp(filepath.Dir(path), ".restart-")
	if err != nil {
		return fmt.Errorf("record restart request: %w", err)
	}
	name := file.Name()
	_, err = file.Write(data)
	if err == nil {
		err = file.Sync()
	}
	err = errors.Join(err, file.Close())
	if err == nil {
		err = os.Rename(name, path)
	}
	if err != nil {
		_ = os.Remove(name)
		return fmt.Errorf("record restart request: %w", err)
	}
	return nil
}
