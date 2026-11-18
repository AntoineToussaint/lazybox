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
	"os/signal"
	"sync"
	"syscall"
	"time"

	"golang.org/x/sys/unix"
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
	cmd    *exec.Cmd
	live   *os.File
	result *os.File
	// liveMu orders Interrupt's writes with Stop's close of live.
	liveMu  sync.Mutex
	stopped bool
	stopErr error
	wait    sync.Once
	outcome Outcome
	err     error
	output  []*capture
	// pid is an adopted helper's (Adopt); a started guard's is its command's.
	pid int
	// handMu orders Release with the start of Wait's read of the outcome:
	// exactly one of them gets the result pipe.
	handMu   sync.Mutex
	released bool
	reading  bool
}

// ErrReleased means the guard handed its helper to another process (Release):
// that process stops it and reads its outcome now.
var ErrReleased = errors.New("owned process was handed over to another owner")

// PID is the helper's process ID, which is also the process group of the
// program and everything it starts. It identifies the tree to inspect (its
// listening ports, say), never one to signal: Interrupt and Stop do that.
func (g *Guard) PID() int {
	if g.cmd != nil && g.cmd.Process != nil {
		return g.cmd.Process.Pid
	}
	return g.pid
}

// Release hands the helper to another owner, for a program that must outlive
// this process (a `lazybox run` that moves to the background). It returns the
// lifetime pipe's write end and the outcome pipe's read end, to pass to the
// new owner (as inherited descriptors), which rebuilds the guard with Adopt.
// Afterwards this guard neither stops the helper nor reads its outcome: Stop
// is a no-op and Wait returns ErrReleased. The caller closes its copies once
// the new owner holds them. Only a guard whose program writes to files the
// caller passed (no output this guard copies) can be released, and only
// before Wait has started reading the outcome.
func (g *Guard) Release() (live, result *os.File, err error) {
	if len(g.output) > 0 {
		return nil, nil, errors.New("a guard that copies its program's output cannot be handed over")
	}
	g.handMu.Lock()
	defer g.handMu.Unlock()
	if g.reading {
		return nil, nil, errors.New("the owned program already exited")
	}
	g.liveMu.Lock()
	defer g.liveMu.Unlock()
	if g.stopped || g.released {
		return nil, nil, errors.New("owned process is already stopping or handed over")
	}
	g.released = true
	g.stopped = true // Stop must not close the pipe the new owner holds.
	return g.live, g.result, nil
}

// Adopt rebuilds a released guard in its new owner from the descriptors
// Release returned and the helper's pid. Interrupt, Stop and Wait work as on
// the original; Wait reads the program's outcome from the helper, which is no
// longer this process's child, so it cannot also reap it.
func Adopt(live, result *os.File, pid int) *Guard {
	return &Guard{live: live, result: result, pid: pid}
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
// background descendants, until it is empty. Its own PID pins the group
// identity until then.
func (g *Guard) Wait() (Outcome, error) {
	g.wait.Do(func() { g.outcome, g.err = g.waitResult() })
	return g.outcome, g.err
}

// Stop relinquishes this attempt's lifetime capability. The still-running
// helper kills its own group; the caller never signals a possibly reused PID.
// Wait must still confirm the helper exited and emitted cleanup evidence.
func (g *Guard) Stop() error {
	g.liveMu.Lock()
	defer g.liveMu.Unlock()
	if !g.stopped {
		g.stopped = true
		g.stopErr = g.live.Close()
	}
	return g.stopErr
}

// interruptRequest is the one byte the owner writes on the lifetime pipe to
// ask the helper for a graceful stop; closing the pipe still means "kill".
const interruptRequest = 'T'

// Interrupt asks the program to stop gracefully: the helper sends SIGTERM to
// its whole process group (every descendant that did not leave it) and
// survives that itself. The program may exit on its own; Stop is still the
// only guarantee. As with Stop, the owner never signals a PID: the helper,
// alive while it signals, pins the group's identity.
func (g *Guard) Interrupt() error {
	g.liveMu.Lock()
	defer g.liveMu.Unlock()
	if g.stopped {
		return errors.New("owned process is already stopping")
	}
	_, err := g.live.Write([]byte{interruptRequest})
	return err
}

func (g *Guard) waitResult() (Outcome, error) {
	if g.cmd == nil {
		return g.waitAdopted()
	}
	err := g.cmd.Wait()
	g.handMu.Lock()
	if g.released {
		g.handMu.Unlock()
		return Outcome{}, ErrReleased
	}
	g.reading = true
	g.handMu.Unlock()
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
		// A helper that finished its cleanup exits non-zero, or by its own SIGKILL.
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

// waitAdopted reads the outcome an adopted helper writes once its program
// exits. The helper then sweeps the group and exits; with no child to reap,
// the outcome itself is the evidence.
func (g *Guard) waitAdopted() (Outcome, error) {
	defer g.result.Close()
	var outcome Outcome
	if err := json.NewDecoder(io.LimitReader(g.result, 4096)).Decode(&outcome); err != nil {
		_ = g.Stop()
		return outcome, ErrCleanupUnconfirmed
	}
	if outcome.Error != "" {
		return outcome, errors.New(outcome.Error)
	}
	return outcome, nil
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
	// The owner's group, recorded while the owner is surely alive: the helper
	// steps into it to sweep its own group (see sweep).
	owner, _ := syscall.Getpgid(os.Getppid())
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
			sweep(group, owner)
		})
	}
	// The helper shares its group with the program, so a graceful stop's
	// SIGTERM reaches it too. It is caught, never ignored (an ignored signal
	// would be inherited by the program), and the helper keeps guarding until
	// the program exits or its owner closes the lifetime pipe.
	terms := make(chan os.Signal, 1)
	signal.Notify(terms, syscall.SIGTERM)
	go func() {
		for range terms {
		}
	}()
	go func() {
		buf := make([]byte, 64)
		for {
			n, err := live.Read(buf)
			for _, b := range buf[:n] {
				if b == interruptRequest {
					_ = syscall.Kill(-group, syscall.SIGTERM)
				}
			}
			if err != nil {
				break
			}
		}
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
		select {} // terminate is killing this group, and then the helper exits.
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

// sweepFor bounds how long the helper keeps killing a group that will not
// empty (a member that cannot be signalled); the owner's own cleanup check
// still reports anything left holding the program's output.
const sweepFor = 2 * time.Second

// sweep kills every process in the helper's group, then ends the helper.
//
// A single group kill reaches only the processes already in the group: a
// descendant being forked while the kill is delivered can be missed (macOS does
// not order the two) and would run on with no owner, holding the output pipe.
// So the helper repeats the kill until the group is empty. To survive its own
// kills it first steps into its owner's group; while it lives its PID cannot be
// reused, so the group ID it keeps killing cannot name anyone else's group.
// When the owner died and took its group with it, the session leader's group
// serves. If neither exists, one kill that ends the helper with everyone else
// is all it can do.
func sweep(group, owner int) {
	if !stepOut(group, owner) {
		_ = syscall.Kill(-group, syscall.SIGKILL)
		select {} // The kill above ends the helper too.
	}
	for deadline := time.Now().Add(sweepFor); time.Now().Before(deadline); time.Sleep(time.Millisecond) {
		if syscall.Kill(-group, syscall.SIGKILL) == syscall.ESRCH {
			break
		}
	}
	os.Exit(125)
}

// stepOut moves the helper into another live group of its session.
func stepOut(group, owner int) bool {
	session, _ := unix.Getsid(0) // syscall has Getsid on darwin only
	for _, target := range []int{owner, session} {
		if target > 0 && target != group && syscall.Setpgid(0, target) == nil {
			return true
		}
	}
	return false
}
