package sessionhost

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// Tmux is the optional backend that starts each host in a window of a private
// tmux server: one server per Runner state directory, on its own socket
// (`tmux -L lazybox-<hash>`), one session per host, named after the host.
//
// The host in the window is the same session host: it owns the program's
// pipes or pseudo-terminal, sequences and buffers its output, and speaks the
// same wire, so the delivery contract (acknowledged cursors, redelivery,
// backpressure, the gateway proxy, scrollback) is unchanged. tmux adds a place
// a person can watch it from any terminal; see console for what the window
// shows. What tmux changes:
//
//   - The host is a child of the tmux server, not a process of its own
//     session: `tmux kill-server` on that socket, or the server crashing,
//     stops every process it hosts.
//   - Closing a host's window stops its program.
//   - The window's own scrollback is tmux's history-limit; the app's
//     scrollback and journals are the host's, as before.
type Tmux struct {
	records
	// Path is the tmux executable; "tmux" on PATH when empty.
	Path string
	// Config is the file the private server loads when it starts; none when
	// empty, so a person's tmux.conf cannot change how hosts behave.
	Config string
}

func (Tmux) Name() string { return BackendTmux }

// MinTmux is the oldest tmux this backend uses: it runs a window's program from
// argv, without a shell.
const MinTmux = "3.0"

var tmuxVersion = regexp.MustCompile(`^tmux (?:next-)?(\d+)\.(\d+)`)

// Executable resolves the tmux binary.
func (t Tmux) Executable() (string, error) {
	name := t.Path
	if name == "" {
		name = "tmux"
	}
	path, err := exec.LookPath(name)
	if err != nil {
		if t.Path == "" {
			return "", errors.New("tmux is not installed (not found on the Runner's PATH)")
		}
		return "", fmt.Errorf("tmux not found at %s", t.Path)
	}
	return path, nil
}

// Version reports the tmux version, or why this backend cannot use tmux.
func (t Tmux) Version(ctx context.Context) (string, error) {
	path, err := t.Executable()
	if err != nil {
		return "", err
	}
	ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	out, err := exec.CommandContext(ctx, path, "-V").Output()
	if err != nil {
		return "", fmt.Errorf("tmux -V failed: %w", err)
	}
	version := strings.TrimSpace(string(out))
	m := tmuxVersion.FindStringSubmatch(version)
	if m == nil {
		return version, nil // A build without a number (master): assume current.
	}
	major, _ := strconv.Atoi(m[1])
	if major < 3 {
		return version, fmt.Errorf("%s is too old; lazybox needs tmux %s or newer", version, MinTmux)
	}
	return version, nil
}

// ServerName is the -L name of the private server for the hosts under root:
// one per Runner state directory (the parent of its host roots).
func ServerName(root string) string {
	sum := sha256.Sum256([]byte(filepath.Dir(filepath.Clean(root))))
	return "lazybox-" + hex.EncodeToString(sum[:6])
}

// tmuxWindowName bounds the label tmux shows for a host's window.
func tmuxWindowName(opts Options) string {
	name := opts.Label
	if name == "" {
		name = opts.Kind
	}
	if name == "" {
		name = "lazybox"
	}
	name = strings.Map(func(r rune) rune {
		if r < 0x20 || r == 0x7f {
			return ' '
		}
		return r
	}, name)
	if r := []rune(name); len(r) > 48 {
		name = string(r[:47]) + "…"
	}
	return name
}

// Launch starts a host in a new session of the private server, starting the
// server if it is not running, and attaches to it.
func (t Tmux) Launch(ctx context.Context, root string, opts Options) (*Client, error) {
	if !filepath.IsAbs(root) {
		return nil, errors.New("session host root must be absolute")
	}
	path, err := t.Executable()
	if err != nil {
		return nil, err
	}
	if err = os.MkdirAll(root, 0700); err != nil {
		return nil, err
	}
	id := rand.Text()[:20]
	dir := filepath.Join(root, id)
	if err = os.Mkdir(dir, 0700); err != nil {
		return nil, err
	}
	defer launching(dir)()
	window := &TmuxWindow{Path: path, Server: ServerName(root), Session: id}
	started := false
	defer func() {
		if !started {
			t.kill(window)
			_ = os.RemoveAll(dir)
		}
	}()
	binary, err := os.Executable()
	if err != nil {
		return nil, err
	}
	token := rand.Text() + rand.Text()
	cfg := newLaunch(id, dir, token, opts)
	cfg.Tmux, cfg.Env = window, os.Environ()
	data, err := json.Marshal(cfg)
	if err != nil {
		return nil, err
	}
	file := filepath.Join(dir, "launch.json")
	if err = os.WriteFile(file, data, 0600); err != nil {
		return nil, err
	}
	config := t.Config
	if config == "" {
		config = os.DevNull
	}
	// One client call: start the server if needed, pin the options a person's
	// configuration could otherwise turn against the hosts (a server that
	// exits with no client attached would take every host with it), then the
	// session. The program runs from argv: no shell, no quoting.
	args := []string{"-L", window.Server, "-f", config,
		"start-server", ";",
		"set-option", "-s", "exit-unattached", "off", ";",
		"set-option", "-s", "exit-empty", "on", ";",
		"set-option", "-g", "destroy-unattached", "off", ";",
		"set-option", "-g", "remain-on-exit", "off", ";",
		"new-session", "-d", "-s", id, "-n", tmuxWindowName(opts), "-c", dir, "--", binary, helperArgument, file}
	cmd := exec.CommandContext(ctx, path, args...)
	cmd.Env = tmuxEnv()
	cmd.Dir = dir
	if out, err := cmd.CombinedOutput(); err != nil {
		return nil, fmt.Errorf("tmux could not start the session host: %w: %s", err, bytes.TrimSpace(out))
	}
	record, err := awaitRecord(ctx, dir, id, token)
	if err != nil {
		return nil, fmt.Errorf("session host startup failed: %w", errors.Join(err, readLog(dir)))
	}
	c, err := dial(ctx, dir, record, 0)
	if err != nil {
		_ = syscall.Kill(record.PID, syscall.SIGKILL)
		return nil, err
	}
	started = true
	return c, nil
}

// awaitRecord waits for the host to write its record: it does so once it
// listens. The token proves the record is this launch's.
func awaitRecord(ctx context.Context, dir, id, token string) (Record, error) {
	deadline := time.Now().Add(15 * time.Second)
	for {
		if r, err := ReadRecord(dir); err == nil {
			if r.ID != id || r.Token != token {
				return Record{}, errors.New("session host record does not match its launch")
			}
			return r, nil
		}
		if time.Now().After(deadline) {
			return Record{}, errors.New("session host did not become ready")
		}
		select {
		case <-ctx.Done():
			return Record{}, ctx.Err()
		case <-time.After(20 * time.Millisecond):
		}
	}
}

// kill ends a host's session; its host, if any, then stops its program.
func (t Tmux) kill(w *TmuxWindow) {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, w.Path, append(w.server(), "kill-session", "-t", w.Session)...)
	cmd.Env = tmuxEnv()
	_ = cmd.Run()
}
