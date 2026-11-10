package telemetry

import (
	"context"
	"crypto/rand"
	"errors"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"
)

type Upstreams struct {
	Claude  string `json:"claude"`
	OpenAI  string `json:"openai"`
	ChatGPT string `json:"chatgpt"`
}
type route struct{ scope Scope }
type Gateway struct {
	mu        sync.RWMutex
	routes    map[string]route
	server    *http.Server
	client    *http.Client
	BaseURL   string
	upstreams Upstreams
	store     Store
	prices    Pricer
	parse     func(string, string) ResponseParser
	handlers  sync.WaitGroup
	closed    bool
}

func identifier() string { return rand.Text() }
func StartGateway(store Store, prices Pricer, upstreams Upstreams) (*Gateway, error) {
	if upstreams.Claude == "" {
		upstreams.Claude = "https://api.anthropic.com"
	}
	if upstreams.OpenAI == "" {
		upstreams.OpenAI = "https://api.openai.com/v1"
	}
	if upstreams.ChatGPT == "" {
		upstreams.ChatGPT = "https://chatgpt.com/backend-api/codex"
	}
	for _, raw := range []string{upstreams.Claude, upstreams.OpenAI, upstreams.ChatGPT} {
		u, err := url.Parse(raw)
		if err != nil || u.Host == "" || u.User != nil || u.RawQuery != "" || u.Fragment != "" {
			return nil, errors.New("invalid meter upstream")
		}
		if u.Scheme != "https" && !(u.Scheme == "http" && net.ParseIP(u.Hostname()).IsLoopback()) {
			return nil, errors.New("meter upstream must use HTTPS or loopback HTTP")
		}
	}
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.DisableCompression = true
	transport.Proxy = nil
	transport.ResponseHeaderTimeout = 2 * time.Minute
	g := &Gateway{routes: map[string]route{}, BaseURL: "http://" + l.Addr().String(), upstreams: upstreams, store: store, prices: prices, parse: func(p, c string) ResponseParser { return NewParser(p, c) }, client: &http.Client{Transport: transport, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}}
	g.server = &http.Server{Handler: g, ReadHeaderTimeout: 10 * time.Second, MaxHeaderBytes: 64 << 10}
	go func() { _ = g.server.Serve(l) }()
	return g, nil
}
func (g *Gateway) Register(scope Scope) (string, func(), error) {
	if !validScope(scope) {
		return "", nil, errors.New("invalid meter scope")
	}
	id := identifier()
	g.mu.Lock()
	defer g.mu.Unlock()
	if len(g.routes) >= 1024 {
		return "", nil, errors.New("meter route capacity exhausted")
	}
	g.routes[id] = route{scope}
	return g.BaseURL + "/" + id, func() { g.mu.Lock(); delete(g.routes, id); g.mu.Unlock() }, nil
}
func stripHop(h http.Header) {
	for _, v := range h.Values("Connection") {
		for _, k := range strings.Split(v, ",") {
			h.Del(strings.TrimSpace(k))
		}
	}
	for _, k := range []string{"Connection", "Proxy-Connection", "Keep-Alive", "Proxy-Authenticate", "Proxy-Authorization", "TE", "Trailer", "Transfer-Encoding", "Upgrade"} {
		h.Del(k)
	}
}
func (g *Gateway) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	g.mu.Lock()
	if g.closed {
		g.mu.Unlock()
		http.Error(w, "meter stopped", 503)
		return
	}
	g.handlers.Add(1)
	g.mu.Unlock()
	defer g.handlers.Done()
	parts := strings.SplitN(strings.TrimPrefix(r.URL.Path, "/"), "/", 2)
	if len(parts) != 2 {
		http.NotFound(w, r)
		return
	}
	g.mu.RLock()
	route, ok := g.routes[parts[0]]
	g.mu.RUnlock()
	if !ok {
		http.NotFound(w, r)
		return
	}
	path := "/" + parts[1]
	provider := route.scope.Provider
	if r.Method != "POST" || !((provider == "claude" && (path == "/v1/messages" || path == "/v1/messages/count_tokens")) || (provider == "codex" && path == "/responses")) {
		http.Error(w, "unsupported meter endpoint", 404)
		return
	}
	base := g.upstreams.Claude
	if provider == "codex" {
		base = g.upstreams.OpenAI
		if r.Header.Get("chatgpt-account-id") != "" {
			base = g.upstreams.ChatGPT
		}
	}
	u, _ := url.Parse(strings.TrimRight(base, "/") + path)
	u.RawQuery = r.URL.RawQuery
	forward := r.Clone(r.Context())
	forward.URL = u
	forward.Host = u.Host
	forward.RequestURI = ""
	forward.Header = r.Header.Clone()
	stripHop(forward.Header)
	forward.Header.Set("Accept-Encoding", "identity")
	billable := path != "/v1/messages/count_tokens"
	record := Record{ID: identifier(), Scope: route.scope, Source: "gateway", Status: "pending"}
	if billable {
		if err := g.store.Put(context.Background(), record); err != nil {
			http.Error(w, "meter could not persist request", 503)
			return
		}
	}
	response, err := g.client.Do(forward)
	if err != nil {
		if billable {
			record.Status = "failed"
			record.Reason = "Upstream request failed"
			_ = g.store.Put(context.Background(), record)
		}
		http.Error(w, "meter upstream unavailable", 502)
		return
	}
	defer response.Body.Close()
	if response.StatusCode >= 300 && response.StatusCode < 400 {
		if billable {
			record.Status = "failed"
			record.Reason = "Upstream redirect refused"
			_ = g.store.Put(context.Background(), record)
		}
		http.Error(w, "meter upstream redirect refused", 502)
		return
	}
	headers := response.Header.Clone()
	stripHop(headers)
	for k, v := range headers {
		w.Header()[k] = v
	}
	w.WriteHeader(response.StatusCode)
	parser := g.parse(provider, response.Header.Get("Content-Type"))
	published := false
	publish := func(clean bool) {
		if published || !billable {
			return
		}
		parsed := parser.Finish(clean)
		parsed.ID = record.ID
		parsed.Scope = record.Scope
		parsed.Source = "gateway"
		if response.StatusCode >= 400 {
			parsed.Status = "failed"
			parsed.Reason = "Provider returned an error"
		}
		parsed = g.prices.Price(parsed)
		if err := g.store.Put(context.Background(), parsed); err == nil {
			published = true
		}
	}
	defer func() { publish(false) }()
	buffer := make([]byte, 32<<10)
	for {
		n, err := response.Body.Read(buffer)
		if n > 0 {
			parser.Push(buffer[:n])
			if parser.Terminal() {
				publish(true)
			}
			if _, writeErr := w.Write(buffer[:n]); writeErr != nil {
				return
			}
			if f, ok := w.(http.Flusher); ok {
				f.Flush()
			}
		}
		if err != nil {
			publish(err == io.EOF)
			return
		}
	}
}
func (g *Gateway) Close() error {
	g.mu.Lock()
	g.closed = true
	g.mu.Unlock()
	err := g.server.Close()
	g.handlers.Wait()
	g.client.CloseIdleConnections()
	return err
}
