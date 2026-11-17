package restart

import (
	"context"
	"encoding/json"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"testing"
	"time"
)

func TestRequestRecordsTheRestartBeforeStoppingOnce(t *testing.T) {
	path := filepath.Join(t.TempDir(), "restart-request.json")
	stopped := make(chan struct{}, 2)
	s := &Supervisor{Path: path, Stop: func() {
		// The request must be on disk before shutdown begins: the launcher
		// reads it only after the Runner has exited.
		if _, err := os.Stat(path); err != nil {
			t.Errorf("stop began before the request was recorded: %v", err)
		}
		stopped <- struct{}{}
	}}
	if err := s.Request(); err != nil {
		t.Fatal(err)
	}
	if err := s.Request(); err != nil {
		t.Fatal("a repeated request while stopping must be accepted", err)
	}
	select {
	case <-stopped:
	case <-time.After(5 * time.Second):
		t.Fatal("shutdown never began")
	}
	select {
	case <-stopped:
		t.Fatal("a repeated request began a second shutdown")
	case <-time.After(100 * time.Millisecond):
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var request Request
	if err = json.Unmarshal(data, &request); err != nil {
		t.Fatal(err)
	}
	if request.Version != Version || request.PID != os.Getpid() || request.RequestedAt.IsZero() {
		t.Fatalf("unexpected request %+v", request)
	}
	info, err := os.Stat(path)
	if err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("request file must be private: %v %v", info.Mode(), err)
	}
	if leftovers, _ := filepath.Glob(filepath.Join(filepath.Dir(path), ".restart-*")); len(leftovers) != 0 {
		t.Fatalf("temporary files left behind: %v", leftovers)
	}
}

// A request that cannot be recorded must not stop the Runner: the launcher
// would read the exit as a failure and stop the whole environment.
func TestUnrecordedRequestDoesNotStop(t *testing.T) {
	s := &Supervisor{Path: filepath.Join(t.TempDir(), "missing", "restart-request.json"), Stop: func() {
		t.Error("stopped without a recorded request")
	}}
	if err := s.Request(); err == nil {
		t.Fatal("expected an error for an unwritable request")
	}
	time.Sleep(50 * time.Millisecond)
}

func TestFromEnvironmentRequiresALauncherThatKeepsAgents(t *testing.T) {
	path := filepath.Join(t.TempDir(), "restart-request.json")
	t.Setenv(Environment, "")
	if s, err := FromEnvironment(true); s != nil || err != nil {
		t.Fatal("no launcher declared a restart, yet one is available", s, err)
	}
	t.Setenv(Environment, path)
	if s, err := FromEnvironment(false); s != nil || err != nil {
		t.Fatal("a Runner that stops its agents on exit offered a restart", s, err)
	}
	s, err := FromEnvironment(true)
	if err != nil || s == nil || s.Path != path || s.Stop == nil {
		t.Fatal("launcher-declared restart not available", s, err)
	}
	t.Setenv(Environment, "relative/restart.json")
	if _, err = FromEnvironment(true); err == nil {
		t.Fatal("relative request path accepted")
	}
}

// Terminate reaches the same signal the generated main's NotifyContext waits
// on, so a requested restart runs the ordinary shutdown (Bench.Detach, then
// the listeners drain) and nothing else.
func TestTerminateTakesTheOrdinaryShutdownPath(t *testing.T) {
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	Terminate()
	select {
	case <-ctx.Done():
	case <-time.After(5 * time.Second):
		t.Fatal("SIGTERM never reached the shutdown context")
	}
}
