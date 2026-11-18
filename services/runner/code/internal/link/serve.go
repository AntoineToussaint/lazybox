package link

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"golang.org/x/net/http2"
)

// RPCPath is the only path a link carries: the Runner's Connect service.
const RPCPath = "/lazybox.bench.v1.RunnerService/"

// hostLocal are the RPCs that act on the host's own screen (REMOTE_RUNNERS
// §2.2): a remote owner gets something else from them, so the link refuses
// them rather than open a Finder window nobody sees.
var hostLocal = map[string]string{
	"OpenRepositoryHome": "Opening a folder in Finder or an editor acts on that computer's own screen; it is not available from another computer.",
}

// Upstream is the loopback Runner a link proxies to: its origin and the owner
// bearer, as its endpoint.json says now (both change when it restarts).
type Upstream struct {
	URL      string `json:"url"`
	Token    string `json:"token"`
	RunnerID string `json:"runnerId"`
}

// ReadEndpoint reads a Runner's endpoint file and checks it names a loopback
// origin: a link never proxies anywhere else.
func ReadEndpoint(path string) (Upstream, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return Upstream{}, err
	}
	var up Upstream
	if err = json.Unmarshal(data, &up); err != nil || up.URL == "" || up.Token == "" {
		return Upstream{}, fmt.Errorf("%s is not a Runner endpoint file", path)
	}
	parsed, err := url.Parse(up.URL)
	if err != nil || parsed.Scheme != "http" || !loopbackHost(parsed.Hostname()) {
		return Upstream{}, fmt.Errorf("%s names %q, not a loopback Runner", path, up.URL)
	}
	return up, nil
}

func loopbackHost(host string) bool {
	if host == "localhost" {
		return true
	}
	ip := net.ParseIP(host)
	return ip != nil && ip.IsLoopback()
}

// ServeOptions is one Runner's link endpoint.
type ServeOptions struct {
	// State is the Runner's state directory: owners.json, link-noise.key, audit.jsonl.
	State string
	// Endpoint is the Runner's endpoint.json, read for every request.
	Endpoint string
	// Route names what carries the stream ("ssh"), for the audit.
	Route string
	// Peer is what the route knows of the other end (SSH_CONNECTION), for the audit.
	Peer string
	// Machine and Release describe this computer and Lazybox version in message 2.
	Machine, Release string
	// RevokeCheck is how often a live session rechecks its key (default 250 ms).
	RevokeCheck time.Duration
	// SessionLimit ends a session after this long; the client re-handshakes
	// silently (default 12 h).
	SessionLimit time.Duration
	// Transport reaches the loopback Runner (default: a plain HTTP/1.1 transport).
	Transport http.RoundTripper
	// Log receives one line per refusal and session (stderr for `link serve`).
	Log io.Writer
}

// Serve is the Runner side of one link (REMOTE_RUNNERS §3.2): every route
// ends here. It runs the Noise responder, admits only an enrolled owner key
// that proves its identity, then serves HTTP/2 over the channel, proxying
// each Connect call to the loopback Runner with the owner bearer read on
// this host. It returns when the stream ends, the key is revoked, the session
// limit passes, or ctx ends.
func Serve(ctx context.Context, stream io.ReadWriteCloser, o ServeOptions) error {
	if o.RevokeCheck == 0 {
		o.RevokeCheck = 250 * time.Millisecond
	}
	if o.SessionLimit == 0 {
		o.SessionLimit = 12 * time.Hour
	}
	if o.Log == nil {
		o.Log = io.Discard
	}
	if o.Route == "" {
		o.Route = "stdio"
	}
	static, err := LoadOrCreateRunnerKey(o.State)
	if err != nil {
		_ = stream.Close()
		return err
	}
	owners := Owners{State: o.State}
	audit := Audit{State: o.State}
	up, upErr := ReadEndpoint(o.Endpoint)
	hello := RunnerHello{Version: 1, Machine: o.Machine, Release: o.Release}
	if upErr == nil {
		hello.RunnerID = up.RunnerID
	}
	conn, owner, client, err := serverHandshake(ctx, stream, static, owners, hello)
	if err != nil {
		_ = stream.Close()
		var refusal *Refusal
		if errors.As(err, &refusal) {
			fmt.Fprintf(o.Log, "lazybox-link: refused: %s: %s\n", refusal.Code, refusal.Reason)
			audit.Record(AuditEntry{Event: "link.refused", Route: o.Route, Peer: o.Peer, Detail: refusal.Code})
		} else {
			fmt.Fprintf(o.Log, "lazybox-link: handshake: %v\n", err)
		}
		return err
	}
	started := time.Now()
	audit.Record(AuditEntry{Event: "link.opened", KeyID: owner.KeyID, Name: owner.Name, Route: o.Route, Peer: o.Peer, Detail: client.Name})
	ctx, cancel := context.WithCancelCause(ctx)
	defer cancel(nil)
	channel, _ := Decode(owner.ChannelKey)
	go func() {
		ticker := time.NewTicker(o.RevokeCheck)
		defer ticker.Stop()
		limit := time.NewTimer(o.SessionLimit)
		defer limit.Stop()
		for {
			select {
			case <-ctx.Done():
				_ = conn.Close()
				return
			case <-conn.Done():
				cancel(nil)
				return
			case <-limit.C:
				cancel(errors.New("session limit reached"))
			case <-ticker.C:
				// A file being rewritten reads as an error: only a clean "not there" revokes.
				if _, still, err := owners.Lookup(channel); err == nil && !still {
					fmt.Fprintf(o.Log, "lazybox-link: refused: unknown-key: key %s was revoked\n", owner.KeyID)
					cancel(errors.New("key revoked"))
				}
			}
		}
	}()
	transport := o.Transport
	if transport == nil {
		transport = &http.Transport{Proxy: nil, DisableCompression: true, MaxIdleConnsPerHost: 16, IdleConnTimeout: 90 * time.Second}
	}
	server := &http2.Server{MaxConcurrentStreams: 256, MaxReadFrameSize: 1 << 20}
	server.ServeConn(conn, &http2.ServeConnOpts{Context: ctx, Handler: proxyHandler(o.Endpoint, transport), BaseConfig: &http.Server{}})
	_ = conn.Close()
	reason := context.Cause(ctx)
	detail := "closed"
	if reason != nil && !errors.Is(reason, context.Canceled) {
		detail = reason.Error()
	}
	audit.Record(AuditEntry{Event: "link.closed", KeyID: owner.KeyID, Name: owner.Name, Route: o.Route, Peer: o.Peer, Detail: fmt.Sprintf("%s after %s", detail, time.Since(started).Round(time.Second))})
	return nil
}

// proxyHandler forwards each Connect call to the loopback Runner, with the
// owner bearer from its endpoint file now. A request that carries its own
// Authorization is refused: the link is the credential, and nothing a client
// sends can stand in for (or probe) the bearer.
func proxyHandler(endpoint string, transport http.RoundTripper) http.Handler {
	var mu sync.Mutex
	proxies := map[string]*httputil.ReverseProxy{}
	proxyFor := func(origin *url.URL) *httputil.ReverseProxy {
		mu.Lock()
		defer mu.Unlock()
		if p := proxies[origin.String()]; p != nil {
			return p
		}
		p := &httputil.ReverseProxy{
			Rewrite: func(r *httputil.ProxyRequest) {
				r.SetURL(origin)
				r.Out.Host = origin.Host // the Runner's DNS-rebinding guard wants its own Host
			},
			Transport:     transport,
			FlushInterval: -1,
			ErrorHandler: func(w http.ResponseWriter, r *http.Request, err error) {
				connectError(w, http.StatusServiceUnavailable, "unavailable", "The Runner on that computer is not answering: "+err.Error())
			},
		}
		proxies[origin.String()] = p
		return p
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !strings.HasPrefix(r.URL.Path, RPCPath) {
			connectError(w, http.StatusNotFound, "unimplemented", "a link carries the Runner's RPCs only")
			return
		}
		if r.Header.Get("Authorization") != "" {
			connectError(w, http.StatusUnauthorized, "unauthenticated", "a link request carries no credential of its own")
			return
		}
		if reason, ok := hostLocal[strings.TrimPrefix(r.URL.Path, RPCPath)]; ok {
			connectError(w, http.StatusForbidden, "permission_denied", reason)
			return
		}
		up, err := ReadEndpoint(endpoint)
		if err != nil {
			connectError(w, http.StatusServiceUnavailable, "unavailable", "The Runner on that computer is not running")
			return
		}
		origin, _ := url.Parse(up.URL)
		r.Header.Set("Authorization", "Bearer "+up.Token)
		proxyFor(origin).ServeHTTP(w, r)
	})
}

// connectError answers in the Connect protocol's error shape, which every
// client maps to a code (unary and the start of a stream alike).
func connectError(w http.ResponseWriter, status int, code, message string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]string{"code": code, "message": message})
}

// AuditEntry is one line of `<state>/audit.jsonl` (REMOTE_RUNNERS §2.5).
type AuditEntry struct {
	At     time.Time `json:"at"`
	Event  string    `json:"event"`
	KeyID  string    `json:"keyId,omitempty"`
	Name   string    `json:"name,omitempty"`
	Route  string    `json:"route,omitempty"`
	Peer   string    `json:"peer,omitempty"`
	By     string    `json:"by,omitempty"`
	Detail string    `json:"detail,omitempty"`
}

// Audit appends to the Runner's audit log (0600), rotating it past 4 MiB.
type Audit struct{ State string }

const auditLimit = 4 << 20

func (a Audit) Record(e AuditEntry) {
	if a.State == "" {
		return
	}
	if e.At.IsZero() {
		e.At = time.Now().UTC()
	}
	line, err := json.Marshal(e)
	if err != nil {
		return
	}
	path := filepath.Join(a.State, "audit.jsonl")
	if info, err := os.Stat(path); err == nil && info.Size() > auditLimit {
		_ = os.Rename(path, path+".1")
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		return
	}
	_, _ = f.Write(append(line, '\n'))
	_ = f.Close()
}
