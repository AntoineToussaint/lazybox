package runner

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agentshell"
)

// Agents run every command through the person's shell, whose startup files
// may put another lazybox (V1's, in ~/.cargo/bin or Homebrew) first on PATH.
// The Runner gives every agent, managed command and workspace terminal the
// shell integration of internal/agentshell, probes which lazybox their shells
// reach, and tells the person when something else shadows its CLI.

// agentCLIState is the shell integration and the last probe of it.
type agentCLIState struct {
	install sync.Once
	ready   bool
	mu      sync.Mutex
	check   *agentshell.Check
	probing chan struct{}
	probes  sync.WaitGroup
	// stopped is set when the Runner shuts down: no probe starts after.
	stopped bool
	// told serializes the once-per-shadow notice between conversations.
	told sync.Mutex
}

// stop waits for probes and notices in progress; none start after it.
func (s *agentCLIState) stop() {
	s.mu.Lock()
	s.stopped = true
	s.mu.Unlock()
	s.probes.Wait()
}

// track counts a background task, unless the Runner is shutting down.
func (s *agentCLIState) track() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.stopped {
		return false
	}
	s.probes.Add(1)
	return true
}

// cliCheckTTL is how long a probe stands: startup files change rarely.
const cliCheckTTL = 10 * time.Minute

func (b *Bench) shellIntegrationDir() string {
	return filepath.Join(b.Config.State, "shell-integration")
}

// withCLI puts the lazybox CLI first on PATH, and adds the shell integration
// that keeps it there whatever the person's startup files do, so agents, the
// commands they start and workspace terminals run this Runner's CLI rather
// than another lazybox on the system. The integration files are written on
// first use; if they cannot be, the CLI is still first on the initial PATH.
func (b *Bench) withCLI(env []string) []string {
	if b.Config.CLIDir == "" {
		return env
	}
	s := &b.agentCLI
	s.install.Do(func() { s.ready = agentshell.Install(b.shellIntegrationDir()) == nil })
	dir := ""
	if s.ready {
		dir = b.shellIntegrationDir()
	}
	return agentshell.Env(env, dir, b.Config.CLIDir)
}

// cliProgram is this Runner's CLI, or "" when it has none to probe.
func (b *Bench) cliProgram() string {
	if b.Config.CLIDir == "" {
		return ""
	}
	program := filepath.Join(b.Config.CLIDir, "lazybox")
	if info, err := os.Stat(program); err != nil || !info.Mode().IsRegular() || info.Mode()&0o111 == 0 {
		return ""
	}
	return program
}

// AgentCLI is the last probe of which lazybox agents' shells reach, or nil
// before the first finishes. A stale or missing probe starts a new one in the
// background; done is closed when it ends.
func (b *Bench) AgentCLI() (check *agentshell.Check, done <-chan struct{}) {
	program := b.cliProgram()
	if program == "" {
		closed := make(chan struct{})
		close(closed)
		return nil, closed
	}
	s := &b.agentCLI
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.probing == nil && !s.stopped && (s.check == nil || time.Since(s.check.CheckedAt) > cliCheckTTL) {
		s.probing = make(chan struct{})
		s.probes.Add(1)
		go b.probeAgentCLI(program, s.probing)
	}
	done = s.probing
	if done == nil {
		closed := make(chan struct{})
		close(closed)
		done = closed
	}
	if s.check != nil {
		c := *s.check
		check = &c
	}
	return check, done
}

func (b *Bench) probeAgentCLI(program string, done chan struct{}) {
	s := &b.agentCLI
	defer s.probes.Done()
	base := childEnv()
	// Without the integration: what the person's startup files do on their own.
	raw := agentshell.Env(base, "", b.Config.CLIDir)
	check := agentshell.Probe(b.ctx, agentshell.ProbeShells(os.Getenv), raw, b.withCLI(base), program)
	s.mu.Lock()
	if b.ctx.Err() == nil {
		s.check = &check
	}
	s.probing = nil
	s.mu.Unlock()
	close(done)
}

// cliNoticeFile remembers the shadow the person was last told about, so a
// shadow Lazybox works around is announced once, not in every conversation.
func (b *Bench) cliNoticeFile() string {
	return filepath.Join(b.Config.State, "shell-integration-notice.json")
}

// announceAgentCLI tells a new coding conversation, in a Runner line, when
// another lazybox shadows this Runner's CLI: every time while the shell
// integration cannot fix it (the agent then needs the full path), once per
// shadow while it does.
func (b *Bench) announceAgentCLI(e *Execution) {
	if agent.IsAssistant(e.Purpose) || b.cliProgram() == "" {
		return
	}
	_, done := b.AgentCLI()
	if !b.agentCLI.track() {
		return
	}
	go func() {
		defer b.agentCLI.probes.Done()
		select {
		case <-done:
		case <-b.ctx.Done():
			return
		}
		check, _ := b.AgentCLI()
		if check == nil || check.Notice() == "" {
			return
		}
		b.agentCLI.told.Lock()
		defer b.agentCLI.told.Unlock()
		if check.Fixed() {
			var told struct {
				Shadow string `json:"shadow"`
			}
			if data, err := os.ReadFile(b.cliNoticeFile()); err == nil && json.Unmarshal(data, &told) == nil && told.Shadow == check.Shadow {
				return
			}
		}
		if s := e.Engine.Snapshot(); s.Closed {
			return
		}
		data, _ := json.Marshal(map[string]any{"cli": check.CLI, "shadow": check.Shadow, "fixed": check.Fixed(), "unfixed": check.Unfixed})
		if err := b.Record(e, agent.Event{Type: "cli_notice", Text: check.Notice(), Status: map[bool]string{true: "fixed", false: "unfixed"}[check.Fixed()], Data: data}); err != nil {
			return
		}
		if check.Fixed() {
			told, _ := json.Marshal(map[string]string{"shadow": check.Shadow})
			_ = writeFileAtomic(b.cliNoticeFile(), told)
		}
	}()
}

// writeFileAtomic replaces a small state file, readable by the user alone.
func writeFileAtomic(path string, data []byte) error {
	f, err := os.CreateTemp(filepath.Dir(path), ".write-*")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	if closeErr := f.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	return os.Rename(f.Name(), path)
}
