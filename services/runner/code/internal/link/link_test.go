package link

import (
	"bufio"
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// upstream is a stand-in for the loopback Runner: it records what reached
// it and answers unary calls and a stream.
type upstream struct {
	*httptest.Server
	token    string
	mu       sync.Mutex
	requests []*http.Request
}

func newUpstream(t *testing.T, state string) *upstream {
	t.Helper()
	u := &upstream{token: "owner-bearer-" + t.Name()}
	u.Server = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		u.mu.Lock()
		u.requests = append(u.requests, r.Clone(context.Background()))
		u.mu.Unlock()
		if r.Header.Get("Authorization") != "Bearer "+u.token {
			http.Error(w, "bad bearer", http.StatusUnauthorized)
			return
		}
		if r.Host != strings.TrimPrefix(u.URL, "http://") {
			http.Error(w, "wrong host", http.StatusNotFound)
			return
		}
		switch strings.TrimPrefix(r.URL.Path, RPCPath) {
		case "Info":
			w.Header().Set("Content-Type", "application/json")
			body, _ := io.ReadAll(r.Body)
			fmt.Fprintf(w, `{"echo":%q}`, body)
		case "WatchEvents":
			w.Header().Set("Content-Type", "application/connect+json")
			flusher := w.(http.Flusher)
			for i := range 3 {
				fmt.Fprintf(w, "event-%d\n", i)
				flusher.Flush()
				time.Sleep(20 * time.Millisecond)
			}
		default:
			http.Error(w, "no such method", http.StatusNotFound)
		}
	}))
	t.Cleanup(u.Close)
	writeEndpoint(t, state, u.URL, u.token)
	return u
}

func writeEndpoint(t *testing.T, state, url, token string) {
	t.Helper()
	data, _ := json.Marshal(map[string]string{"url": url, "token": token, "runnerId": "remoterunneridentity0001"})
	if err := os.WriteFile(filepath.Join(state, "endpoint.json"), data, 0o600); err != nil {
		t.Fatal(err)
	}
}

func (u *upstream) seen() []*http.Request {
	u.mu.Lock()
	defer u.mu.Unlock()
	return append([]*http.Request(nil), u.requests...)
}

func newClient(t *testing.T) ClientKeys {
	t.Helper()
	keys, err := FileKeys{Path: filepath.Join(t.TempDir(), "keys", "link-client.json")}.Keys()
	if err != nil {
		t.Fatal(err)
	}
	return keys
}

func enrol(t *testing.T, state string, keys ClientKeys) Owner {
	t.Helper()
	identity, err := keys.IdentityPublic()
	if err != nil {
		t.Fatal(err)
	}
	owner, err := Owners{State: state}.Add("Test laptop", keys.ChannelPublic(), identity, "test", time.Now())
	if err != nil {
		t.Fatal(err)
	}
	return owner
}

// recorder keeps every byte that crossed a stream, both ways.
type recorder struct {
	io.ReadWriteCloser
	mu   sync.Mutex
	seen bytes.Buffer
	// log is what `link serve` would print on ssh's stderr; done once it returned.
	log  limitedBuffer
	done chan struct{}
}

func (r *recorder) Diagnosis() string {
	select {
	case <-r.done:
	case <-time.After(time.Second):
	}
	return r.log.String()
}

func (r *recorder) Read(p []byte) (int, error) {
	n, err := r.ReadWriteCloser.Read(p)
	r.mu.Lock()
	r.seen.Write(p[:n])
	r.mu.Unlock()
	return n, err
}

func (r *recorder) Write(p []byte) (int, error) {
	r.mu.Lock()
	r.seen.Write(p)
	r.mu.Unlock()
	return r.ReadWriteCloser.Write(p)
}

// pipeRoute is an in-process route: each Dial is a fresh pipe with Serve on
// the other end, as `ssh host lazybox link serve --stdio` is.
type pipeRoute struct {
	opts    ServeOptions
	dials   atomic.Int32
	mu      sync.Mutex
	streams []*recorder
	served  chan error
	fail    atomic.Bool
}

func newPipeRoute(t *testing.T, opts ServeOptions) *pipeRoute {
	t.Helper()
	p := &pipeRoute{opts: opts, served: make(chan error, 64)}
	t.Cleanup(func() {
		p.cut()
		p.mu.Lock()
		streams := append([]*recorder(nil), p.streams...)
		p.mu.Unlock()
		for _, stream := range streams {
			select {
			case <-stream.done:
			case <-time.After(5 * time.Second):
				t.Error("link server did not finish before fixture cleanup")
			}
		}
	})
	return p
}

func (p *pipeRoute) Kind() string { return "test" }
func (p *pipeRoute) Describe() RouteInfo {
	return RouteInfo{Kind: "test", Label: "Test", Host: "test-host"}
}
func (p *pipeRoute) Dial(ctx context.Context) (io.ReadWriteCloser, error) {
	if p.fail.Load() {
		return nil, errors.New("the network is down")
	}
	p.dials.Add(1)
	client, server := net.Pipe()
	r := &recorder{ReadWriteCloser: client, done: make(chan struct{})}
	opts := p.opts
	opts.Log = &r.log
	go func() { err := Serve(context.Background(), server, opts); close(r.done); p.served <- err }()
	p.mu.Lock()
	p.streams = append(p.streams, r)
	p.mu.Unlock()
	return r, nil
}

// cut drops every stream, as a dead ssh would.
func (p *pipeRoute) cut() {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, s := range p.streams {
		_ = s.Close()
	}
}

func (p *pipeRoute) wire() []byte {
	p.mu.Lock()
	defer p.mu.Unlock()
	var all bytes.Buffer
	for _, s := range p.streams {
		s.mu.Lock()
		all.Write(s.seen.Bytes())
		s.mu.Unlock()
	}
	return all.Bytes()
}

func runnerPublic(t *testing.T, state string) []byte {
	t.Helper()
	key, err := LoadOrCreateRunnerKey(state)
	if err != nil {
		t.Fatal(err)
	}
	return key.Public
}

func serveOptions(state string) ServeOptions {
	return ServeOptions{State: state, Endpoint: filepath.Join(state, "endpoint.json"), Route: "test", Machine: "build-box", Release: "0.1.22", RevokeCheck: 20 * time.Millisecond}
}

func call(t *testing.T, client *http.Client, base, method, body string) (int, string) {
	t.Helper()
	req, _ := http.NewRequest(http.MethodPost, base+RPCPath+method, strings.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	resp, err := client.Do(req)
	if err != nil {
		t.Fatalf("%s: %v", method, err)
	}
	defer resp.Body.Close()
	data, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(data)
}

func TestAnEnrolledOwnerReachesTheRunnerWithTheBearerAddedOnTheHost(t *testing.T) {
	state := t.TempDir()
	up := newUpstream(t, state)
	keys := newClient(t)
	enrol(t, state, keys)
	route := newPipeRoute(t, serveOptions(state))
	session, err := Dial(context.Background(), route, keys, runnerPublic(t, state), "Test laptop")
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()
	if session.Ready.Principal != "owner" || session.Runner.Machine != "build-box" || session.Runner.RunnerID != "remoterunneridentity0001" {
		t.Fatalf("ready %+v hello %+v", session.Ready, session.Runner)
	}
	client := session.HTTPClient()
	status, body := call(t, client, "http://runner", "Info", `{"x":1}`)
	if status != 200 || !strings.Contains(body, `\"x\":1`) {
		t.Fatalf("Info: %d %s", status, body)
	}
	// A request that carries any credential of its own is refused, whatever it is.
	req, _ := http.NewRequest(http.MethodPost, "http://runner"+RPCPath+"Info", strings.NewReader("{}"))
	req.Header.Set("Authorization", "Bearer "+up.token)
	resp, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusUnauthorized {
		t.Fatalf("a request with its own bearer: %d", resp.StatusCode)
	}
	// Only the Runner's RPCs, and not the host-local ones.
	if status, _ := call(t, client, "http://runner", "OpenRepositoryHome", "{}"); status != http.StatusForbidden {
		t.Fatalf("OpenRepositoryHome over a link: %d", status)
	}
	req, _ = http.NewRequest(http.MethodGet, "http://runner/remote/v1/challenge", nil)
	if resp, err = client.Do(req); err != nil || resp.StatusCode != http.StatusNotFound {
		t.Fatalf("a non-RPC path: %v %v", resp, err)
	}
	resp.Body.Close()
	if n := len(up.seen()); n != 1 {
		t.Fatalf("the upstream saw %d requests, want only the one allowed call", n)
	}
	// The bearer never crossed the link: it is added on the host.
	if bytes.Contains(route.wire(), []byte(up.token)) {
		t.Fatal("the owner bearer appeared on the link")
	}
	data, _ := os.ReadFile(filepath.Join(state, "audit.jsonl"))
	if !strings.Contains(string(data), `"event":"link.opened"`) || !strings.Contains(string(data), `"name":"Test laptop"`) {
		t.Fatalf("audit: %s", data)
	}
}

func TestAWrongPinnedKeyIsRefusedBeforeAnyRequest(t *testing.T) {
	state := t.TempDir()
	up := newUpstream(t, state)
	keys := newClient(t)
	enrol(t, state, keys)
	// The client pinned another Runner's key (an SSH alias now pointing elsewhere).
	other := runnerPublic(t, t.TempDir())
	route := newPipeRoute(t, serveOptions(state))
	_, err := Dial(context.Background(), route, keys, other, "Test laptop")
	if !errors.Is(err, ErrHandshake) {
		t.Fatalf("dial with a wrong pin: %v", err)
	}
	var refusal *Refusal
	if served := <-route.served; !errors.As(served, &refusal) || refusal.Code != "wrong-runner" {
		t.Fatalf("the Runner's side: %v", served)
	}
	if len(up.seen()) != 0 {
		t.Fatal("a request reached the Runner")
	}
}

func TestAKeyThatIsNotEnrolledOrWasRevokedIsRefused(t *testing.T) {
	state := t.TempDir()
	up := newUpstream(t, state)
	pin := runnerPublic(t, state)
	stranger := newClient(t)
	route := newPipeRoute(t, serveOptions(state))
	if _, err := Dial(context.Background(), route, stranger, pin, "Stranger"); !errors.Is(err, ErrHandshake) {
		t.Fatalf("a stranger: %v", err)
	}
	var refusal *Refusal
	if served := <-route.served; !errors.As(served, &refusal) || refusal.Code != "unknown-key" {
		t.Fatalf("the Runner's side for a stranger: %v", served)
	}

	keys := newClient(t)
	owner := enrol(t, state, keys)
	if _, err := (Owners{State: state}).Revoke(owner.KeyID[:8]); err != nil {
		t.Fatal(err)
	}
	if _, err := Dial(context.Background(), route, keys, pin, "Test laptop"); !errors.Is(err, ErrHandshake) {
		t.Fatalf("a revoked owner: %v", err)
	}
	if served := <-route.served; !errors.As(served, &refusal) || refusal.Code != "unknown-key" {
		t.Fatalf("the Runner's side for a revoked owner: %v", served)
	}
	if len(up.seen()) != 0 {
		t.Fatal("a request reached the Runner")
	}
}

func TestTheChannelKeyAloneIsNotEnough(t *testing.T) {
	state := t.TempDir()
	newUpstream(t, state)
	keys := newClient(t)
	enrol(t, state, keys)
	// Someone who copied the channel key but not the identity key.
	thief := keys
	var err error
	if thief.Identity, err = ecdsa.GenerateKey(elliptic.P256(), rand.Reader); err != nil {
		t.Fatal(err)
	}
	route := newPipeRoute(t, serveOptions(state))
	_, err = Dial(context.Background(), route, thief, runnerPublic(t, state), "Thief")
	var refusal *Refusal
	if !errors.As(err, &refusal) || refusal.Code != "proof" {
		t.Fatalf("a wrong identity key: %v", err)
	}
}

func TestRevokingAnOwnerEndsItsLiveSession(t *testing.T) {
	state := t.TempDir()
	newUpstream(t, state)
	keys := newClient(t)
	owner := enrol(t, state, keys)
	route := newPipeRoute(t, serveOptions(state))
	session, err := Dial(context.Background(), route, keys, runnerPublic(t, state), "Test laptop")
	if err != nil {
		t.Fatal(err)
	}
	defer session.Close()
	if status, _ := call(t, session.HTTPClient(), "http://runner", "Info", "{}"); status != 200 {
		t.Fatalf("before revoke: %d", status)
	}
	revoked := time.Now()
	if _, err = (Owners{State: state}).Revoke(owner.KeyID); err != nil {
		t.Fatal(err)
	}
	select {
	case <-session.Done():
	case <-time.After(time.Second):
		t.Fatal("the session outlived its key by more than a second")
	}
	t.Logf("session ended %s after the revoke", time.Since(revoked).Round(time.Millisecond))
	data, _ := os.ReadFile(filepath.Join(state, "audit.jsonl"))
	if !strings.Contains(string(data), "key revoked") {
		t.Fatalf("audit: %s", data)
	}
}

func TestStreamsFlowThroughTheLinkAsTheyAreWritten(t *testing.T) {
	state := t.TempDir()
	newUpstream(t, state)
	keys := newClient(t)
	enrol(t, state, keys)
	agent := &Agent{Route: newPipeRoute(t, serveOptions(state)), Keys: keys, RunnerKey: runnerPublic(t, state), ClientName: "Test laptop", Label: "build-box"}
	base, token, err := agent.Listen()
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go func() { _ = agent.Run(ctx) }()
	waitFor(t, func() bool { return agent.Status().State == "connected" })
	req, _ := http.NewRequest(http.MethodPost, base+RPCPath+"WatchEvents", strings.NewReader("{}"))
	req.Header.Set("Authorization", "Bearer "+token)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	lines := bufio.NewScanner(resp.Body)
	var at []time.Time
	for lines.Scan() {
		at = append(at, time.Now())
	}
	if len(at) != 3 || at[2].Sub(at[0]) < 30*time.Millisecond {
		t.Fatalf("events arrived together or not at all: %v", at)
	}
}

func waitFor(t *testing.T, ok func() bool) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for !ok() {
		if time.Now().After(deadline) {
			t.Fatal("timed out")
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func TestTheAgentServesLoopbackWithItsTokenAndReconnects(t *testing.T) {
	state := t.TempDir()
	up := newUpstream(t, state)
	keys := newClient(t)
	owner := enrol(t, state, keys)
	route := newPipeRoute(t, serveOptions(state))
	var mu sync.Mutex
	var states []string
	agent := &Agent{Route: route, Keys: keys, RunnerKey: runnerPublic(t, state), ClientName: "Test laptop", Label: "build-box", Ping: 50 * time.Millisecond,
		OnStatus: func(s Status) {
			mu.Lock()
			if len(states) == 0 || states[len(states)-1] != s.State {
				states = append(states, s.State)
			}
			mu.Unlock()
		}}
	base, token, err := agent.Listen()
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(base, "http://127.0.0.1:") {
		t.Fatalf("not loopback: %s", base)
	}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() { _ = agent.Run(ctx); close(done) }()
	waitFor(t, func() bool { return agent.Status().State == "connected" })
	if agent.Status().RTTMillis == 0 || agent.Status().Machine != "build-box" {
		t.Fatalf("status %+v", agent.Status())
	}
	authed := func(method string) (int, string) {
		req, _ := http.NewRequest(http.MethodPost, base+RPCPath+method, strings.NewReader("{}"))
		req.Header.Set("Authorization", "Bearer "+token)
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		defer resp.Body.Close()
		body, _ := io.ReadAll(resp.Body)
		return resp.StatusCode, string(body)
	}
	if status, body := authed("Info"); status != 200 {
		t.Fatalf("through the agent: %d %s", status, body)
	}
	// No token, a wrong token, another Host: refused here, nothing crosses.
	before := len(up.seen())
	if status, _ := call(t, http.DefaultClient, base, "Info", "{}"); status != http.StatusUnauthorized {
		t.Fatalf("without the token: %d", status)
	}
	req, _ := http.NewRequest(http.MethodPost, base+RPCPath+"Info", strings.NewReader("{}"))
	req.Header.Set("Authorization", "Bearer "+token)
	req.Host = "evil.example:80"
	if resp, err := http.DefaultClient.Do(req); err != nil || resp.StatusCode != http.StatusNotFound {
		t.Fatalf("a rebinding Host: %v %v", resp, err)
	}
	if len(up.seen()) != before {
		t.Fatal("a refused request reached the Runner")
	}
	// The upstream never sees the local token.
	for _, r := range up.seen() {
		if strings.Contains(r.Header.Get("Authorization"), token) {
			t.Fatal("the local token crossed the link")
		}
	}

	// ssh dies (sleep, Wi-Fi change): the agent says so and comes back.
	route.cut()
	waitFor(t, func() bool { return route.dials.Load() >= 2 && agent.Status().State == "connected" })
	if status, _ := authed("Info"); status != 200 {
		t.Fatalf("after reconnecting: %d", status)
	}
	// While the network is down a call says it is reconnecting, as unavailable.
	route.fail.Store(true)
	route.cut()
	waitFor(t, func() bool { return agent.Status().State == "reconnecting" })
	if status, body := authed("Info"); status != http.StatusServiceUnavailable || !strings.Contains(body, "Reconnecting to build-box") {
		t.Fatalf("while down: %d %s", status, body)
	}
	route.fail.Store(false)
	agent.Wake()
	waitFor(t, func() bool { return agent.Status().State == "connected" })

	// Revoked on the host: the session ends, and the agent says why.
	if _, err = (Owners{State: state}).Revoke(owner.KeyID); err != nil {
		t.Fatal(err)
	}
	waitFor(t, func() bool { return agent.Status().State == "refused" })
	if s := agent.Status(); s.Code != "unknown-key" || !strings.Contains(s.Problem, "build-box no longer accepts") {
		t.Fatalf("revoked status %+v", s)
	}
	if status, _ := authed("Info"); status != http.StatusUnauthorized {
		t.Fatalf("after the revoke: %d", status)
	}
	cancel()
	<-done
	mu.Lock()
	defer mu.Unlock()
	if strings.Join(states, ",") != "connecting,connected,reconnecting,connected,reconnecting,connected,reconnecting,refused,stopped" {
		t.Logf("states: %v", states)
	}
	if states[0] != "connecting" || states[len(states)-1] != "stopped" {
		t.Fatalf("states: %v", states)
	}
}

func TestOwnersAndKeys(t *testing.T) {
	state := t.TempDir()
	keys := newClient(t)
	identity, _ := keys.IdentityPublic()
	owners := Owners{State: state}
	a, err := owners.Add("Laptop", keys.ChannelPublic(), identity, "ssh", time.Now())
	if err != nil {
		t.Fatal(err)
	}
	// Enrolling again renames; a different identity for the same channel key is refused.
	if b, err := owners.Add("Laptop 2", keys.ChannelPublic(), identity, "ssh", time.Now()); err != nil || b.KeyID != a.KeyID || b.Name != "Laptop 2" {
		t.Fatalf("re-enrol: %+v %v", b, err)
	}
	other := newClient(t)
	otherIdentity, _ := other.IdentityPublic()
	if _, err = owners.Add("Laptop", keys.ChannelPublic(), otherIdentity, "ssh", time.Now()); err == nil {
		t.Fatal("a second identity for one channel key was accepted")
	}
	if _, err = owners.Add("Bad\nname", other.ChannelPublic(), otherIdentity, "ssh", time.Now()); err == nil {
		t.Fatal("a two-line name was accepted")
	}
	if _, err = owners.Add("Short", []byte("short"), otherIdentity, "ssh", time.Now()); err == nil {
		t.Fatal("a short channel key was accepted")
	}
	list, _ := owners.List()
	if len(list) != 1 {
		t.Fatalf("owners: %+v", list)
	}
	info, _ := os.Stat(filepath.Join(state, OwnersFile))
	if info.Mode().Perm() != 0o600 {
		t.Fatalf("owners.json mode %v", info.Mode())
	}
	if _, err = owners.Revoke("nope"); err == nil {
		t.Fatal("revoking an unknown key")
	}
	if _, err = owners.Revoke(a.KeyID); err != nil {
		t.Fatal(err)
	}
	if list, _ = owners.List(); len(list) != 0 {
		t.Fatalf("after revoke: %+v", list)
	}
	// The Runner key is made once and kept; a key file others can read is refused.
	first := runnerPublic(t, state)
	if !bytes.Equal(first, runnerPublic(t, state)) {
		t.Fatal("the Runner key changed")
	}
	if err = os.Chmod(filepath.Join(state, RunnerKeyFile), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err = LoadOrCreateRunnerKey(state); err == nil {
		t.Fatal("a readable key file was used")
	}
	// Client keys are stable across reads.
	path := filepath.Join(t.TempDir(), "k.json")
	k1, _ := FileKeys{Path: path}.Keys()
	k2, err := FileKeys{Path: path}.Keys()
	if err != nil || k1.ID() != k2.ID() || !k1.Identity.Equal(k2.Identity) {
		t.Fatalf("client keys changed: %v", err)
	}
}

func TestShellWordsAndHosts(t *testing.T) {
	for _, ok := range []string{"build-box", "me@build-box.local", "dev_vm", "10.0.0.2", "[::1]", "lazybox-test-linux@orb"} {
		if !ValidHost(ok) {
			t.Errorf("%q refused", ok)
		}
	}
	for _, bad := range []string{"-oProxyCommand=evil", "a b", "a;b", "", "$(x)", "a|b"} {
		if ValidHost(bad) {
			t.Errorf("%q accepted", bad)
		}
	}
	if w, _ := shellWord("/home/me/.local/share/lazybox/runner/current/bin/lazybox"); w != "/home/me/.local/share/lazybox/runner/current/bin/lazybox" {
		t.Errorf("plain path quoted: %s", w)
	}
	if w, _ := shellWord("My Laptop"); w != "'My Laptop'" {
		t.Errorf("space: %s", w)
	}
	if _, err := shellWord("it's"); err == nil {
		t.Error("a quote was passed")
	}
	args, _ := SSHRoute{Host: "build-box", ControlDir: "/tmp/c"}.Args("/opt/lazybox", "link", "serve", "--stdio")
	joined := strings.Join(args, " ")
	if !strings.Contains(joined, "BatchMode=yes") || !strings.Contains(joined, "ControlPath=/tmp/c/%C") || !strings.HasSuffix(joined, "build-box /opt/lazybox link serve --stdio") {
		t.Errorf("ssh args: %s", joined)
	}
	// A directory too deep for a socket path: no multiplexing, rather than ssh failing.
	deep, _ := SSHRoute{Host: "build-box", ControlDir: "/" + strings.Repeat("d", 80)}.Args()
	if strings.Contains(strings.Join(deep, " "), "ControlMaster") {
		t.Errorf("multiplexing with a %d-byte socket path", 82+40)
	}
}
