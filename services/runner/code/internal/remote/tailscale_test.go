package remote

import (
	"context"
	"errors"
	"strings"
	"testing"
)

// fakeTailscale answers the CLI from fixed status and serve configurations
// and records every command; nothing touches a real tailnet.
type fakeTailscale struct {
	status, serve string
	missing       bool
	calls         []string
}

func (f *fakeTailscale) run(_ context.Context, args ...string) ([]byte, error) {
	f.calls = append(f.calls, strings.Join(args, " "))
	if f.missing {
		return nil, errNotInstalled
	}
	switch strings.Join(args, " ") {
	case "status --json":
		return []byte(f.status), nil
	case "serve status --json":
		return []byte(f.serve), nil
	}
	return nil, nil
}

func (f *fakeTailscale) writes() []string {
	var out []string
	for _, c := range f.calls {
		if !strings.HasSuffix(c, "--json") {
			out = append(out, c)
		}
	}
	return out
}

const runningStatus = `{"BackendState":"Running","Self":{"DNSName":"laptop.example.ts.net."},"CertDomains":["laptop.example.ts.net"]}`

func TestTailscaleStatesExplainWhatToDo(t *testing.T) {
	for _, c := range []struct {
		name string
		fake fakeTailscale
		want string
	}{
		{"missing", fakeTailscale{missing: true}, TailscaleNotInstalled},
		{"logged out", fakeTailscale{status: `{"BackendState":"NeedsLogin"}`}, TailscaleNeedsLogin},
		{"stopped", fakeTailscale{status: `{"BackendState":"Stopped"}`}, TailscaleNotRunning},
		{"no https", fakeTailscale{status: `{"BackendState":"Running","Self":{"DNSName":"laptop.example.ts.net."}}`}, TailscaleHTTPSDisabled},
		{"available", fakeTailscale{status: runningStatus, serve: `{}`}, TailscaleAvailable},
		{"serving", fakeTailscale{status: runningStatus, serve: `{"TCP":{"8443":{"HTTPS":true}},"Web":{"laptop.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:4100"}}}}}`}, TailscaleServing},
		{"taken", fakeTailscale{status: runningStatus, serve: `{"TCP":{"8443":{"HTTPS":true}},"Web":{"laptop.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}}}}`}, TailscaleConflict},
		{"funnel", fakeTailscale{status: runningStatus, serve: `{"TCP":{"8443":{"HTTPS":true}},"Web":{"laptop.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:4100"}}}},"AllowFunnel":{"laptop.example.ts.net:8443":true}}`}, TailscaleConflict},
	} {
		f := c.fake
		got := Tailscale{Run: f.run}.Status(context.Background(), 8443, "http://127.0.0.1:4100")
		if got.State != c.want {
			t.Errorf("%s: state %q, want %q (%s)", c.name, got.State, c.want, got.Detail)
		}
		if got.State != TailscaleAvailable && got.State != TailscaleServing && got.Detail == "" {
			t.Errorf("%s: no explanation", c.name)
		}
	}
}

// The route is HTTPS on the tailnet only, to the gateway on loopback: never
// plain HTTP, never Funnel, never another address.
func TestTailscaleServesOnlyHTTPSToLoopback(t *testing.T) {
	f := &fakeTailscale{status: runningStatus, serve: `{}`}
	ts := Tailscale{Run: f.run}
	if _, err := ts.Serve(context.Background(), 8443, "http://127.0.0.1:4100", ""); err != nil {
		t.Fatal(err)
	}
	if got := f.writes(); len(got) != 1 || got[0] != "serve --bg --https=8443 http://127.0.0.1:4100" {
		t.Fatalf("tailscale commands %q", got)
	}
	for _, target := range []string{"http://0.0.0.0:4100", "http://192.168.1.4:4100", "https://127.0.0.1:4100", "http://127.0.0.1:4100/path", "tcp://127.0.0.1:4100"} {
		f.calls = nil
		if _, err := ts.Serve(context.Background(), 8443, target, ""); err == nil || len(f.writes()) != 0 {
			t.Errorf("served %s: %v %q", target, err, f.writes())
		}
	}
	for _, c := range f.calls {
		if strings.Contains(c, "funnel") || strings.Contains(c, "--http=") {
			t.Fatalf("published beyond the tailnet: %s", c)
		}
	}
}

func TestTailscaleNeverReplacesAnotherService(t *testing.T) {
	other := `{"TCP":{"8443":{"HTTPS":true}},"Web":{"laptop.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}}}}`
	f := &fakeTailscale{status: runningStatus, serve: other}
	ts := Tailscale{Run: f.run}
	status, err := ts.Serve(context.Background(), 8443, "http://127.0.0.1:4100", "")
	if err == nil || status.State != TailscaleConflict || !strings.Contains(status.Detail, "http://127.0.0.1:3000") {
		t.Fatalf("replaced another service: %+v %v", status, err)
	}
	if err = ts.Unserve(context.Background(), 8443, "http://127.0.0.1:4100", ""); err != nil {
		t.Fatal(err)
	}
	if len(f.writes()) != 0 {
		t.Fatalf("touched another service: %q", f.writes())
	}
	// Its own earlier route (a Runner that came back on another port) is replaced.
	f.calls = nil
	if _, err = ts.Serve(context.Background(), 8443, "http://127.0.0.1:4100", "http://127.0.0.1:3000"); err != nil {
		t.Fatal(err)
	}
	if got := f.writes(); len(got) != 1 || got[0] != "serve --bg --https=8443 http://127.0.0.1:4100" {
		t.Fatalf("tailscale commands %q", got)
	}
	// A Funnel on its own earlier route is still refused.
	f.serve = `{"TCP":{"8443":{"HTTPS":true}},"Web":{"laptop.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}}},"AllowFunnel":{"laptop.example.ts.net:8443":true}}`
	f.calls = nil
	if _, err = ts.Serve(context.Background(), 8443, "http://127.0.0.1:4100", "http://127.0.0.1:3000"); err == nil || len(f.writes()) != 0 {
		t.Fatalf("served over a Funnel: %v %q", err, f.writes())
	}
}

func TestTailscaleUnservesOnlyItsOwnRoute(t *testing.T) {
	f := &fakeTailscale{status: runningStatus, serve: `{"TCP":{"8443":{"HTTPS":true}},"Web":{"laptop.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:4100"}}}}}`}
	if err := (Tailscale{Run: f.run}).Unserve(context.Background(), 8443, "http://127.0.0.1:4100", ""); err != nil {
		t.Fatal(err)
	}
	if got := f.writes(); len(got) != 1 || got[0] != "serve --https=8443 off" {
		t.Fatalf("tailscale commands %q", got)
	}
}

func TestTailscaleFailuresAreReported(t *testing.T) {
	ts := Tailscale{Run: func(_ context.Context, args ...string) ([]byte, error) {
		if args[0] == "status" {
			return []byte(runningStatus), nil
		}
		if len(args) > 1 && args[1] == "status" {
			return []byte(`{}`), nil
		}
		return nil, errors.New("tailscale serve: access denied")
	}}
	status, err := ts.Serve(context.Background(), 8443, "http://127.0.0.1:4100", "")
	if err == nil || status.State != TailscaleFailed || !strings.Contains(status.Detail, "access denied") {
		t.Fatalf("%+v %v", status, err)
	}
}
