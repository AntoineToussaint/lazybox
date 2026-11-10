// Package processguard binds pipe-based child groups to their owner's lifetime.
// It preserves execution output in the caller's journal; it is not a reconnectable
// supervisor or a container/cgroup boundary against deliberately detached children.
// Agents are guarded by their session host (internal/sessionhost) rather than
// the Runner, which is what lets them outlive a Runner restart.
package processguard

import (
	"encoding/json"
	"errors"
	"io"
	"os"
	"os/exec"
	"sync"
	"syscall"
	"time"
)

const helperArgument = "--lazybox-owned-process-v1"

// ErrCleanupUnconfirmed means the helper disappeared without completing its
// cleanup protocol. A caller must not release admission on that evidence.
var ErrCleanupUnconfirmed = errors.New("owned process exited without cleanup evidence")

// Re-exec the importing binary in a private helper mode before its normal entry
// point. This also works for Go test binaries; no separately installed executable
// or machine-specific path is needed. Extra descriptors are the ownership proof.
func init() {
	if len(os.Args) >= 3 && os.Args[1] == helperArgument {
		os.Exit(run(os.Args[2:]))
	}
}

type Outcome struct {
	Started   bool   `json:"started"`
	ExitCode  int    `json:"exitCode"`
	Signal    string `json:"signal,omitempty"`
	Error     string `json:"error,omitempty"`
	Cancelled bool   `json:"cancelled,omitempty"`
}
type Guard struct {
	cmd     *exec.Cmd
	live    *os.File
	result  *os.File
	stop    sync.Once
	stopErr error
	wait    sync.Once
	outcome Outcome
	err     error
	output  []*capture
}

type capture struct {
	read, write *os.File
	destination io.Writer
	done        chan error
}

type lockedWriter struct {
	mu     *sync.Mutex
	writer io.Writer
}

func (w lockedWriter) Write(data []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.writer.Write(data)
}

// Start takes an unstarted command. The helper is its process-group leader and
// passes stdio/environment/cwd unchanged to the actual program. A dedicated pipe,
// never protocol stdin or a PID poll, detects loss of the Runner even under SIGKILL.
func Start(cmd *exec.Cmd) (*Guard, error) {
	if cmd == nil || len(cmd.Args) == 0 || cmd.Process != nil || len(cmd.ExtraFiles) != 0 || cmd.SysProcAttr != nil || cmd.Cancel != nil {
		return nil, errors.New("guard requires an unstarted command without extra descriptors, process attributes or cancellation hooks")
	}
	if cmd.Err != nil {
		return nil, cmd.Err
	}
	binary, err := os.Executable()
	if err != nil {
		return nil, err
	}
	liveRead, liveWrite, err := os.Pipe()
	if err != nil {
		return nil, err
	}
	resultRead, resultWrite, err := os.Pipe()
	if err != nil {
		liveRead.Close()
		liveWrite.Close()
		return nil, err
	}
	// os/exec masks a copy/WaitDelay error when the helper exits by signal.
	// Own these copies explicitly so SIGKILL cleanup cannot hide retained pipes.
	var output []*capture
	var outputMu sync.Mutex
	for _, writer := range []*io.Writer{&cmd.Stdout, &cmd.Stderr} {
		if *writer == nil {
			continue
		}
		if _, file := (*writer).(*os.File); file {
			continue
		}
		read, write, pipeErr := os.Pipe()
		if pipeErr != nil {
			for _, c := range output {
				c.read.Close()
				c.write.Close()
			}
			liveRead.Close()
			liveWrite.Close()
			resultRead.Close()
			resultWrite.Close()
			return nil, pipeErr
		}
		output = append(output, &capture{read: read, write: write, destination: lockedWriter{&outputMu, *writer}, done: make(chan error, 1)})
		*writer = write
	}
	target := cmd.Path
	argv := append([]string{}, cmd.Args[1:]...)
	cmd.Path = binary
	cmd.Args = append([]string{binary, helperArgument, target}, argv...)
	cmd.ExtraFiles = []*os.File{liveRead, resultWrite}
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if cmd.WaitDelay == 0 {
		cmd.WaitDelay = time.Second
	}
	err = cmd.Start()
	liveRead.Close()
	resultWrite.Close()
	cmd.ExtraFiles = nil
	for _, c := range output {
		c.write.Close()
	}
	if err != nil {
		for _, c := range output {
			c.read.Close()
		}
		liveWrite.Close()
		resultRead.Close()
		return nil, err
	}
	for _, c := range output {
		go func() { _, err := io.Copy(c.destination, c.read); c.read.Close(); c.done <- err }()
	}
	return &Guard{cmd: cmd, live: liveWrite, result: resultRead, output: output}, nil
}

// Wait reaps the helper and returns the actual program's outcome. On normal exit
// the helper records this outcome, then kills its own entire group, including
// background descendants. Its own PID pins the group identity until that kill.
func (g *Guard) Wait() (Outcome, error) {
	g.wait.Do(func() { g.outcome, g.err = g.waitResult() })
	return g.outcome, g.err
}

// Stop relinquishes this attempt's lifetime capability. The still-running
// helper kills its own group; the caller never signals a possibly reused PID.
// Wait must still confirm the helper exited and emitted cleanup evidence.
func (g *Guard) Stop() error {
	g.stop.Do(func() { g.stopErr = g.live.Close() })
	return g.stopErr
}

func (g *Guard) waitResult() (Outcome, error) {
	err := g.cmd.Wait()
	_ = g.Stop()
	drainErr := g.drain()
	defer g.result.Close()
	var outcome Outcome
	decodeErr := json.NewDecoder(io.LimitReader(g.result, 4096)).Decode(&outcome)
	if decodeErr == nil {
		if drainErr != nil {
			return outcome, drainErr
		}
		if outcome.Error != "" {
			return outcome, errors.New(outcome.Error)
		}
		var exit *exec.ExitError
		if err != nil && !errors.As(err, &exit) {
			// An escaped descendant can retain output descriptors after the helper
			// exits. WaitDelay/pipe failures are not successful resource cleanup.
			return outcome, ErrCleanupUnconfirmed
		}
		// A normal helper cleanup intentionally exits via SIGKILL.
		return outcome, nil
	}
	if g.cmd.ProcessState != nil {
		outcome.Started = true
		outcome.ExitCode = g.cmd.ProcessState.ExitCode()
		if status, ok := g.cmd.ProcessState.Sys().(syscall.WaitStatus); ok && status.Signaled() {
			outcome.Signal = status.Signal().String()
		}
	}
	if err != nil {
		return outcome, ErrCleanupUnconfirmed
	}
	return outcome, ErrCleanupUnconfirmed
}

func (g *Guard) drain() error {
	timer := time.NewTimer(g.cmd.WaitDelay)
	defer timer.Stop()
	defer func() {
		for _, c := range g.output {
			c.read.Close()
		}
	}()
	for _, c := range g.output {
		select {
		case err := <-c.done:
			if err != nil {
				return ErrCleanupUnconfirmed
			}
		case <-timer.C:
			return ErrCleanupUnconfirmed
		}
	}
	return nil
}
func run(argv []string) int {
	// Refuse an unowned invocation; never signal a caller's inherited process group.
	live := os.NewFile(3, "runner-lifetime")
	result := os.NewFile(4, "process-result")
	if live == nil || result == nil {
		return 125
	}
	a, err := live.Stat()
	if err != nil || a.Mode()&os.ModeNamedPipe == 0 {
		return 125
	}
	a, err = result.Stat()
	if err != nil || a.Mode()&os.ModeNamedPipe == 0 {
		return 125
	}
	group, err := syscall.Getpgid(0)
	if err != nil || group != os.Getpid() {
		return 125
	}
	syscall.CloseOnExec(3)
	syscall.CloseOnExec(4)
	var finish sync.Once
	// A group kill only reaches processes already in the group. A cancellation
	// racing child.Start could sweep the group while the fork is in flight and
	// leave the new child alive outside any owner. starting serialises the two:
	// either the program never starts, or it is in the group when it is killed.
	var starting sync.Mutex
	terminating := false
	terminate := func(outcome Outcome) {
		finish.Do(func() {
			starting.Lock()
			terminating = true
			starting.Unlock()
			_ = json.NewEncoder(result).Encode(outcome)
			result.Close()
			_ = syscall.Kill(-group, syscall.SIGKILL)
		})
	}
	go func() {
		_, _ = io.Copy(io.Discard, live)
		terminate(Outcome{ExitCode: -1, Signal: syscall.SIGKILL.String(), Cancelled: true})
	}()
	child := exec.Command(argv[0], argv[1:]...)
	child.Stdin = os.Stdin
	child.Stdout = os.Stdout
	child.Stderr = os.Stderr
	// Inherit the helper's group. The guard remains alive while cleanup signals it,
	// so it never targets a re-used persisted PID or a global tmux/container server.
	outcome := Outcome{ExitCode: 125}
	starting.Lock()
	if terminating {
		starting.Unlock()
		select {} // terminate is killing this group, the helper included.
	}
	err = child.Start()
	starting.Unlock()
	if err != nil {
		outcome.Error = "owned program could not start"
	} else {
		outcome.Started = true
		err = child.Wait()
		outcome.ExitCode = child.ProcessState.ExitCode()
		if status, ok := child.ProcessState.Sys().(syscall.WaitStatus); ok && status.Signaled() {
			outcome.Signal = status.Signal().String()
		}
		var exit *exec.ExitError
		if err != nil && !errors.As(err, &exit) {
			outcome.Error = "owned program wait failed"
		}
	}
	terminate(outcome)
	return 125
}
