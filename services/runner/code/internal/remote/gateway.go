// Package remote is the device gateway (REMOTE.md): the one listener a paired
// phone talks to. It binds loopback only, like every Runner listener; a phone
// reaches it through `tailscale serve`, which adds HTTPS and admits only the
// owner's tailnet. It serves the phone web app, the pairing and sign-in
// endpoints, and the Runner's RPC behind an interceptor that accepts paired
// device credentials alone: the owner's bearer is refused here, and a device
// credential is refused on the owner's listener.
package remote

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// DefaultHTTPSPort is the tailnet HTTPS port the gateway is served on unless
// the Runner configuration names another (remote.httpsPort). Not 443, which
// people commonly give their own services.
const DefaultHTTPSPort = 8443

const (
	rpcPrefix   = "/lazybox.bench.v1.RunnerService/"
	apiPrefix   = "/remote/v1/"
	maxBodySize = 16 << 10
)

// contentPolicy confines the phone app to its own origin: no inline or
// foreign script, no framing, no form posts, no referrer.
const contentPolicy = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; worker-src 'self' blob:; manifest-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'; object-src 'none'"

// Gateway is the device listener and the remote-access settings.
type Gateway struct {
	Devices *devices.Manager
	// RPC serves the Runner's Connect API with DeviceAuthority alone.
	RPC http.Handler
	// WebDir holds the built web app (apps/web/dist); "" serves a page that
	// says how to build it.
	WebDir    string
	Tailscale Tailscale
	HTTPSPort uint32

	path string
	mu   sync.Mutex
	// saved is what remote.json keeps across Runner restarts.
	saved struct {
		Port      int    `json:"port"`
		Tailscale bool   `json:"tailscale"`
		Served    string `json:"served,omitempty"`
	}
	listener net.Listener
	server   *http.Server
	problem  string

	// name is this machine's tailnet name, the one Host besides loopback the
	// gateway answers (allowedHost). It is learned from Tailscale's status
	// whenever the gateway reads it, and looked up again, at most once per
	// nameRefresh, when a request names another host.
	name struct {
		sync.Mutex
		value   string
		checked time.Time
	}
}

// nameRefresh bounds how often a request naming an unknown host makes the
// gateway ask Tailscale for this machine's name again.
const nameRefresh = 30 * time.Second

// New loads the gateway's settings from the Runner state directory.
func New(state string, manager *devices.Manager, rpc http.Handler) (*Gateway, error) {
	g := &Gateway{Devices: manager, RPC: rpc, HTTPSPort: DefaultHTTPSPort, path: filepath.Join(state, "remote.json")}
	data, err := os.ReadFile(g.path)
	if err == nil {
		if err = json.Unmarshal(data, &g.saved); err != nil {
			return nil, fmt.Errorf("remote access settings unreadable: %w", err)
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	return g, nil
}

// Listen binds the gateway on loopback, on the port it used before when that
// is free (so a Tailscale route made earlier still reaches it), and serves.
// If remote access was on, the route is restored to the bound port.
func (g *Gateway) Listen(ctx context.Context) error {
	var listener net.Listener
	var err error
	if g.saved.Port > 0 {
		listener, err = net.Listen("tcp", net.JoinHostPort("127.0.0.1", strconv.Itoa(g.saved.Port)))
	}
	if listener == nil {
		if listener, err = net.Listen("tcp", "127.0.0.1:0"); err != nil {
			return err
		}
	}
	if host, _, _ := net.SplitHostPort(listener.Addr().String()); !net.ParseIP(host).IsLoopback() {
		listener.Close()
		return fmt.Errorf("the device gateway must answer on loopback only, not %s", host)
	}
	g.mu.Lock()
	g.listener = listener
	g.server = &http.Server{Handler: g, ReadHeaderTimeout: 10 * time.Second, IdleTimeout: 2 * time.Minute}
	g.saved.Port = listener.Addr().(*net.TCPAddr).Port
	err = g.save()
	enabled := g.saved.Tailscale
	g.mu.Unlock()
	if err != nil {
		listener.Close()
		return err
	}
	go func() { _ = g.server.Serve(listener) }()
	if enabled {
		if _, err := g.SetTailscale(ctx, true); err != nil {
			g.mu.Lock()
			g.problem = "Tailscale access could not be restored: " + err.Error()
			g.mu.Unlock()
		}
	}
	return nil
}

// Close stops the gateway. The Tailscale route stays: the next Runner binds
// the same port, and a phone meanwhile sees the Runner as offline.
func (g *Gateway) Close() error {
	g.mu.Lock()
	server, listener := g.server, g.listener
	g.mu.Unlock()
	if server == nil {
		return nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	err := server.Shutdown(ctx)
	// Shutdown closes only listeners Serve has begun with; this one may not be yet.
	if closeErr := listener.Close(); closeErr != nil && !errors.Is(closeErr, net.ErrClosed) && err == nil {
		err = closeErr
	}
	return err
}

// LocalURL is the gateway on this machine.
func (g *Gateway) LocalURL() string {
	g.mu.Lock()
	defer g.mu.Unlock()
	if g.listener == nil {
		return ""
	}
	return "http://" + g.listener.Addr().String()
}

func (g *Gateway) save() error {
	data, err := json.Marshal(g.saved)
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(g.path), ".remote-*")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	if closeErr := f.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	return os.Rename(f.Name(), g.path)
}

func phoneURL(name string, port uint32) string {
	if port == 443 {
		return "https://" + name
	}
	return "https://" + name + ":" + strconv.FormatUint(uint64(port), 10)
}

// Access reports how a phone reaches this Runner now.
func (g *Gateway) Access(ctx context.Context) *pb.RemoteAccess {
	local := g.LocalURL()
	g.mu.Lock()
	enabled, problem := g.saved.Tailscale, g.problem
	g.mu.Unlock()
	status := g.Tailscale.Status(ctx, g.HTTPSPort, local)
	g.learnName(status.DNSName)
	out := &pb.RemoteAccess{LocalUrl: local, Problem: problem, Tailscale: &pb.TailscaleAccess{State: status.State, DnsName: status.DNSName, HttpsPort: g.HTTPSPort, Enabled: enabled, Detail: status.Detail}}
	if local == "" {
		out.Problem = "The device gateway is not running."
	} else if g.missingApp() {
		out.Problem = "The phone app is not built on this Runner. Run `make build-web`, then restart the Runner."
	}
	if status.State == TailscaleServing && enabled {
		out.PhoneUrl = phoneURL(status.DNSName, g.HTTPSPort)
	}
	return out
}

// SetTailscale turns the Tailscale route on or off and remembers the choice.
func (g *Gateway) SetTailscale(ctx context.Context, on bool) (*pb.RemoteAccess, error) {
	target := g.LocalURL()
	if target == "" {
		return nil, errors.New("the device gateway is not running")
	}
	g.mu.Lock()
	previous := g.saved.Served
	g.mu.Unlock()
	var err error
	if on {
		_, err = g.Tailscale.Serve(ctx, g.HTTPSPort, target, previous)
	} else {
		err = g.Tailscale.Unserve(ctx, g.HTTPSPort, target, previous)
	}
	g.mu.Lock()
	if err == nil {
		g.saved.Tailscale, g.problem = on, ""
		g.saved.Served = ""
		if on {
			g.saved.Served = target
		}
		err = g.save()
	}
	g.mu.Unlock()
	if err != nil {
		return nil, err
	}
	return g.Access(ctx), nil
}

func (g *Gateway) missingApp() bool {
	if g.WebDir == "" {
		return true
	}
	_, err := os.Stat(filepath.Join(g.WebDir, "index.html"))
	return err != nil
}

func (g *Gateway) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	h := w.Header()
	h.Set("Content-Security-Policy", contentPolicy)
	h.Set("Referrer-Policy", "no-referrer")
	h.Set("X-Content-Type-Options", "nosniff")
	h.Set("X-Frame-Options", "DENY")
	h.Set("Permissions-Policy", "camera=(), microphone=(), geolocation=()")
	// Funnel marks the requests it carries from the public internet; this
	// gateway is for the owner's tailnet only, whatever the route config says.
	if r.Header.Get("Tailscale-Funnel-Request") != "" {
		http.Error(w, "This Runner is not available from the public internet.", http.StatusForbidden)
		return
	}
	// DNS rebinding: a web page whose own name resolves to 127.0.0.1 reaches
	// this listener with that name as its Host.
	if !g.allowedHost(r.Context(), r.Host) {
		http.Error(w, "This Runner answers only on this computer and on its tailnet name.", http.StatusMisdirectedRequest)
		return
	}
	switch {
	case strings.HasPrefix(r.URL.Path, rpcPrefix):
		h.Set("Cache-Control", "no-store")
		if g.RPC == nil {
			http.NotFound(w, r)
			return
		}
		g.RPC.ServeHTTP(w, r)
	case strings.HasPrefix(r.URL.Path, apiPrefix):
		h.Set("Cache-Control", "no-store")
		g.api(w, r)
	default:
		g.static(w, r)
	}
}

// allowedHost admits a request addressed to loopback (this computer) or to
// this machine's tailnet name (`tailscale serve` keeps the phone's Host).
// Another name is looked up again at most once per nameRefresh, so a gateway
// that started before Tailscale connected, or a renamed machine, is still
// reached without reopening Settings.
func (g *Gateway) allowedHost(ctx context.Context, raw string) bool {
	host := raw
	if h, _, err := net.SplitHostPort(raw); err == nil {
		host = h
	}
	host = strings.TrimSuffix(strings.ToLower(strings.Trim(host, "[]")), ".")
	if host == "localhost" {
		return true
	}
	if ip := net.ParseIP(host); ip != nil || host == "" {
		return ip != nil && ip.IsLoopback()
	}
	g.name.Lock()
	known, stale := g.name.value, time.Since(g.name.checked) >= nameRefresh
	if host == known || !stale {
		g.name.Unlock()
		return host == known
	}
	g.name.checked = time.Now()
	g.name.Unlock()
	g.learnName(g.Tailscale.Status(ctx, g.HTTPSPort, g.LocalURL()).DNSName)
	g.name.Lock()
	defer g.name.Unlock()
	return host == g.name.value
}

// learnName records this machine's tailnet name from a Tailscale status.
func (g *Gateway) learnName(name string) {
	name = strings.TrimSuffix(strings.ToLower(name), ".")
	if name == "" {
		return
	}
	g.name.Lock()
	g.name.value = name
	g.name.Unlock()
}

func (g *Gateway) static(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	if g.missingApp() {
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		w.Header().Set("Cache-Control", "no-store")
		w.WriteHeader(http.StatusServiceUnavailable)
		_, _ = io.WriteString(w, `<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><title>Lazybox</title><body style="font:16px system-ui;margin:2rem;max-width:32rem"><h1>Lazybox is reachable</h1><p>The phone app is not built on this computer yet. On it, run <code>make build-web</code>, then restart the Runner.</p></body>`)
		return
	}
	// The service worker and the manifest are checked on every start, so a new
	// release's worker takes over at once (sw.js keeps no cache of the API).
	if r.URL.Path == "/" || r.URL.Path == "/index.html" || r.URL.Path == "/sw.js" || r.URL.Path == "/manifest.webmanifest" {
		w.Header().Set("Cache-Control", "no-cache")
	} else if strings.HasPrefix(r.URL.Path, "/assets/") {
		w.Header().Set("Cache-Control", "public, max-age=31536000, immutable")
	}
	http.FileServer(http.Dir(g.WebDir)).ServeHTTP(w, r)
}

type apiError struct {
	Error string `json:"error"`
	// Code is "revoked" when the device is no longer paired: the phone then
	// forgets its key and asks to pair again.
	Code string `json:"code,omitempty"`
}

func writeJSON(w http.ResponseWriter, code int, value any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(value)
}

func wireDevice(d devices.Device) map[string]any {
	return map[string]any{"id": d.ID, "name": d.Name, "grants": d.Grants, "fingerprint": d.Fingerprint()}
}

// api serves the three unauthenticated endpoints a phone needs before it has
// a credential: pair (with the QR code's one-time secret), challenge and
// session (with its device key). JSON only, POST only, small bodies only.
func (g *Gateway) api(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, apiError{Error: "POST only"})
		return
	}
	if !strings.HasPrefix(r.Header.Get("Content-Type"), "application/json") {
		writeJSON(w, http.StatusUnsupportedMediaType, apiError{Error: "JSON only"})
		return
	}
	if g.Devices == nil {
		writeJSON(w, http.StatusNotFound, apiError{Error: "this Runner does not pair devices"})
		return
	}
	body := http.MaxBytesReader(w, r.Body, maxBodySize)
	decoder := json.NewDecoder(body)
	decoder.DisallowUnknownFields()
	switch strings.TrimPrefix(r.URL.Path, apiPrefix) {
	case "pair":
		// Each offer bounds its own attempts (devices.Complete): one budget
		// shared by every request let anyone on the tailnet block pairing.
		var req devices.PairRequest
		if decoder.Decode(&req) != nil {
			writeJSON(w, http.StatusBadRequest, apiError{Error: "malformed pairing request"})
			return
		}
		d, err := g.Devices.Complete(req)
		if errors.Is(err, devices.ErrTooManyAttempts) {
			writeJSON(w, http.StatusTooManyRequests, apiError{Error: err.Error()})
			return
		}
		if err != nil {
			writeJSON(w, http.StatusForbidden, apiError{Error: err.Error()})
			return
		}
		writeJSON(w, http.StatusOK, map[string]any{"device": wireDevice(d)})
	case "challenge":
		var req struct {
			Device string `json:"device"`
		}
		if decoder.Decode(&req) != nil {
			writeJSON(w, http.StatusBadRequest, apiError{Error: "malformed request"})
			return
		}
		challenge, expires, err := g.Devices.Challenge(req.Device)
		if errors.Is(err, devices.ErrUnknownDevice) {
			writeJSON(w, http.StatusUnauthorized, apiError{Error: err.Error(), Code: "revoked"})
			return
		}
		if err != nil {
			writeJSON(w, http.StatusTooManyRequests, apiError{Error: err.Error()})
			return
		}
		writeJSON(w, http.StatusOK, map[string]any{"challenge": challenge, "expiresAt": expires.UnixMilli()})
	case "session":
		var req struct {
			Device    string `json:"device"`
			Challenge string `json:"challenge"`
			Signature string `json:"signature"`
		}
		if decoder.Decode(&req) != nil {
			writeJSON(w, http.StatusBadRequest, apiError{Error: "malformed request"})
			return
		}
		token, expires, d, err := g.Devices.Session(req.Device, req.Challenge, req.Signature)
		if errors.Is(err, devices.ErrUnknownDevice) {
			writeJSON(w, http.StatusUnauthorized, apiError{Error: err.Error(), Code: "revoked"})
			return
		}
		if err != nil {
			writeJSON(w, http.StatusUnauthorized, apiError{Error: err.Error()})
			return
		}
		writeJSON(w, http.StatusOK, map[string]any{"token": token, "expiresAt": expires.UnixMilli(), "device": wireDevice(d)})
	default:
		writeJSON(w, http.StatusNotFound, apiError{Error: "unknown endpoint"})
	}
}
