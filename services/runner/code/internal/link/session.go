package link

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"time"

	"golang.org/x/net/http2"
)

// Session is one live link from a client to a Runner: the encrypted channel
// and an HTTP/2 connection inside it.
type Session struct {
	Runner RunnerHello
	Ready  Ready
	conn   *Conn
	cc     *http2.ClientConn
}

// Dial opens a route, runs the handshake against the pinned Runner key and
// starts HTTP/2. A refusal says why in the host's words when the route can
// tell (ssh's stderr carries `link serve`'s reason).
func Dial(ctx context.Context, route Route, keys ClientKeys, runnerKey []byte, clientName string) (*Session, error) {
	stream, err := route.Dial(ctx)
	if err != nil {
		return nil, err
	}
	conn, runner, ready, err := ClientHandshake(ctx, stream, keys, runnerKey, route.Kind(), clientName)
	if err != nil {
		_ = stream.Close()
		var refusal *Refusal
		if d, ok := stream.(diagnoser); ok {
			diagnosis := d.Diagnosis()
			if host := refusalFrom(diagnosis); host != nil {
				return nil, host
			}
			if !errors.As(err, &refusal) && diagnosis != "" {
				return nil, fmt.Errorf("%w: %s", ErrRoute, Explain(route.Describe().Host, diagnosis, err))
			}
		}
		return nil, err
	}
	transport := &http2.Transport{AllowHTTP: true, ReadIdleTimeout: 15 * time.Second, PingTimeout: 10 * time.Second, DisableCompression: true}
	cc, err := transport.NewClientConn(conn)
	if err != nil {
		_ = conn.Close()
		return nil, err
	}
	return &Session{Runner: runner, Ready: ready, conn: conn, cc: cc}, nil
}

// ErrRoute is a route that failed before the Runner could answer (ssh could
// not connect, the host has no Lazybox).
var ErrRoute = errors.New("the route failed")

// RoundTrip sends one request to the Runner over the link.
func (s *Session) RoundTrip(r *http.Request) (*http.Response, error) { return s.cc.RoundTrip(r) }

// Ping measures the round trip to the Runner's end of the link.
func (s *Session) Ping(ctx context.Context) (time.Duration, error) {
	start := time.Now()
	if err := s.cc.Ping(ctx); err != nil {
		return 0, err
	}
	return time.Since(start), nil
}

// Alive reports whether the session can still take requests.
func (s *Session) Alive() bool {
	select {
	case <-s.conn.Done():
		return false
	default:
	}
	return s.cc.CanTakeNewRequest()
}

// Done is closed when the channel underneath closes.
func (s *Session) Done() <-chan struct{} { return s.conn.Done() }

// Close ends the session and the route under it.
func (s *Session) Close() error {
	_ = s.cc.Close()
	return s.conn.Close()
}

// HTTPClient is an *http.Client whose requests go over this session.
func (s *Session) HTTPClient() *http.Client {
	return &http.Client{Transport: roundTripper(s.RoundTrip)}
}

type roundTripper func(*http.Request) (*http.Response, error)

func (f roundTripper) RoundTrip(r *http.Request) (*http.Response, error) { return f(r) }

var _ io.Closer = (*Session)(nil)
