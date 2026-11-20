package sessionhost

import (
	"bufio"
	"context"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

// Message is one agent line delivered to the client.
type Message struct {
	Seq         uint64
	Line        json.RawMessage
	Redelivered bool
}

// Exit reports the agent's end, after every line it wrote has been delivered.
type Exit struct {
	Outcome Outcome
	Stderr  string
}

// Client is a Runner's connection to one host. It is not safe to share between
// owners: the newest attached client supersedes any earlier one.
type Client struct {
	id, dir string
	token   string
	// protocol is the host's wire version, which this client speaks to it.
	protocol int
	conn     net.Conn
	welcome  frame
	writeMu  sync.Mutex
	enc      *json.Encoder
	frames   chan frame
	// early holds output that arrived before Spawn's reply: a fast program can
	// write before the host has answered. Next returns it first.
	early   []frame
	readErr error
	done    chan struct{}
	once    sync.Once
	// held ends this client's count in holders, once, when its connection ends.
	held sync.Once
}

// holders counts this process's open client connections per host directory.
// The Runner holding the state lock is the only legitimate client of its
// hosts, so whether a client in this process holds a host is what decides that
// the host is claimed. The host's own view does not: it learns that a
// predecessor's connection closed only once it reads the close, which can lag
// the kernel by longer than a Runner restart takes.
var holders = struct {
	sync.Mutex
	n map[string]int
}{n: map[string]int{}}

func hold(dir string) {
	holders.Lock()
	holders.n[filepath.Clean(dir)]++
	holders.Unlock()
}

func (c *Client) unhold() {
	c.held.Do(func() { unholdDir(c.dir) })
}

// Held reports whether a client in this process is attached to the host: one
// launched or attached here whose connection was neither detached nor ended by
// the host (superseded, released or gone).
func Held(root, id string) bool {
	holders.Lock()
	defer holders.Unlock()
	return holders.n[filepath.Clean(filepath.Join(root, id))] > 0
}

// Options tune a new host. Zero values select the defaults.
type Options struct {
	// BufferLimit bounds unacknowledged output; DefaultBufferLimit if zero.
	BufferLimit int64
	// OrphanAfter is how long a host keeps its program running with no Runner
	// attached; DefaultOrphanAfter if zero.
	OrphanAfter time.Duration
	// Scrollback bounds the output a terminal host keeps for a re-attaching
	// Runner; DefaultScrollback if zero.
	Scrollback int
	// Kind, Owner and Label are recorded for an inventory (see Record).
	Kind, Owner, Label string
}

// launching marks a host this process is starting as held, from before the
// host exists until its client holds it: a sweep that runs in between must not
// take a host on its way up for an orphan.
func launching(dir string) func() {
	hold(dir)
	var once sync.Once
	return func() { once.Do(func() { unholdDir(dir) }) }
}

func unholdDir(dir string) {
	dir = filepath.Clean(dir)
	holders.Lock()
	if holders.n[dir]--; holders.n[dir] <= 0 {
		delete(holders.n, dir)
	}
	holders.Unlock()
}

// Launch starts a new detached host under root and attaches to it. The host
// has no agent until Spawn.
func Launch(ctx context.Context, root string, opts Options) (*Client, error) {
	if !filepath.IsAbs(root) {
		return nil, errors.New("session host root must be absolute")
	}
	if err := os.MkdirAll(root, 0700); err != nil {
		return nil, err
	}
	id := rand.Text()[:20]
	dir := filepath.Join(root, id)
	if err := os.Mkdir(dir, 0700); err != nil {
		return nil, err
	}
	defer launching(dir)()
	// Until the host runs, nothing else will remove its directory.
	started := false
	defer func() {
		if !started {
			_ = os.RemoveAll(dir)
		}
	}()
	token := rand.Text() + rand.Text()
	label := opts.Label
	if label == "" {
		label = opts.Kind
	}
	description := strings.TrimSpace("Lazybox session host " + label)
	record, err := startHost(ctx, dir, newLaunch(id, dir, token, opts), description, true)
	if errors.Is(err, errScoped) {
		// systemd-run passed the check but not this scope: the host is worth
		// more than its scope, so it starts as it would without systemd, and
		// the next hosts do too until the check is made again.
		scopeFailed(err.Error())
		record, err = startHost(ctx, dir, newLaunch(id, dir, token, opts), description, false)
	}
	if err != nil {
		return nil, err
	}
	c, err := dial(ctx, dir, record, 0)
	if err != nil {
		_ = syscall.Kill(record.PID, syscall.SIGKILL)
		return nil, err
	}
	started = true
	return c, nil
}

// errScoped marks a host that failed to start in its own systemd scope.
var errScoped = errors.New("in its systemd scope")

// startHost starts the host process for cfg and waits for its record. With
// scope, it starts in a transient systemd scope of its own where the system
// offers one (scope_linux.go); a failure then wraps errScoped.
func startHost(ctx context.Context, dir string, cfg launch, description string, scope bool) (Record, error) {
	binary, err := os.Executable()
	if err != nil {
		return Record{}, err
	}
	logFile, err := os.OpenFile(filepath.Join(dir, "host.log"), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0600)
	if err != nil {
		return Record{}, err
	}
	defer logFile.Close()
	cmd := exec.Command(binary, helperArgument)
	cmd.Dir = dir
	cmd.Env = os.Environ()
	cmd.Stderr = logFile
	// Its own session: no Runner signal, process-group kill or terminal hangup reaches it.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	// And on Linux its own cgroup: no service manager's stop of the Runner reaches it.
	scoped := scope && inOwnScope(cmd, ScopeUnit(cfg.ID), description)
	failed := func(err error) error {
		if scoped {
			return fmt.Errorf("session host startup failed %w: %w", errScoped, err)
		}
		return fmt.Errorf("session host startup failed: %w", err)
	}
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return Record{}, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return Record{}, err
	}
	if err = cmd.Start(); err != nil {
		return Record{}, failed(err)
	}
	// The Runner never waits on its hosts; reap only if this one fails to start.
	cleanup := func() { _ = cmd.Process.Kill(); _ = cmd.Wait() }
	if err = json.NewEncoder(stdin).Encode(cfg); err != nil {
		cleanup()
		return Record{}, failed(err)
	}
	_ = stdin.Close()
	ready := make(chan error, 1)
	var record Record
	go func() { ready <- json.NewDecoder(io.LimitReader(stdout, 1<<16)).Decode(&record) }()
	select {
	case err = <-ready:
	case <-ctx.Done():
		// Not the scope's fault: no second attempt.
		cleanup()
		return Record{}, fmt.Errorf("session host startup failed: %w", ctx.Err())
	case <-time.After(15 * time.Second):
		err = errors.New("session host did not become ready")
	}
	if err != nil || record.ID != cfg.ID || record.Token != cfg.Token {
		cleanup()
		return Record{}, failed(errors.Join(err, readLog(dir)))
	}
	// Release the child: the host outlives this process and must not stay a zombie
	// of a long-lived Runner. Reaping happens in the background.
	go func() { _ = cmd.Wait() }()
	return record, nil
}

func readLog(dir string) error {
	data, _ := os.ReadFile(filepath.Join(dir, "host.log"))
	if len(data) == 0 {
		return nil
	}
	if len(data) > 4096 {
		data = data[len(data)-4096:]
	}
	return errors.New(string(data))
}

// Attach connects to an existing host. Lines up to ack are treated as recorded;
// everything after is replayed, preceded by unanswered requests.
func Attach(ctx context.Context, root, id string, ack uint64) (*Client, error) {
	if !validID.MatchString(id) {
		return nil, errors.New("invalid session host id")
	}
	dir := filepath.Join(root, id)
	record, err := ReadRecord(dir)
	if err != nil {
		return nil, fmt.Errorf("session host %s unavailable: %w", id, err)
	}
	if !Speaks(record.Protocol) {
		return nil, ErrIncompatible
	}
	return dial(ctx, dir, record, ack)
}

func dial(ctx context.Context, dir string, record Record, ack uint64) (*Client, error) {
	d := net.Dialer{Timeout: 5 * time.Second}
	raw, err := d.DialContext(ctx, "tcp", "127.0.0.1:"+strconv.Itoa(record.Port))
	if err != nil {
		return nil, fmt.Errorf("session host %s unreachable: %w", record.ID, err)
	}
	c := &Client{id: record.ID, dir: dir, token: record.Token, conn: raw, enc: json.NewEncoder(raw), frames: make(chan frame, 64), done: make(chan struct{})}
	// Speak the host's version, not the newest: a host knows only its own.
	c.protocol = record.Protocol
	if err = c.write(frame{Op: "hello", Token: record.Token, Ack: ack, Protocol: record.Protocol}); err != nil {
		raw.Close()
		return nil, err
	}
	dec := json.NewDecoder(bufio.NewReaderSize(raw, 64<<10))
	_ = raw.SetReadDeadline(time.Now().Add(10 * time.Second))
	if err = dec.Decode(&c.welcome); err != nil {
		raw.Close()
		return nil, fmt.Errorf("session host %s refused the connection: %w", record.ID, err)
	}
	_ = raw.SetReadDeadline(time.Time{})
	if c.welcome.Op == "error" {
		raw.Close()
		if c.welcome.Protocol != record.Protocol {
			return nil, ErrIncompatible
		}
		return nil, errors.New(c.welcome.Text)
	}
	hold(dir)
	go c.read(dec)
	return c, nil
}

func (c *Client) read(dec *json.Decoder) {
	defer close(c.frames)
	// The host closes a superseded or released client's connection.
	defer c.unhold()
	for {
		var f frame
		if err := dec.Decode(&f); err != nil {
			c.readErr = err
			return
		}
		select {
		case c.frames <- f:
		case <-c.done:
			return
		}
	}
}

func (c *Client) write(f frame) error {
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	_ = c.conn.SetWriteDeadline(time.Now().Add(30 * time.Second))
	return c.enc.Encode(f)
}

// Protocol is the wire version this host speaks.
func (c *Client) Protocol() int { return c.protocol }

// ID is the host identity used to re-attach.
func (c *Client) ID() string { return c.id }

// ProxyURL is the session-stable loopback base URL for provider traffic.
func (c *Client) ProxyURL() string { return c.welcome.Proxy }

// Spawned reports whether the host already runs its agent.
func (c *Client) Spawned() bool { return c.welcome.Spawned }

// AgentPID is the agent's process ID as observed by the host (zero before spawn).
func (c *Client) AgentPID() int { return c.welcome.AgentPID }

// Spawn starts the agent. It must be the first operation on a new host.
func (c *Client) Spawn(ctx context.Context, spec Spec) error {
	if err := c.write(frame{Op: "spawn", Spec: &spec}); err != nil {
		return err
	}
	for {
		select {
		case f, ok := <-c.frames:
			if !ok {
				return fmt.Errorf("session host closed during spawn: %w", c.readErr)
			}
			switch f.Op {
			case "error":
				return errors.New(f.Text)
			case "spawned":
				c.welcome.Spawned, c.welcome.AgentPID = true, f.AgentPID
				return nil
			case "msg", "exit":
				c.early = append(c.early, f)
			default:
				return fmt.Errorf("unexpected session host reply %q", f.Op)
			}
		case <-ctx.Done():
			return ctx.Err()
		}
	}
}

// Send writes one line to the agent's stdin.
func (c *Client) Send(line json.RawMessage) error { return c.write(frame{Op: "send", Line: line}) }

// Input writes raw bytes to a terminal host's shell, at most InputLimit.
func (c *Client) Input(data []byte) error {
	if len(data) > InputLimit {
		return errors.New("terminal input exceeds 64 KiB")
	}
	return c.write(frame{Op: "input", Data: data})
}

// Resize changes a terminal host's size.
func (c *Client) Resize(size Size) error { return c.write(frame{Op: "resize", Size: &size}) }

// Scrollback is a terminal host's retained output when this client attached,
// and the total output it had produced then: live chunks continue from there.
func (c *Client) Scrollback() ([]byte, uint64) { return c.welcome.Scrollback, c.welcome.Produced }

// Ack records that every line up to seq is durably handled by this client.
func (c *Client) Ack(seq uint64) error { return c.write(frame{Op: "ack", Seq: seq}) }

// Resolve tells the host the agent no longer waits on its request id (the raw
// JSON-RPC id of the request line) although no reply was written: the agent
// resolved it itself, or the turn it belonged to ended. The host then never
// redelivers it. New hosts acknowledge the mutation before this returns;
// older hosts retain their asynchronous resolve behavior.
func (c *Client) Resolve(id json.RawMessage) error {
	if c.welcome.Owner == "" {
		return c.write(frame{Op: "resolve", Line: id}) // older hosts have no acknowledgement
	}
	raw, err := net.DialTimeout("tcp", c.conn.RemoteAddr().String(), 5*time.Second)
	if err != nil {
		return err
	}
	defer raw.Close()
	_ = raw.SetDeadline(time.Now().Add(5 * time.Second))
	if err := json.NewEncoder(raw).Encode(frame{Op: "resolve", Token: c.token, Owner: c.welcome.Owner, Line: id}); err != nil {
		return err
	}
	var reply frame
	if err := json.NewDecoder(raw).Decode(&reply); err != nil {
		return err
	}
	if reply.Op != "resolved" {
		if reply.Text == ErrSuperseded.Error() {
			return ErrSuperseded
		}
		return fmt.Errorf("session host did not confirm request resolution: %s", reply.Text)
	}
	return nil
}

// SetUpstream points the host's provider proxy at this Runner's gateway.
func (c *Client) SetUpstream(url string) error { return c.write(frame{Op: "upstream", URL: url}) }

// Terminate stops the agent; Next then reports its Exit.
func (c *Client) Terminate() error { return c.write(frame{Op: "terminate"}) }

// Release lets a host whose agent has exited go away.
func (c *Client) Release() error { return c.write(frame{Op: "release"}) }

// Next returns the next agent line or the agent's exit. ErrSuperseded means
// another Runner attached; io.EOF means the connection ended otherwise.
func (c *Client) Next(ctx context.Context) (*Message, *Exit, error) {
	for {
		f, err := c.receive(ctx)
		if err != nil {
			return nil, nil, err
		}
		switch f.Op {
		case "msg":
			return &Message{Seq: f.Seq, Line: f.Line, Redelivered: f.Redelivered}, nil, nil
		case "superseded":
			return nil, nil, ErrSuperseded
		case "exit":
			exit := &Exit{Stderr: f.Text}
			if f.Outcome != nil {
				exit.Outcome = *f.Outcome
			}
			return nil, exit, nil
		}
	}
}

// receive returns output held back during Spawn, then the next frame.
func (c *Client) receive(ctx context.Context) (frame, error) {
	if len(c.early) > 0 {
		f := c.early[0]
		c.early = c.early[1:]
		return f, nil
	}
	select {
	case f, ok := <-c.frames:
		if !ok {
			if c.readErr != nil && !errors.Is(c.readErr, io.EOF) {
				return frame{}, c.readErr
			}
			return frame{}, io.EOF
		}
		return f, nil
	case <-ctx.Done():
		return frame{}, ctx.Err()
	}
}

// Detach closes this connection and leaves the host and its agent running.
func (c *Client) Detach() error {
	c.unhold()
	c.once.Do(func() { close(c.done) })
	return c.conn.Close()
}

// Hosts lists host IDs with a discovery record under root.
func Hosts(root string) ([]string, error) {
	entries, err := os.ReadDir(root)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var ids []string
	for _, e := range entries {
		if !e.IsDir() || !validID.MatchString(e.Name()) {
			continue
		}
		if _, err := os.Stat(recordPath(filepath.Join(root, e.Name()))); err == nil {
			ids = append(ids, e.Name())
		}
	}
	return ids, nil
}

// Stop terminates a host's agent and waits for its exit, then releases the host.
// It is version-stable: it also stops a host whose protocol this Runner cannot adopt.
func Stop(ctx context.Context, root, id string) error {
	if !validID.MatchString(id) {
		return errors.New("invalid session host id")
	}
	dir := filepath.Join(root, id)
	record, err := ReadRecord(dir)
	if err != nil {
		return err
	}
	d := net.Dialer{Timeout: 5 * time.Second}
	raw, err := d.DialContext(ctx, "tcp", "127.0.0.1:"+strconv.Itoa(record.Port))
	if err != nil {
		return err
	}
	defer raw.Close()
	enc := json.NewEncoder(raw)
	for _, f := range []frame{{Op: "hello", Token: record.Token, Ack: ^uint64(0), Protocol: record.Protocol}, {Op: "terminate"}} {
		if err = enc.Encode(f); err != nil {
			return err
		}
	}
	dec := json.NewDecoder(raw)
	deadline, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	go func() { <-deadline.Done(); _ = raw.SetReadDeadline(time.Now()) }()
	for {
		var f frame
		if err = dec.Decode(&f); err != nil {
			// The host exits once a terminated agent is gone; EOF confirms it.
			if errors.Is(err, io.EOF) {
				return nil
			}
			return fmt.Errorf("session host %s stop unconfirmed: %w", id, err)
		}
		if f.Op == "exit" || (f.Op == "welcome" && f.Exited) {
			_ = enc.Encode(frame{Op: "release"})
		}
		if f.Op == "welcome" && !f.Spawned {
			_ = enc.Encode(frame{Op: "release"})
		}
	}
}

// Status is a host's state as reported without attaching to it.
type Status struct {
	Protocol int
	Attached bool
	Spawned  bool
	Exited   bool
	AgentPID int
}

// Probe inspects a host without superseding the client attached to it.
func Probe(ctx context.Context, root, id string) (Status, error) {
	if !validID.MatchString(id) {
		return Status{}, errors.New("invalid session host id")
	}
	record, err := ReadRecord(filepath.Join(root, id))
	if err != nil {
		return Status{}, err
	}
	d := net.Dialer{Timeout: 5 * time.Second}
	raw, err := d.DialContext(ctx, "tcp", "127.0.0.1:"+strconv.Itoa(record.Port))
	if err != nil {
		return Status{}, err
	}
	defer raw.Close()
	_ = raw.SetDeadline(time.Now().Add(5 * time.Second))
	if err = json.NewEncoder(raw).Encode(frame{Op: "probe", Token: record.Token}); err != nil {
		return Status{}, err
	}
	var f frame
	if err = json.NewDecoder(raw).Decode(&f); err != nil {
		return Status{}, err
	}
	return Status{Protocol: f.Protocol, Attached: f.Attached, Spawned: f.Spawned, Exited: f.Exited, AgentPID: f.AgentPID}, nil
}

// Forget removes the directory of a host that is gone: its process no longer
// answers, so nothing it owned survives it (the agent's guard dies with it).
func Forget(root, id string) error {
	if !validID.MatchString(id) {
		return errors.New("invalid session host id")
	}
	return os.RemoveAll(filepath.Join(root, id))
}

// Gone reports whether err from Attach, Probe or Stop shows the host no longer
// exists: its record is missing or nothing listens at its address.
func Gone(err error) bool {
	var dial *net.OpError
	return errors.Is(err, os.ErrNotExist) || (errors.As(err, &dial) && dial.Op == "dial" && errors.Is(err, syscall.ECONNREFUSED))
}
