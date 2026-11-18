package link

import (
	"context"
	"crypto/rand"
	"crypto/subtle"
	"encoding/hex"
	"errors"
	"math"
	mrand "math/rand/v2"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"strings"
	"sync"
	"time"
)

// Status is how a link is doing, as the window's Runner header says it.
type Status struct {
	// State: "connecting" (first dial), "connected", "reconnecting",
	// "refused" (the Runner does not accept this client: revoked, or another
	// Runner answers), "stopped".
	State string `json:"state"`
	// Code is a refusal's kind: "unknown-key", "wrong-runner", "proof".
	Code    string `json:"code,omitempty"`
	Problem string `json:"problem,omitempty"`
	// RTTMillis is the last measured round trip to the Runner.
	RTTMillis int64     `json:"rttMs,omitempty"`
	Since     time.Time `json:"since"`
	Machine   string    `json:"machine,omitempty"`
	Release   string    `json:"release,omitempty"`
	RunnerID  string    `json:"runnerId,omitempty"`
}

// Agent is the client half of one link (the "link agent" of REMOTE_RUNNERS
// §3.1): it keeps a session to one remote Runner, redialling with backoff
// after a drop, a sleep or a network change, and serves that Runner on a
// loopback origin with a per-start token, so the window's RunnerLink and the
// CLI talk to it exactly as to a local Runner. The token is checked here and
// never forwarded; the Runner never sees a bearer from this computer.
type Agent struct {
	Route      Route
	Keys       ClientKeys
	RunnerKey  []byte
	ClientName string
	// Label names the Runner in errors ("build-box").
	Label string
	// OnStatus is told every change of Status (and every new RTT).
	OnStatus func(Status)
	// Ping is how often the RTT is measured and the link checked (default 10 s).
	Ping time.Duration

	token    string
	listener net.Listener
	mu       sync.Mutex
	session  *Session
	status   Status
	wake     chan struct{}
}

// Listen binds the loopback origin and makes the token; call before Run.
func (a *Agent) Listen() (url, token string, err error) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return "", "", err
	}
	raw := make([]byte, 32)
	if _, err = rand.Read(raw); err != nil {
		_ = l.Close()
		return "", "", err
	}
	a.listener, a.token = l, hex.EncodeToString(raw)
	a.wake = make(chan struct{}, 1)
	return "http://" + l.Addr().String(), a.token, nil
}

// Status is the link's state now.
func (a *Agent) Status() Status {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.status
}

func (a *Agent) setStatus(change func(*Status)) {
	a.mu.Lock()
	before := a.status
	change(&a.status)
	if a.status.State != before.State {
		a.status.Since = time.Now().UTC()
	}
	now := a.status
	a.mu.Unlock()
	if a.OnStatus != nil {
		a.OnStatus(now)
	}
}

func (a *Agent) current() *Session {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.session != nil && a.session.Alive() {
		return a.session
	}
	return nil
}

// Run serves the loopback origin and keeps the link up until ctx ends.
func (a *Agent) Run(ctx context.Context) error {
	if a.listener == nil {
		if _, _, err := a.Listen(); err != nil {
			return err
		}
	}
	if a.Ping == 0 {
		a.Ping = 10 * time.Second
	}
	server := &http.Server{Handler: a.handler(), ReadHeaderTimeout: 10 * time.Second}
	served := make(chan error, 1)
	go func() { served <- server.Serve(a.listener) }()
	go a.watchClock(ctx)
	a.setStatus(func(s *Status) { s.State = "connecting" })
	a.keep(ctx)
	shutdown, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	_ = server.Shutdown(shutdown)
	if s := a.current(); s != nil {
		_ = s.Close()
	}
	a.setStatus(func(s *Status) { s.State = "stopped"; s.Problem = "" })
	select {
	case err := <-served:
		if errors.Is(err, http.ErrServerClosed) {
			return nil
		}
		return err
	default:
		return nil
	}
}

// keep dials, holds the session while it lives, and redials after it ends.
func (a *Agent) keep(ctx context.Context) {
	attempt := 0
	for ctx.Err() == nil {
		session, err := Dial(ctx, a.Route, a.Keys, a.RunnerKey, a.ClientName)
		if err != nil {
			var refusal *Refusal
			wait := backoff(attempt)
			attempt++
			if errors.As(err, &refusal) {
				// Revoked, or another Runner answers: retrying fast changes
				// nothing, but a key enrolled again comes back by itself.
				wait = time.Minute
				a.setStatus(func(s *Status) { s.State = "refused"; s.Code = refusal.Code; s.Problem = refusalText(a.Label, refusal) })
			} else {
				a.setStatus(func(s *Status) {
					if s.State != "connecting" {
						s.State = "reconnecting"
					}
					s.Code, s.Problem = "", err.Error()
				})
			}
			if !a.sleep(ctx, wait) {
				return
			}
			continue
		}
		attempt = 0
		a.mu.Lock()
		a.session = session
		a.mu.Unlock()
		a.setStatus(func(s *Status) {
			s.State, s.Code, s.Problem = "connected", "", ""
			s.Machine, s.Release, s.RunnerID = session.Runner.Machine, session.Runner.Release, session.Runner.RunnerID
		})
		a.hold(ctx, session)
		_ = session.Close()
		a.mu.Lock()
		a.session = nil
		a.mu.Unlock()
		if ctx.Err() != nil {
			return
		}
		a.setStatus(func(s *Status) { s.State = "reconnecting"; s.Problem = "" })
	}
}

// hold measures the RTT while the session lives; a ping that fails, or a
// wake from sleep whose ping fails, ends it.
func (a *Agent) hold(ctx context.Context, session *Session) {
	ticker := time.NewTicker(a.Ping)
	defer ticker.Stop()
	measure := func(limit time.Duration) bool {
		pctx, cancel := context.WithTimeout(ctx, limit)
		defer cancel()
		rtt, err := session.Ping(pctx)
		if err != nil {
			return false
		}
		ms := max(int64(math.Round(float64(rtt)/float64(time.Millisecond))), 1)
		a.setStatus(func(s *Status) { s.RTTMillis = ms })
		return true
	}
	measure(5 * time.Second)
	for {
		select {
		case <-ctx.Done():
			return
		case <-session.Done():
			return
		case <-ticker.C:
			if !measure(10 * time.Second) {
				return
			}
		case <-a.wake:
			// After a sleep or a network change the old connection is often
			// dead without knowing it: a quick ping decides, not keepalives.
			if !measure(3 * time.Second) {
				return
			}
		}
	}
}

// watchClock notices a sleep: the wall clock jumps past what the ticker saw.
func (a *Agent) watchClock(ctx context.Context) {
	const step = 2 * time.Second
	ticker := time.NewTicker(step)
	defer ticker.Stop()
	last := time.Now().Round(0)
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			now := time.Now().Round(0) // wall clock: it keeps counting while asleep
			if now.Sub(last) > 4*step {
				a.Wake()
			}
			last = now
		}
	}
}

// Wake tells the agent the network may have changed (a wake from sleep, a new
// Wi-Fi): it rechecks the session now and redials at once if it is gone.
func (a *Agent) Wake() {
	select {
	case a.wake <- struct{}{}:
	default:
	}
}

func (a *Agent) sleep(ctx context.Context, d time.Duration) bool {
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-timer.C:
		return true
	case <-a.wake:
		return true
	}
}

// backoff: 250 ms doubling to 15 s, with ±20% jitter.
func backoff(attempt int) time.Duration {
	d := 250 * time.Millisecond << min(attempt, 6)
	d = min(d, 15*time.Second)
	jitter := 0.8 + 0.4*mrand.Float64()
	return time.Duration(float64(d) * jitter)
}

func refusalText(label string, r *Refusal) string {
	if label == "" {
		label = "That computer"
	}
	switch r.Code {
	case "unknown-key":
		return label + " no longer accepts this computer (its key was revoked there). Add it again to reconnect."
	case "wrong-runner":
		return "This is not " + label + "'s Runner: another Runner answered at that address. Nothing was sent."
	}
	return label + " refused this computer: " + r.Reason
}

// handler is the loopback origin: the token, a loopback Host, the RPC path;
// then the request goes over the link without the token.
func (a *Agent) handler() http.Handler {
	proxy := &httputil.ReverseProxy{
		Rewrite: func(r *httputil.ProxyRequest) {
			r.Out.URL.Scheme, r.Out.URL.Host, r.Out.Host = "http", "runner", "runner"
			r.Out.Header.Del("Authorization")
			r.Out.Header.Del("Cookie")
		},
		Transport:     roundTripper(a.roundTrip),
		FlushInterval: -1,
		ErrorHandler: func(w http.ResponseWriter, r *http.Request, err error) {
			status := a.Status()
			message := "Reconnecting to " + firstNonEmpty(a.Label, "the computer") + "…"
			if status.State == "refused" {
				connectError(w, http.StatusUnauthorized, "unauthenticated", status.Problem)
				return
			}
			if !errors.Is(err, errNoSession) && r.Context().Err() == nil {
				message = "The link to " + firstNonEmpty(a.Label, "the computer") + " dropped during this call; it is reconnecting."
			}
			connectError(w, http.StatusServiceUnavailable, "unavailable", message)
		},
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		host, _, err := net.SplitHostPort(r.Host)
		if err != nil || !loopbackHost(host) {
			// DNS rebinding: a page that resolves its own name to 127.0.0.1.
			http.Error(w, "not found", http.StatusNotFound)
			return
		}
		got, ok := strings.CutPrefix(r.Header.Get("Authorization"), "Bearer ")
		if !ok || subtle.ConstantTimeCompare([]byte(got), []byte(a.token)) != 1 {
			connectError(w, http.StatusUnauthorized, "unauthenticated", "this link needs its local token")
			return
		}
		if !strings.HasPrefix(r.URL.Path, RPCPath) {
			connectError(w, http.StatusNotFound, "unimplemented", "a link carries the Runner's RPCs only")
			return
		}
		proxy.ServeHTTP(w, r)
	})
}

func (a *Agent) roundTrip(r *http.Request) (*http.Response, error) {
	session := a.current()
	if session == nil {
		return nil, errNoSession
	}
	return session.RoundTrip(r)
}

// Origin is the loopback URL the agent serves on (after Listen).
func (a *Agent) Origin() *url.URL {
	if a.listener == nil {
		return nil
	}
	return &url.URL{Scheme: "http", Host: a.listener.Addr().String()}
}
