package sessionhost

import (
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"os/signal"
	"syscall"
	"time"

	"github.com/creack/pty"
)

// A terminal host runs one interactive program, a human's shell, on a
// pseudo-terminal. The shell is a session leader with the terminal as its
// controlling terminal, so job control, Ctrl-C and window-size changes work as
// in any terminal emulator. It is deliberately not under processguard: an
// interactive shell puts each job in its own process group, which a group kill
// cannot reach. Its lifetime is bound the way a terminal window binds it: when
// the terminal hangs up (Terminate, or the host exiting and closing the
// terminal), the shell gets SIGHUP and hangs up its jobs.

// inputLimit bounds input queued for a shell that is not reading it.
const inputLimit = 4 << 20

// hangUpGrace is how long a hung-up shell has to exit before it is killed.
const hangUpGrace = 2 * time.Second

func validSize(s *Size) bool {
	return s != nil && s.Cols > 0 && s.Rows > 0 && s.Cols <= 1000 && s.Rows <= 500
}

func (h *host) spawnTerminalLocked(spec *Spec) error {
	if !validSize(spec.Terminal) {
		return errors.New("terminal size must be 1-1000 columns and 1-500 rows")
	}
	if spec.Stream {
		return errors.New("a terminal is not a streamed command")
	}
	// The host ignores SIGHUP, SIGINT and SIGPIPE, and an ignored disposition
	// survives exec: a shell started now would ignore Ctrl-C and hangups, and so
	// would every program it runs. Handling them in Go instead (and discarding
	// them) keeps the host immune while its children start with the defaults.
	discarded := make(chan os.Signal, 8)
	signal.Notify(discarded, syscall.SIGHUP, syscall.SIGINT, syscall.SIGPIPE)
	go func() {
		for range discarded {
		}
	}()
	cmd := exec.Command(spec.Path, spec.Args...)
	if spec.Argv0 != "" {
		cmd.Args[0] = spec.Argv0
	}
	cmd.Env = spec.Env
	cmd.Dir = spec.Cwd
	// A new session with the terminal as its controlling terminal.
	master, err := pty.StartWithSize(cmd, &pty.Winsize{Cols: spec.Terminal.Cols, Rows: spec.Terminal.Rows})
	if err != nil {
		return err
	}
	h.terminal, h.stdin, h.spawned = master, master, true
	h.agent = cmd.Process.Pid
	h.console.started(spec, h.agent)
	h.console.resize(*spec.Terminal)
	go h.feed(master)
	readDone := make(chan struct{})
	go func() {
		defer close(readDone)
		h.readTerminal(master)
	}()
	go func() {
		err := cmd.Wait()
		h.mu.Lock()
		h.reaped, h.gone = true, true
		h.cond.Broadcast()
		h.mu.Unlock()
		outcome := Outcome{Started: true}
		if state := cmd.ProcessState; state != nil {
			outcome.ExitCode = state.ExitCode()
			if status, ok := state.Sys().(syscall.WaitStatus); ok && status.Signaled() {
				outcome.Signal = status.Signal().String()
			}
		}
		var exit *exec.ExitError
		if err != nil && !errors.As(err, &exit) {
			outcome.Error = err.Error()
		}
		// Output the shell wrote just before exiting is still in the terminal.
		// A background job that kept the terminal open would hold the read
		// forever: stop waiting, and let the host's exit hang the terminal up.
		select {
		case <-readDone:
		case <-time.After(time.Second):
		}
		h.mu.Lock()
		h.hungUp = true
		h.readDone = true
		h.exited = true
		h.outcome = outcome
		h.cond.Broadcast()
		h.mu.Unlock()
		h.console.exited(outcome)
	}()
	return nil
}

// readTerminal sequences the terminal's output, one Chunk per read.
func (h *host) readTerminal(master *os.File) {
	buf := make([]byte, ChunkSize)
	for {
		n, err := master.Read(buf)
		if n > 0 {
			data := append([]byte(nil), buf[:n]...)
			line, _ := json.Marshal(Chunk{Channel: TerminalChannel, Data: data})
			h.console.raw(data)
			h.recordOutput(line, data)
		}
		if err != nil {
			// EIO or EOF: every process closed the terminal.
			return
		}
	}
}

// keepLocked appends output to the scrollback. The buffer is compacted only
// once it holds twice its limit, so trimming costs O(1) amortised per byte.
func (h *host) keepLocked(data []byte) {
	h.produced += uint64(len(data))
	h.scrollback = append(h.scrollback, data...)
	if limit := h.cfg.Scrollback; len(h.scrollback) > 2*limit {
		h.scrollback = append([]byte(nil), h.scrollback[len(h.scrollback)-limit:]...)
	}
}

// scrollbackLocked returns a copy of the retained output, at most the limit.
func (h *host) scrollbackLocked() []byte {
	data := h.scrollback
	if len(data) > h.cfg.Scrollback {
		data = data[len(data)-h.cfg.Scrollback:]
	}
	return append([]byte(nil), data...)
}

// writeInput queues raw input for the terminal, in order. Input beyond the
// bound, to a shell that stopped reading, is dropped as a full terminal would.
func (h *host) writeInput(data []byte) {
	if len(data) == 0 || len(data) > InputLimit {
		return
	}
	h.mu.Lock()
	queued := 0
	for _, chunk := range h.input {
		queued += len(chunk)
	}
	if h.terminal != nil && !h.exited && queued+len(data) <= inputLimit {
		h.input = append(h.input, append([]byte(nil), data...))
	}
	h.mu.Unlock()
	select {
	case h.inputReady <- struct{}{}:
	default:
	}
}

// resize changes the terminal's size; the kernel signals SIGWINCH to the
// foreground job.
func (h *host) resize(size *Size) {
	h.mu.Lock()
	terminal, exited := h.terminal, h.exited
	h.mu.Unlock()
	if terminal == nil || exited || !validSize(size) {
		return
	}
	_ = pty.Setsize(terminal, &pty.Winsize{Cols: size.Cols, Rows: size.Rows})
	h.console.resize(*size)
}

// hangUp ends the terminal as closing a terminal window does: SIGHUP to the
// shell's process group (the shell passes it on to its jobs), then SIGKILL if
// the shell is still there after the grace period. The shell is a session
// leader, so its process group ID is its PID; it is signalled only while it
// has not been reaped, so a reused PID is never hit.
func (h *host) hangUp() {
	signalShell := func(sig syscall.Signal) bool {
		h.mu.Lock()
		defer h.mu.Unlock()
		if h.reaped {
			return false
		}
		_ = syscall.Kill(-h.agent, sig)
		return true
	}
	if !signalShell(syscall.SIGHUP) {
		return
	}
	deadline := time.Now().Add(hangUpGrace)
	for time.Now().Before(deadline) {
		h.mu.Lock()
		reaped := h.reaped
		h.mu.Unlock()
		if reaped {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	signalShell(syscall.SIGKILL)
}
