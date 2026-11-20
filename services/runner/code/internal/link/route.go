package link

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"regexp"
	"strings"
	"sync"
	"syscall"
	"time"
)

// Route is one way to reach a Runner's link endpoint (REMOTE_RUNNERS §3.2):
// a byte stream, nothing more. It is never trusted; Noise is.
type Route interface {
	Kind() string
	Dial(ctx context.Context) (io.ReadWriteCloser, error)
	Describe() RouteInfo
}

// RouteInfo is how the window labels a route.
type RouteInfo struct {
	Kind  string `json:"kind"`
	Label string `json:"label"`
	Host  string `json:"host,omitempty"`
}

// diagnoser is a stream that can say why it ended (ssh's stderr).
type diagnoser interface{ Diagnosis() string }

// SSHRoute reaches a Runner through the person's own OpenSSH client and
// ~/.ssh/config (REMOTE_RUNNERS §4.1): `ssh <host> lazybox link serve
// --stdio`, the session on ssh's stdin and stdout. No port is opened on
// either computer.
type SSHRoute struct {
	// Host is the ssh destination: an alias from ~/.ssh/config, or user@host.
	Host string
	// Remote is the lazybox on the host, as the probe found it.
	Remote string
	// Runner is the registered Runner on the host the link serves (its
	// locator name, "lazybox" for an installed one); empty: the host's default.
	Runner string
	// SSH is the client binary (default "ssh"; LAZYBOX_SSH names another, for tests).
	SSH string
	// ControlDir holds ControlMaster sockets, so later commands reuse one
	// connection; empty turns multiplexing off.
	ControlDir string
	// Interactive lets ssh prompt (a passphrase, a new host key) on this
	// terminal; otherwise BatchMode, and a prompt is an error.
	Interactive bool
	// Env adds variables to the ssh process (SSH_ASKPASS, tests).
	Env []string
}

var hostPattern = regexp.MustCompile(`^[A-Za-z0-9_[][A-Za-z0-9_.@%:\[\]+-]{0,252}$`)

// ValidHost reports whether s can be an ssh destination without being read
// as an option or carrying shell syntax.
func ValidHost(s string) bool { return hostPattern.MatchString(s) }

func (r SSHRoute) Kind() string { return "ssh" }
func (r SSHRoute) Describe() RouteInfo {
	return RouteInfo{Kind: "ssh", Label: "SSH", Host: r.Host}
}

func (r SSHRoute) binary() string {
	if r.SSH != "" {
		return r.SSH
	}
	if v := os.Getenv("LAZYBOX_SSH"); v != "" {
		return v
	}
	return "ssh"
}

// Args is the full ssh command line for running remote on the host.
func (r SSHRoute) Args(remote ...string) ([]string, error) {
	if !ValidHost(r.Host) {
		return nil, fmt.Errorf("%q is not an ssh host name", r.Host)
	}
	args := []string{"-T", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3", "-o", "ConnectTimeout=15"}
	if !r.Interactive {
		args = append(args, "-o", "BatchMode=yes")
	}
	// A unix socket path has about 104 bytes on macOS; %C is 40 more. A
	// configuration directory too deep for that connects without multiplexing.
	if r.ControlDir != "" && len(strings.TrimRight(r.ControlDir, "/"))+1+40 < 100 {
		args = append(args, "-o", "ControlMaster=auto", "-o", "ControlPath="+strings.TrimRight(r.ControlDir, "/")+"/%C", "-o", "ControlPersist=10m")
	}
	args = append(args, r.Host)
	if len(remote) > 0 {
		words := make([]string, len(remote))
		for i, w := range remote {
			q, err := shellWord(w)
			if err != nil {
				return nil, err
			}
			words[i] = q
		}
		// One argument: ssh joins its arguments with spaces anyway, and the
		// remote login shell (sh, bash, zsh or fish) reads this line.
		args = append(args, strings.Join(words, " "))
	}
	return args, nil
}

var plainWord = regexp.MustCompile(`^[A-Za-z0-9_@%+=:,./-]+$`)

// shellWord quotes one word for the remote login shell. Single quotes mean
// the same in sh, bash, zsh and fish as long as the word has no quote or
// backslash, which we refuse rather than escape.
func shellWord(w string) (string, error) {
	if plainWord.MatchString(w) {
		return w, nil
	}
	if strings.ContainsAny(w, "'\\\n\r\x00") {
		return "", fmt.Errorf("cannot pass %q to the remote shell", w)
	}
	return "'" + w + "'", nil
}

func (r SSHRoute) command(ctx context.Context, remote ...string) (*exec.Cmd, error) {
	args, err := r.Args(remote...)
	if err != nil {
		return nil, err
	}
	if r.ControlDir != "" {
		if err = os.MkdirAll(r.ControlDir, 0o700); err != nil {
			return nil, err
		}
	}
	cmd := exec.CommandContext(ctx, r.binary(), args...)
	cmd.Env = append(os.Environ(), r.Env...)
	// Its own process group: a ^C in the terminal that ran `lazybox` is ours
	// to handle, not ssh's, and Close ends exactly this ssh.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: !r.Interactive}
	cmd.WaitDelay = 2 * time.Second
	return cmd, nil
}

// Run runs one command on the host and returns its stdout. Its stderr is in
// the error when it fails.
func (r SSHRoute) Run(ctx context.Context, stdin io.Reader, remote ...string) ([]byte, error) {
	cmd, err := r.command(ctx, remote...)
	if err != nil {
		return nil, err
	}
	var stdout, stderr bytes.Buffer
	cmd.Stdin, cmd.Stdout = stdin, &stdout
	if r.Interactive {
		cmd.Stderr = io.MultiWriter(&stderr, os.Stderr)
	} else {
		cmd.Stderr = &stderr
	}
	if err = cmd.Run(); err != nil {
		return stdout.Bytes(), &SSHError{Host: r.Host, Err: err, Stderr: strings.TrimSpace(stderr.String())}
	}
	return stdout.Bytes(), nil
}

// Dial starts `ssh host <lazybox> link serve --stdio` and returns its stdio.
func (r SSHRoute) Dial(ctx context.Context) (io.ReadWriteCloser, error) {
	remote := r.Remote
	if remote == "" {
		remote = "lazybox"
	}
	// Not CommandContext's ctx: the stream outlives the dial; Close ends it.
	words := []string{remote}
	if r.Runner != "" {
		words = append(words, "--runner", r.Runner)
	}
	words = append(words, "link", "serve", "--stdio", "--route", "ssh")
	cmd, err := r.command(context.Background(), words...)
	if err != nil {
		return nil, err
	}
	return startStream(cmd, r.Host)
}

func startStream(cmd *exec.Cmd, host string) (io.ReadWriteCloser, error) {
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		_ = stdin.Close()
		return nil, err
	}
	s := &cmdStream{cmd: cmd, in: stdin, out: stdout, host: host, done: make(chan struct{})}
	cmd.Stderr = &s.stderr
	if err = cmd.Start(); err != nil {
		_ = stdin.Close()
		_ = stdout.Close()
		return nil, fmt.Errorf("could not run %s: %w", cmd.Args[0], err)
	}
	go func() { s.waitErr = cmd.Wait(); close(s.done) }()
	return s, nil
}

// cmdStream is a child process's stdio as one stream.
type cmdStream struct {
	cmd     *exec.Cmd
	in      io.WriteCloser
	out     io.ReadCloser
	host    string
	stderr  limitedBuffer
	once    sync.Once
	done    chan struct{}
	waitErr error
}

func (s *cmdStream) Read(p []byte) (int, error)  { return s.out.Read(p) }
func (s *cmdStream) Write(p []byte) (int, error) { return s.in.Write(p) }
func (s *cmdStream) Close() error {
	s.once.Do(func() {
		_ = s.in.Close()
		select {
		case <-s.done:
		case <-time.After(500 * time.Millisecond):
			if s.cmd.Process != nil {
				_ = s.cmd.Process.Signal(syscall.SIGTERM)
			}
			select {
			case <-s.done:
			case <-time.After(2 * time.Second):
				_ = s.cmd.Process.Kill()
				<-s.done
			}
		}
	})
	return nil
}

// Diagnosis is why the stream ended, from ssh's and the host's stderr.
func (s *cmdStream) Diagnosis() string {
	select {
	case <-s.done:
	case <-time.After(300 * time.Millisecond):
	}
	return strings.TrimSpace(s.stderr.String())
}

// limitedBuffer keeps the last 8 KiB written to it.
type limitedBuffer struct {
	mu  sync.Mutex
	buf []byte
}

func (b *limitedBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.buf = append(b.buf, p...)
	if len(b.buf) > 8192 {
		b.buf = b.buf[len(b.buf)-8192:]
	}
	return len(p), nil
}

func (b *limitedBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return string(b.buf)
}

// SSHError is a failed ssh command, said plainly.
type SSHError struct {
	Host   string
	Err    error
	Stderr string
}

func (e *SSHError) Error() string { return Explain(e.Host, e.Stderr, e.Err) }
func (e *SSHError) Unwrap() error { return e.Err }

// Explain turns what ssh printed into a sentence a person can act on.
func Explain(host, stderr string, err error) string {
	lower := strings.ToLower(stderr)
	switch {
	case strings.Contains(lower, "could not resolve hostname"):
		return fmt.Sprintf("%s: this computer cannot find that host name. Check the name, or its Host entry in ~/.ssh/config.", host)
	case strings.Contains(lower, "connection refused"):
		return fmt.Sprintf("%s refused the SSH connection: is SSH (Remote Login) turned on there, and is the port right?", host)
	case strings.Contains(lower, "operation timed out"), strings.Contains(lower, "connection timed out"):
		return fmt.Sprintf("%s did not answer: it may be asleep, off, or on another network.", host)
	case strings.Contains(lower, "host key verification failed"):
		return fmt.Sprintf("%s's host key is not known yet, or it changed. Connect once with `ssh %s` in a terminal to check and accept it, then try again.", host, host)
	case strings.Contains(lower, "permission denied"):
		return fmt.Sprintf("%s refused your SSH login. Check that `ssh %s` works in a terminal without a password prompt (an SSH key, an agent).", host, host)
	case strings.Contains(lower, "no route to host"), strings.Contains(lower, "network is unreachable"):
		return fmt.Sprintf("%s cannot be reached from this network.", host)
	case strings.Contains(lower, "command not found"), strings.Contains(lower, "no such file or directory"):
		return fmt.Sprintf("%s has no Lazybox at the expected place. Add it again to install it.", host)
	}
	last := lastLine(stderr)
	if last == "" && err != nil {
		last = err.Error()
	}
	return fmt.Sprintf("%s: %s", host, last)
}

func lastLine(s string) string {
	lines := strings.Split(strings.TrimSpace(s), "\n")
	for i := len(lines) - 1; i >= 0; i-- {
		if l := strings.TrimSpace(lines[i]); l != "" {
			return l
		}
	}
	return ""
}

// refusalFrom reads the host's own reason for refusing a handshake from the
// stream's diagnosis (`lazybox-link: refused: <code>: <reason>`).
func refusalFrom(diagnosis string) *Refusal {
	for _, line := range strings.Split(diagnosis, "\n") {
		rest, ok := strings.CutPrefix(strings.TrimSpace(line), "lazybox-link: refused: ")
		if !ok {
			continue
		}
		code, reason, _ := strings.Cut(rest, ": ")
		return &Refusal{Code: code, Reason: reason}
	}
	return nil
}

var errNoSession = errors.New("not connected")
