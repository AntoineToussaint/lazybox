package sessionhost

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"golang.org/x/sys/unix"
	"golang.org/x/term"
)

// readLaunch reads the host's launch: one JSON line on stdin from Launch, or,
// in a tmux window, the launch file named after the helper argument. The file
// is in the host's own 0700 directory, is read once and removed at once, and
// carries the Runner's environment: tmux starts a window with its server's.
func readLaunch() (launch, bool) {
	var cfg launch
	if len(os.Args) < 3 {
		if json.NewDecoder(io.LimitReader(os.Stdin, 1<<16)).Decode(&cfg) != nil {
			return cfg, false
		}
		_ = os.Stdin.Close()
		return cfg, true
	}
	path := os.Args[2]
	data, err := os.ReadFile(path)
	_ = os.Remove(path)
	if err != nil || len(data) > launchFileLimit || json.Unmarshal(data, &cfg) != nil || cfg.Tmux == nil || filepath.Dir(path) != filepath.Clean(cfg.Dir) {
		return cfg, false
	}
	// tmux names its server in $TMUX ("socket,pid,session") for programs in
	// its windows; the record keeps the socket so a person can attach.
	if socket, _, ok := strings.Cut(os.Getenv("TMUX"), ","); ok && socket != "" {
		cfg.Tmux.Socket = socket
	}
	os.Clearenv()
	for _, kv := range cfg.Env {
		if k, v, ok := strings.Cut(kv, "="); ok && k != "" {
			_ = os.Setenv(k, v)
		}
	}
	cfg.Env = nil
	// The window is for the program; the host's own diagnostics (and a race
	// detector's reports) go to host.log, as a detached host's stderr does.
	if f, err := os.OpenFile(filepath.Join(cfg.Dir, "host.log"), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0600); err == nil {
		_ = unix.Dup2(int(f.Fd()), 2)
		_ = f.Close()
	}
	return cfg, true
}

// launchFileLimit bounds the launch file; the environment is most of it.
const launchFileLimit = 4 << 20

// console is a tmux-hosted host's window: what a person sees after
// `tmux attach`. It is a view, never part of the delivery contract: writes to
// it are queued and dropped when the window does not keep up, so a slow or
// stopped tmux never holds the program or the Runner up.
//
//   - An agent's window shows each JSONL line, from the agent (<) and to it
//     (>), cut to one screen line's worth.
//   - A command's window shows its output as it arrives.
//   - A terminal's window is the shell itself: its output, and keys typed in
//     the window go to the shell. The window follows the shell's size, which
//     the app's panel sets.
//
// Closing the window (kill-window, kill-session, kill-server) hangs up its
// terminal: the host stops its program, as Stop does, and the next Runner
// collects the exit as usual.
type console struct {
	h       *host
	window  *TmuxWindow
	title   string
	queue   chan []byte
	dropped atomic.Bool
	restore func()

	sizeMu   sync.Mutex
	size     *Size
	resizing chan struct{}
}

// consoleQueue bounds the writes waiting for the window.
const consoleQueue = 1024

// consoleLine bounds one agent line shown in the window.
const consoleLine = 400

func openConsole(h *host, cfg launch) *console {
	c := &console{h: h, window: cfg.Tmux, queue: make(chan []byte, consoleQueue), resizing: make(chan struct{}, 1)}
	c.title = cfg.Label
	if c.title == "" {
		c.title = cfg.Kind
	}
	// Raw: keys reach the shell untouched, nothing typed echoes into an
	// agent's view, and Ctrl-C is a byte, not a signal to the host.
	if fd := int(os.Stdin.Fd()); term.IsTerminal(fd) {
		if state, err := term.MakeRaw(fd); err == nil {
			c.restore = func() { _ = term.Restore(fd, state) }
		}
	}
	return c
}

// start shows the header and begins reading the window's keys.
func (c *console) start() {
	go c.write()
	go c.follow()
	go c.read()
	if c.h.cfg.Kind != "terminal" {
		c.text("\x1b[1mlazybox\x1b[0m · " + c.title + "\n")
		c.text("\x1b[2mThis window shows the process live. Detach with your tmux prefix, then d. Closing this window stops the process.\x1b[0m\n")
	}
}

func (c *console) write() {
	for data := range c.queue {
		if c.dropped.Swap(false) {
			_, _ = os.Stdout.Write([]byte("\r\n\x1b[2m[lazybox: output skipped while this window was behind]\x1b[0m\r\n"))
		}
		_, _ = os.Stdout.Write(data)
	}
}

func (c *console) enqueue(data []byte) {
	select {
	case c.queue <- data:
	default:
		c.dropped.Store(true)
	}
}

// text shows s with line ends a raw terminal understands.
func (c *console) text(s string) {
	c.enqueue([]byte(strings.ReplaceAll(s, "\n", "\r\n")))
}

func (c *console) status(s string) {
	if c == nil || c.h.cfg.Kind == "terminal" {
		return
	}
	c.text("\x1b[2m" + time.Now().Format("15:04:05") + " · " + s + "\x1b[0m\n")
}

func (c *console) started(spec *Spec, pid int) {
	if c == nil {
		return
	}
	c.status(fmt.Sprintf("started %s (pid %d)", filepath.Base(spec.Path), pid))
}

func (c *console) exited(o Outcome) {
	if c == nil {
		return
	}
	what := "exited with code " + strconv.Itoa(o.ExitCode)
	if o.Signal != "" {
		what = "ended by " + o.Signal
	}
	if c.h.cfg.Kind == "terminal" {
		c.text("\n\x1b[2m[lazybox: shell " + what + "]\x1b[0m\n")
		return
	}
	c.status("process " + what)
}

// line shows one JSONL line, to (">") or from ("<") the agent.
func (c *console) line(direction string, line []byte) {
	if c == nil {
		return
	}
	shown := line
	if len(shown) > consoleLine {
		shown = shown[:consoleLine]
	}
	text := strings.Map(func(r rune) rune {
		if r < 0x20 || r == 0x7f {
			return ' '
		}
		return r
	}, string(bytes.ToValidUTF8(shown, []byte("?"))))
	if len(line) > consoleLine {
		text += fmt.Sprintf(" … (%d bytes)", len(line))
	}
	color := "36" // from the agent
	if direction == ">" {
		color = "35" // to the agent
	}
	c.enqueue([]byte("\x1b[" + color + "m" + direction + "\x1b[0m " + text + "\r\n"))
}

// chunk shows a command's output.
func (c *console) chunk(channel string, data []byte) {
	if c == nil {
		return
	}
	shown := bytes.ReplaceAll(data, []byte("\n"), []byte("\r\n"))
	if channel == "stderr" {
		shown = append(append([]byte("\x1b[31m"), shown...), "\x1b[0m"...)
	}
	c.enqueue(shown)
}

// raw shows a terminal's output exactly.
func (c *console) raw(data []byte) {
	if c == nil {
		return
	}
	c.enqueue(append([]byte(nil), data...))
}

// read forwards keys typed in a terminal's window to its shell, and stops the
// program once the window is gone.
func (c *console) read() {
	buf := make([]byte, 4096)
	for {
		n, err := os.Stdin.Read(buf)
		if n > 0 {
			c.h.mu.Lock()
			terminal := c.h.terminal != nil
			c.h.mu.Unlock()
			if terminal {
				c.h.writeInput(append([]byte(nil), buf[:n]...))
			}
		}
		if err != nil {
			// The window was closed: stop the program, as closing a terminal
			// window does. A Runner still collects its output and exit.
			c.h.terminate()
			return
		}
	}
}

// resize makes the window the shell's size, so an attached tmux client draws
// the same screen as the app's panel. tmux runs in the background, latest
// size first; the shell never waits for it.
func (c *console) resize(size Size) {
	if c == nil {
		return
	}
	c.sizeMu.Lock()
	c.size = &size
	c.sizeMu.Unlock()
	select {
	case c.resizing <- struct{}{}:
	default:
	}
}

func (c *console) follow() {
	for range c.resizing {
		c.sizeMu.Lock()
		size := c.size
		c.size = nil
		c.sizeMu.Unlock()
		if size == nil {
			continue
		}
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		cmd := exec.CommandContext(ctx, c.window.Path, append(c.window.server(), "resize-window", "-t", c.window.Session, "-x", strconv.Itoa(int(size.Cols)), "-y", strconv.Itoa(int(size.Rows)))...)
		cmd.Env = tmuxEnv()
		_ = cmd.Run()
		cancel()
	}
}

// server is the tmux arguments that select this window's private server.
func (w *TmuxWindow) server() []string {
	if w.Socket != "" {
		return []string{"-S", w.Socket}
	}
	return []string{"-L", w.Server}
}

// tmuxEnv is the environment of a tmux client this process runs: its own,
// without what would point tmux at another server or leak the Runner's
// credentials into a server a person can open new windows in.
func tmuxEnv() []string {
	var env []string
	for _, kv := range os.Environ() {
		k, _, _ := strings.Cut(kv, "=")
		if k == "TMUX" || k == "TMUX_PANE" || strings.HasPrefix(k, "LAZYBOX_V2_") {
			continue
		}
		env = append(env, kv)
	}
	return env
}
