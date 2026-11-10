package sessionhost

import (
	"bufio"
	"crypto/rand"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/exec"
	"os/signal"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/processguard"
)

// Re-exec the importing binary in host mode before its normal entry point, the
// same way processguard does, so no separately installed executable is needed.
func init() {
	if len(os.Args) >= 2 && os.Args[1] == helperArgument {
		os.Exit(serve())
	}
}

// DefaultBufferLimit bounds unacknowledged agent output held while no Runner is
// attached. Beyond it the host stops reading and the agent blocks.
const DefaultBufferLimit = 64 << 20

// DefaultOrphanAfter bounds how long a host keeps its program running while no
// Runner is attached. A Runner restart takes seconds; a host unattached for a
// day belongs to an environment nobody runs any more, and a program left
// running for no one keeps spending provider quota or holding ports. The
// conversation itself survives: it resumes natively when its Runner returns.
const DefaultOrphanAfter = 24 * time.Hour

type launch struct {
	ID          string        `json:"id"`
	Dir         string        `json:"dir"`
	Token       string        `json:"token"`
	BufferLimit int64         `json:"bufferLimit"`
	OrphanAfter time.Duration `json:"orphanAfter"`
}

type entry struct {
	seq     uint64
	line    json.RawMessage
	id      string
	request bool
}

type host struct {
	mu   sync.Mutex
	cond *sync.Cond
	cfg  launch

	// Agent output not yet acknowledged, in sequence order, and agent requests
	// acknowledged but not yet answered (they are replayed on re-attachment).
	log      []entry
	size     int64
	open     map[string]entry
	answered map[string]bool
	next     uint64
	acked    uint64
	// sent is the highest line ever sent to any client. On re-attachment every
	// replayed line up to it is marked redelivered: a previous Runner may have
	// acted on it without acknowledging it.
	sent uint64

	client *conn
	// unattached is when the last client went away (or the host started).
	unattached time.Time
	stdin      io.WriteCloser
	guard      *processguard.Guard
	agent      int
	spawned    bool
	// readDone: the agent's stdout reached EOF and every line is buffered.
	readDone bool
	exited   bool
	outcome  Outcome
	stderr   *tail
	released bool
	stopping bool
	upstream *url.URL
	proxy    string
	// proxyKey is a random first path segment: the proxy forwards nothing else.
	proxyKey string
}

type conn struct {
	c       net.Conn
	writeMu sync.Mutex
	enc     *json.Encoder
	done    chan struct{}
	once    sync.Once
}

func (c *conn) send(f frame) error { return c.sendWithin(f, 30*time.Second) }
func (c *conn) sendWithin(f frame, d time.Duration) error {
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	_ = c.c.SetWriteDeadline(time.Now().Add(d))
	return c.enc.Encode(f)
}
func (c *conn) close() { c.once.Do(func() { close(c.done); _ = c.c.Close() }) }

func serve() int {
	signal.Ignore(syscall.SIGHUP, syscall.SIGINT, syscall.SIGPIPE)
	var cfg launch
	if err := json.NewDecoder(io.LimitReader(os.Stdin, 1<<16)).Decode(&cfg); err != nil || !validID.MatchString(cfg.ID) || cfg.Dir == "" || len(cfg.Token) < 32 {
		return 125
	}
	_ = os.Stdin.Close()
	if cfg.BufferLimit <= 0 {
		cfg.BufferLimit = DefaultBufferLimit
	}
	if cfg.OrphanAfter <= 0 {
		cfg.OrphanAfter = DefaultOrphanAfter
	}
	h := &host{cfg: cfg, open: map[string]entry{}, answered: map[string]bool{}, stderr: &tail{}, unattached: time.Now()}
	h.cond = sync.NewCond(&h.mu)
	control, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 125
	}
	web, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 125
	}
	h.proxyKey = rand.Text()
	h.proxy = "http://" + web.Addr().String() + "/" + h.proxyKey
	go func() { _ = (&http.Server{Handler: h, ReadHeaderTimeout: 30 * time.Second}).Serve(web) }()
	record := Record{Protocol: Protocol, ID: cfg.ID, PID: os.Getpid(), Port: control.Addr().(*net.TCPAddr).Port, Token: cfg.Token, Proxy: h.proxy}
	if err = writeRecord(cfg.Dir, record); err != nil {
		return 125
	}
	// Readiness: the launcher reads one line, then the host owns nothing of it.
	_ = json.NewEncoder(os.Stdout).Encode(record)
	_ = os.Stdout.Close()
	terminated := make(chan os.Signal, 1)
	signal.Notify(terminated, syscall.SIGTERM)
	go func() { <-terminated; h.terminate() }()
	go h.watchdog()
	for {
		c, err := control.Accept()
		if err != nil {
			return 1
		}
		go h.handle(c)
		h.mu.Lock()
		finished := h.finished()
		h.mu.Unlock()
		if finished {
			break
		}
	}
	return 0
}

// finished reports whether the host has nothing left to keep alive.
func (h *host) finished() bool {
	return h.released || (h.exited && h.readDone && h.stopping && h.client == nil)
}

// watchdog exits the host once its work is done. A host whose first client
// vanished before spawning has nothing to keep and exits too.
//
// A host left unattached longer than its orphan limit stops its program and exits.
//
// A host whose directory was removed has lost its owner for good (its state was
// deleted): no Runner can ever find it again, so it stops its agent and exits.
func (h *host) watchdog() {
	start, checked := time.Now(), time.Now()
	for {
		time.Sleep(50 * time.Millisecond)
		if time.Since(checked) > 2*time.Second {
			checked = time.Now()
			if _, err := os.Stat(h.cfg.Dir); errors.Is(err, os.ErrNotExist) {
				h.terminate()
				h.mu.Lock()
				h.released = true
				h.cond.Broadcast()
				h.mu.Unlock()
			}
		}
		h.mu.Lock()
		done := h.finished() || (!h.spawned && h.client == nil && time.Since(start) > 30*time.Second)
		orphaned := h.spawned && !h.stopping && h.client == nil && time.Since(h.unattached) > h.cfg.OrphanAfter
		h.mu.Unlock()
		if orphaned {
			go h.terminate()
		}
		if done {
			// Nothing here is needed once the host is gone; a Runner looking for
			// it finds no record, which is what a vanished host looks like.
			_ = os.Remove(recordPath(h.cfg.Dir))
			_ = os.RemoveAll(h.cfg.Dir)
			os.Exit(0)
		}
	}
}

func (h *host) handle(raw net.Conn) {
	c := &conn{c: raw, enc: json.NewEncoder(raw), done: make(chan struct{})}
	defer c.close()
	dec := json.NewDecoder(bufio.NewReaderSize(raw, 64<<10))
	_ = raw.SetReadDeadline(time.Now().Add(10 * time.Second))
	var hello frame
	if err := dec.Decode(&hello); err != nil || (hello.Op != "hello" && hello.Op != "probe") || subtle.ConstantTimeCompare([]byte(hello.Token), []byte(h.cfg.Token)) != 1 {
		return
	}
	if hello.Op == "probe" {
		// Inspection only: never supersedes the attached client.
		h.mu.Lock()
		status := frame{Op: "status", Protocol: Protocol, Attached: h.client != nil, Spawned: h.spawned, Exited: h.exited, AgentPID: h.agent}
		h.mu.Unlock()
		_ = c.send(status)
		return
	}
	_ = raw.SetReadDeadline(time.Time{})
	if hello.Protocol != Protocol {
		_ = c.send(frame{Op: "error", Protocol: Protocol, Text: ErrIncompatible.Error()})
		// Keep the connection for version-stable terminate/release.
	}
	h.mu.Lock()
	if old := h.client; old != nil {
		// A newer Runner supersedes an old or half-open one. Tell the old one,
		// so it stands down rather than reporting a lost host.
		go func() { _ = old.sendWithin(frame{Op: "superseded"}, 2*time.Second); old.close() }()
	}
	h.client = c
	h.ackLocked(hello.Ack)
	replay, cursor, sent := h.replayLocked(), h.acked, h.sent
	welcome := frame{Op: "welcome", Protocol: Protocol, PID: os.Getpid(), AgentPID: h.agent, Proxy: h.proxy, Spawned: h.spawned, Exited: h.exited, Next: h.next + 1}
	h.cond.Broadcast()
	h.mu.Unlock()
	if hello.Protocol == Protocol {
		if c.send(welcome) != nil {
			h.drop(c)
			return
		}
		go h.pump(c, replay, cursor, sent)
	}
	for {
		var f frame
		if err := dec.Decode(&f); err != nil {
			h.drop(c)
			return
		}
		if hello.Protocol != Protocol && f.Op != "terminate" && f.Op != "release" {
			continue
		}
		switch f.Op {
		case "spawn":
			err := h.spawn(f.Spec)
			if err != nil {
				_ = c.send(frame{Op: "error", Text: err.Error()})
			} else {
				h.mu.Lock()
				pid := h.agent
				h.mu.Unlock()
				_ = c.send(frame{Op: "spawned", AgentPID: pid})
			}
		case "send":
			h.write(f.Line)
		case "ack":
			h.mu.Lock()
			h.ackLocked(f.Seq)
			h.cond.Broadcast()
			h.mu.Unlock()
		case "upstream":
			h.setUpstream(f.URL)
		case "terminate":
			go h.terminate()
		case "release":
			h.mu.Lock()
			if h.exited || !h.spawned {
				h.released = true
			}
			h.mu.Unlock()
		}
	}
}

func (h *host) drop(c *conn) {
	h.mu.Lock()
	if h.client == c {
		h.client = nil
		h.unattached = time.Now()
		h.upstream = nil // The gateway belonged to the departed Runner.
	}
	h.cond.Broadcast()
	h.mu.Unlock()
	c.close()
}

// replayLocked returns acknowledged-but-unanswered requests, then everything
// unacknowledged, in sequence order. Answered requests are never replayed.
func (h *host) replayLocked() []frame {
	var out []frame
	pending := make([]entry, 0, len(h.open))
	for _, e := range h.open {
		pending = append(pending, e)
	}
	for i := 1; i < len(pending); i++ {
		for j := i; j > 0 && pending[j].seq < pending[j-1].seq; j-- {
			pending[j], pending[j-1] = pending[j-1], pending[j]
		}
	}
	for _, e := range pending {
		out = append(out, frame{Op: "msg", Seq: e.seq, Line: e.line, Redelivered: true})
	}
	return out
}

// pump sends the replay, then every buffered line after the acknowledged cursor,
// then live output, until the client goes away. The exit frame follows the last line.
func (h *host) pump(c *conn, replay []frame, cursor, sent uint64) {
	for _, f := range replay {
		if c.send(f) != nil {
			h.drop(c)
			return
		}
	}
	exitSent := false
	for {
		h.mu.Lock()
		for h.client == c && !h.hasAfterLocked(cursor) && !(h.exited && h.readDone && !exitSent) {
			h.cond.Wait()
		}
		if h.client != c {
			h.mu.Unlock()
			return
		}
		var batch []frame
		for _, e := range h.log {
			if e.seq <= cursor {
				continue
			}
			cursor = e.seq
			if e.request && h.answered[e.id] {
				continue
			}
			batch = append(batch, frame{Op: "msg", Seq: e.seq, Line: e.line, Redelivered: e.seq <= sent})
			if e.seq > h.sent {
				h.sent = e.seq
			}
		}
		var exit *frame
		if h.exited && h.readDone && !exitSent && !h.hasAfterLocked(cursor) {
			outcome := h.outcome
			exit = &frame{Op: "exit", Outcome: &outcome, Text: h.stderr.text()}
		}
		h.mu.Unlock()
		for _, f := range batch {
			if c.send(f) != nil {
				h.drop(c)
				return
			}
		}
		if exit != nil {
			if c.send(*exit) != nil {
				h.drop(c)
				return
			}
			exitSent = true
		}
	}
}

func (h *host) hasAfterLocked(cursor uint64) bool {
	return len(h.log) > 0 && h.log[len(h.log)-1].seq > cursor
}

func (h *host) ackLocked(seq uint64) {
	if seq > h.next {
		seq = h.next
	}
	if seq <= h.acked {
		return
	}
	h.acked = seq
	keep := h.log[:0]
	for _, e := range h.log {
		if e.seq > seq {
			keep = append(keep, e)
			continue
		}
		h.size -= int64(len(e.line))
		if e.request {
			if h.answered[e.id] {
				delete(h.answered, e.id)
			} else {
				h.open[e.id] = e
			}
		}
	}
	// Do not retain the backing array of acknowledged lines.
	h.log = append([]entry(nil), keep...)
}

func (h *host) record(line []byte) {
	id, request, _ := classify(line)
	h.mu.Lock()
	defer h.mu.Unlock()
	for h.size >= h.cfg.BufferLimit && !h.released {
		h.cond.Wait() // Backpressure: the agent blocks on its own stdout.
	}
	h.next++
	e := entry{seq: h.next, line: append(json.RawMessage(nil), line...), id: id, request: request}
	h.log = append(h.log, e)
	h.size += int64(len(e.line))
	h.cond.Broadcast()
}

func (h *host) write(line json.RawMessage) {
	if id, _, response := classify(line); response {
		h.mu.Lock()
		if _, ok := h.open[id]; ok {
			delete(h.open, id)
		} else {
			h.answered[id] = true
		}
		h.mu.Unlock()
	}
	h.mu.Lock()
	stdin := h.stdin
	h.mu.Unlock()
	if stdin != nil {
		_, _ = stdin.Write(append(append([]byte(nil), line...), '\n'))
	}
}

func (h *host) spawn(spec *Spec) error {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.spawned {
		return errors.New("agent already spawned")
	}
	if spec == nil || spec.Path == "" {
		return errors.New("agent spec required")
	}
	cmd := exec.Command(spec.Path, spec.Args...)
	cmd.Env = spec.Env
	cmd.Dir = spec.Cwd
	var in io.WriteCloser
	var readers []func()
	var closers []*os.File
	fail := func(err error) error {
		if in != nil {
			in.Close()
		}
		for _, f := range closers {
			f.Close()
		}
		return err
	}
	pipe := func() (*os.File, *os.File, error) {
		r, w, err := os.Pipe()
		if err == nil {
			closers = append(closers, r, w)
		}
		return r, w, err
	}
	if spec.Stream {
		// A managed command: no stdin, both streams captured as chunks.
		for _, channel := range []string{"stdout", "stderr"} {
			r, w, err := pipe()
			if err != nil {
				return fail(err)
			}
			if channel == "stdout" {
				cmd.Stdout = w
			} else {
				cmd.Stderr = w
			}
			readers = append(readers, func() { h.readChunks(channel, r) })
		}
	} else {
		var err error
		if in, err = cmd.StdinPipe(); err != nil {
			return fail(err)
		}
		r, w, err := pipe()
		if err != nil {
			return fail(err)
		}
		cmd.Stdout = w
		h.stderr.redact(spec.Env)
		cmd.Stderr = h.stderr
		readers = append(readers, func() { h.readLines(r) })
	}
	guard, err := processguard.Start(cmd)
	// The child holds the write ends now; the host keeps only the read ends.
	for i := 1; i < len(closers); i += 2 {
		closers[i].Close()
	}
	if err != nil {
		if in != nil {
			in.Close()
		}
		for i := 0; i < len(closers); i += 2 {
			closers[i].Close()
		}
		return err
	}
	h.stdin, h.guard, h.spawned = in, guard, true
	if cmd.Process != nil {
		h.agent = cmd.Process.Pid
	}
	readDone := make(chan struct{})
	go func() {
		defer close(readDone)
		var wg sync.WaitGroup
		for _, read := range readers {
			wg.Add(1)
			go func() { defer wg.Done(); read() }()
		}
		wg.Wait()
		h.mu.Lock()
		h.readDone = true
		h.cond.Broadcast()
		h.mu.Unlock()
	}()
	go func() {
		outcome, err := guard.Wait()
		// Escaped descendants can keep output open; do not wait for them forever.
		select {
		case <-readDone:
		case <-time.After(time.Second):
			for i := 0; i < len(closers); i += 2 {
				closers[i].Close()
			}
			<-readDone
			if err == nil {
				err = processguard.ErrCleanupUnconfirmed
			}
		}
		h.mu.Lock()
		h.exited = true
		h.outcome = Outcome(outcome)
		if err != nil && h.outcome.Error == "" {
			h.outcome.Error = err.Error()
		}
		h.cond.Broadcast()
		h.mu.Unlock()
	}()
	return nil
}

// readLines records an agent's JSONL output, one entry per line.
func (h *host) readLines(out *os.File) {
	defer out.Close()
	s := bufio.NewScanner(out)
	s.Buffer(make([]byte, 4096), 16<<20)
	for s.Scan() {
		h.record(s.Bytes())
	}
	if err := s.Err(); err != nil && !errors.Is(err, os.ErrClosed) {
		// An unframeable line ends the protocol, as it does for a local pipe.
		_, _ = h.stderr.Write([]byte("\nsession host: agent output unreadable: " + err.Error()))
		h.mu.Lock()
		guard := h.guard
		h.mu.Unlock()
		if guard != nil {
			_ = guard.Stop()
		}
	}
}

// readChunks records a command's output stream, one entry per read.
func (h *host) readChunks(channel string, out *os.File) {
	defer out.Close()
	buf := make([]byte, ChunkSize)
	for {
		n, err := out.Read(buf)
		if n > 0 {
			line, _ := json.Marshal(Chunk{Channel: channel, Data: buf[:n]})
			h.record(line)
		}
		if err != nil {
			return
		}
	}
}

// terminate closes the agent's stdin, gives it two seconds, then has the guard
// kill its whole process group, mirroring jsonl.Process.Close.
func (h *host) terminate() {
	h.mu.Lock()
	h.stopping = true
	stdin, guard, spawned := h.stdin, h.guard, h.spawned
	if !spawned {
		h.released = true
	}
	h.cond.Broadcast()
	h.mu.Unlock()
	if !spawned {
		return
	}
	if stdin == nil {
		// A streamed command has no stdin to close: there is no graceful request.
		_ = guard.Stop()
		return
	}
	_ = stdin.Close()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		h.mu.Lock()
		exited := h.exited
		h.mu.Unlock()
		if exited {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	_ = guard.Stop()
}

func (h *host) setUpstream(raw string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if raw == "" {
		h.upstream = nil
		return
	}
	u, err := url.Parse(raw)
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		h.upstream = nil
		return
	}
	h.upstream = u
}

// ServeHTTP forwards provider traffic to the attached Runner's gateway.
func (h *host) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	h.mu.Lock()
	upstream, prefix := h.upstream, "/"+h.proxyKey
	h.mu.Unlock()
	if r.URL.Path != prefix && !strings.HasPrefix(r.URL.Path, prefix+"/") {
		http.NotFound(w, r)
		return
	}
	if upstream == nil {
		w.Header().Set("Retry-After", "2")
		http.Error(w, "lazybox Runner gateway unavailable; retry", http.StatusServiceUnavailable)
		return
	}
	proxy := &httputil.ReverseProxy{
		Rewrite: func(p *httputil.ProxyRequest) {
			p.Out.URL.Path = strings.TrimPrefix(p.Out.URL.Path, prefix)
			p.Out.URL.RawPath = ""
			p.SetURL(upstream)
			p.Out.Host = upstream.Host
		},
		FlushInterval: -1,
		ErrorHandler: func(w http.ResponseWriter, _ *http.Request, _ error) {
			w.Header().Set("Retry-After", "2")
			http.Error(w, "lazybox Runner gateway unavailable; retry", http.StatusServiceUnavailable)
		},
	}
	proxy.ServeHTTP(w, r)
}

// tail keeps bounded, redacted agent stderr for exit diagnostics.
type tail struct {
	mu      sync.Mutex
	data    []byte
	secrets []string
}

func (t *tail) redact(env []string) {
	t.mu.Lock()
	defer t.mu.Unlock()
	for _, entry := range env {
		name, value, ok := strings.Cut(entry, "=")
		upper := strings.ToUpper(name)
		if ok && value != "" && (strings.Contains(upper, "KEY") || strings.Contains(upper, "TOKEN") || strings.Contains(upper, "SECRET") || strings.Contains(upper, "PASSWORD")) {
			t.secrets = append(t.secrets, value)
		}
	}
}
func (t *tail) Write(data []byte) (int, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	n := len(data)
	if n > 8192 {
		data = data[n-8192:]
	}
	t.data = append(t.data, data...)
	if len(t.data) > 8192 {
		t.data = t.data[len(t.data)-8192:]
	}
	return n, nil
}
func (t *tail) text() string {
	t.mu.Lock()
	defer t.mu.Unlock()
	text := strings.TrimSpace(string(t.data))
	for _, secret := range t.secrets {
		text = strings.ReplaceAll(text, secret, "[redacted]")
	}
	return text
}
