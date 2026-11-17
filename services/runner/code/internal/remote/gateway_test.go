package remote

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices/devicetest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/transport"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

// stubRunner answers two reads and one write; it stands in for the Runner so
// these tests exercise the gateway, not execution.
type stubRunner struct {
	pb.UnimplementedRunnerServiceServer
	applied []*pb.ApplyRequest
}

func (s *stubRunner) ListCollections(context.Context, *pb.ListCollectionsRequest) (*pb.ListCollectionsResponse, error) {
	return &pb.ListCollectionsResponse{Collections: []*pb.Collection{{Id: "alpha", Name: "Alpha"}}}, nil
}
func (s *stubRunner) Apply(_ context.Context, r *pb.ApplyRequest) (*pb.Receipt, error) {
	s.applied = append(s.applied, r)
	return &pb.Receipt{OperationId: r.OperationId, Delivery: "accepted"}, nil
}

type gatewayFixture struct {
	gateway *Gateway
	devices *devices.Manager
	runner  *stubRunner
	server  *httptest.Server
	web     string
}

func newGateway(t *testing.T) *gatewayFixture {
	t.Helper()
	state := t.TempDir()
	m, err := devices.Open(state)
	if err != nil {
		t.Fatal(err)
	}
	r := &stubRunner{}
	handler, stop, err := transport.DeviceHandler(r, m, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(stop)
	g, err := New(state, m, handler)
	if err != nil {
		t.Fatal(err)
	}
	web := t.TempDir()
	if err = os.MkdirAll(filepath.Join(web, "assets"), 0o755); err != nil {
		t.Fatal(err)
	}
	_ = os.WriteFile(filepath.Join(web, "index.html"), []byte("<!doctype html><title>Lazybox</title>"), 0o644)
	_ = os.WriteFile(filepath.Join(web, "assets", "main.js"), []byte("console.log(1)"), 0o644)
	g.WebDir = web
	server := httptest.NewServer(g)
	t.Cleanup(server.Close)
	return &gatewayFixture{gateway: g, devices: m, runner: r, server: server, web: web}
}

func (f *gatewayFixture) post(t *testing.T, path string, body any) (int, map[string]any) {
	t.Helper()
	data, _ := json.Marshal(body)
	res, err := f.server.Client().Post(f.server.URL+path, "application/json", bytes.NewReader(data))
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	var out map[string]any
	_ = json.NewDecoder(res.Body).Decode(&out)
	return res.StatusCode, out
}

// signIn is what the phone app does after pairing: challenge, sign, session.
func (f *gatewayFixture) signIn(t *testing.T, p *devicetest.Phone, device string) (int, map[string]any) {
	t.Helper()
	code, challenge := f.post(t, "/remote/v1/challenge", map[string]string{"device": device})
	if code != http.StatusOK {
		return code, challenge
	}
	value := challenge["challenge"].(string)
	return f.post(t, "/remote/v1/session", map[string]string{"device": device, "challenge": value, "signature": p.Answer(device, value)})
}

// The whole phone journey over HTTP: pair with the QR code's secret, sign in
// with the device key, call the Runner, and be signed out by revocation.
func TestPhonePairsSignsInAndIsRevokedOverTheGateway(t *testing.T) {
	f := newGateway(t)
	p, err := devicetest.New()
	if err != nil {
		t.Fatal(err)
	}
	offer, secret, err := f.devices.Begin([]string{devices.GrantRespond})
	if err != nil {
		t.Fatal(err)
	}
	code, paired := f.post(t, "/remote/v1/pair", p.PairRequest(offer.ID, secret, "iPhone"))
	if code != http.StatusOK {
		t.Fatalf("pair: %d %v", code, paired)
	}
	device := paired["device"].(map[string]any)
	id := device["id"].(string)
	if device["name"] != "iPhone" || device["fingerprint"] == "" {
		t.Fatalf("paired %v", device)
	}
	// The QR code is spent.
	if code, body := f.post(t, "/remote/v1/pair", p.PairRequest(offer.ID, secret, "iPhone")); code != http.StatusForbidden {
		t.Fatalf("replayed pairing: %d %v", code, body)
	}
	code, session := f.signIn(t, p, id)
	if code != http.StatusOK {
		t.Fatalf("session: %d %v", code, session)
	}
	token := session["token"].(string)
	client := genconnect.NewRunnerServiceClient(f.server.Client(), f.server.URL)
	list := connect.NewRequest(&pb.ListCollectionsRequest{})
	list.Header().Set("Authorization", "Bearer "+token)
	if res, err := client.ListCollections(context.Background(), list); err != nil || len(res.Msg.Collections) != 1 {
		t.Fatalf("list: %v", err)
	}
	send := connect.NewRequest(&pb.ApplyRequest{SessionId: "s", OperationId: "o", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "hi"}}})
	send.Header().Set("Authorization", "Bearer "+token)
	if _, err = client.Apply(context.Background(), send); err != nil || len(f.runner.applied) != 1 {
		t.Fatalf("send: %v", err)
	}
	// Another phone's key cannot sign in as this device.
	other, _ := devicetest.New()
	if code, body := f.signIn(t, other, id); code != http.StatusUnauthorized {
		t.Fatalf("other key signed in: %d %v", code, body)
	}
	if err = f.devices.Revoke(id); err != nil {
		t.Fatal(err)
	}
	list = connect.NewRequest(&pb.ListCollectionsRequest{})
	list.Header().Set("Authorization", "Bearer "+token)
	if _, err = client.ListCollections(context.Background(), list); connect.CodeOf(err) != connect.CodeUnauthenticated {
		t.Fatalf("revoked credential: %v", err)
	}
	// The phone learns it was signed out, so it can say so and forget its key.
	if code, body := f.signIn(t, p, id); code != http.StatusUnauthorized || body["code"] != "revoked" {
		t.Fatalf("revoked sign-in: %d %v", code, body)
	}
}

func TestGatewayAcceptsOnlySmallJSONPosts(t *testing.T) {
	f := newGateway(t)
	res, err := f.server.Client().Get(f.server.URL + "/remote/v1/challenge")
	if err != nil {
		t.Fatal(err)
	}
	res.Body.Close()
	if res.StatusCode != http.StatusMethodNotAllowed {
		t.Fatalf("GET: %d", res.StatusCode)
	}
	res, err = f.server.Client().Post(f.server.URL+"/remote/v1/challenge", "application/x-www-form-urlencoded", strings.NewReader("device=x"))
	if err != nil {
		t.Fatal(err)
	}
	res.Body.Close()
	if res.StatusCode != http.StatusUnsupportedMediaType {
		t.Fatalf("form post: %d", res.StatusCode)
	}
	if code, _ := f.post(t, "/remote/v1/pair", map[string]string{"pairing": strings.Repeat("x", 20<<10)}); code != http.StatusBadRequest {
		t.Fatalf("oversized body: %d", code)
	}
	if code, _ := f.post(t, "/remote/v1/challenge", map[string]string{"device": "x", "owner": "y"}); code != http.StatusBadRequest {
		t.Fatalf("unknown field: %d", code)
	}
	if code, _ := f.post(t, "/remote/v1/elsewhere", map[string]string{}); code != http.StatusNotFound {
		t.Fatalf("unknown endpoint: %d", code)
	}
}

// What a browser is told about every response: own origin only, no framing,
// no referrer; and nothing that arrived through Funnel is answered.
func TestGatewayHardensEveryResponse(t *testing.T) {
	f := newGateway(t)
	for _, path := range []string{"/", "/assets/main.js", "/remote/v1/challenge", "/lazybox.bench.v1.RunnerService/ListCollections"} {
		res, err := f.server.Client().Get(f.server.URL + path)
		if err != nil {
			t.Fatal(err)
		}
		res.Body.Close()
		if !strings.Contains(res.Header.Get("Content-Security-Policy"), "frame-ancestors 'none'") || !strings.Contains(res.Header.Get("Content-Security-Policy"), "script-src 'self';") || res.Header.Get("Referrer-Policy") != "no-referrer" || res.Header.Get("X-Content-Type-Options") != "nosniff" {
			t.Errorf("%s: headers %v", path, res.Header)
		}
		if res.Header.Get("Access-Control-Allow-Origin") != "" {
			t.Errorf("%s: answers cross-origin requests", path)
		}
		req, _ := http.NewRequest(http.MethodGet, f.server.URL+path, nil)
		req.Header.Set("Tailscale-Funnel-Request", "?1")
		res, err = f.server.Client().Do(req)
		if err != nil {
			t.Fatal(err)
		}
		res.Body.Close()
		if res.StatusCode != http.StatusForbidden {
			t.Errorf("%s through Funnel: %d", path, res.StatusCode)
		}
	}
}

func TestGatewayServesOnlyTheBuiltApp(t *testing.T) {
	f := newGateway(t)
	secret := filepath.Join(filepath.Dir(f.web), "devices.json")
	_ = os.WriteFile(secret, []byte("secret"), 0o600)
	for path, want := range map[string]int{"/": 200, "/assets/main.js": 200, "/../devices.json": 404, "/%2e%2e/devices.json": 404} {
		res, err := f.server.Client().Get(f.server.URL + path)
		if err != nil {
			t.Fatal(err)
		}
		body, _ := io.ReadAll(res.Body)
		res.Body.Close()
		if res.StatusCode != want && !(want == 404 && res.StatusCode >= 300 && res.StatusCode < 500) {
			t.Errorf("%s: %d", path, res.StatusCode)
		}
		if strings.Contains(string(body), "secret") {
			t.Errorf("%s served a file outside the app", path)
		}
	}
	res, err := f.server.Client().Post(f.server.URL+"/", "text/plain", strings.NewReader("x"))
	if err != nil {
		t.Fatal(err)
	}
	res.Body.Close()
	if res.StatusCode != http.StatusMethodNotAllowed {
		t.Fatalf("POST /: %d", res.StatusCode)
	}
	// Without a build, the phone sees how to fix it rather than a blank page.
	f.gateway.WebDir = ""
	res, err = f.server.Client().Get(f.server.URL + "/")
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(res.Body)
	res.Body.Close()
	if res.StatusCode != http.StatusServiceUnavailable || !strings.Contains(string(body), "make build-web") {
		t.Fatalf("missing app: %d %s", res.StatusCode, body)
	}
}

// The service worker and the manifest are revalidated on every start, so a new
// release's worker replaces the old one at once; hashed assets are immutable.
func TestGatewayRevalidatesTheServiceWorker(t *testing.T) {
	f := newGateway(t)
	_ = os.WriteFile(filepath.Join(f.web, "sw.js"), []byte("self.addEventListener('push', () => {})"), 0o644)
	_ = os.WriteFile(filepath.Join(f.web, "manifest.webmanifest"), []byte("{}"), 0o644)
	for path, want := range map[string]string{"/sw.js": "no-cache", "/manifest.webmanifest": "no-cache", "/": "no-cache", "/assets/main.js": "public, max-age=31536000, immutable"} {
		res, err := f.server.Client().Get(f.server.URL + path)
		if err != nil {
			t.Fatal(err)
		}
		res.Body.Close()
		if res.StatusCode != http.StatusOK || res.Header.Get("Cache-Control") != want {
			t.Errorf("%s: %d, Cache-Control %q, want %q", path, res.StatusCode, res.Header.Get("Cache-Control"), want)
		}
	}
}

// The gateway binds loopback only, and the same port again after a restart
// so a Tailscale route made before still reaches it.
func TestGatewayListensOnLoopbackAndKeepsItsPort(t *testing.T) {
	state := t.TempDir()
	m, err := devices.Open(state)
	if err != nil {
		t.Fatal(err)
	}
	g, err := New(state, m, nil)
	if err != nil {
		t.Fatal(err)
	}
	g.Tailscale = Tailscale{Run: (&fakeTailscale{missing: true}).run}
	if err = g.Listen(context.Background()); err != nil {
		t.Fatal(err)
	}
	first := g.LocalURL()
	host, port, _ := net.SplitHostPort(strings.TrimPrefix(first, "http://"))
	if !net.ParseIP(host).IsLoopback() || port == "" {
		t.Fatalf("gateway at %s", first)
	}
	access := g.Access(context.Background())
	if access.LocalUrl != first || access.PhoneUrl != "" || access.Tailscale.State != TailscaleNotInstalled || access.Tailscale.HttpsPort != DefaultHTTPSPort {
		t.Fatalf("access %+v", access)
	}
	if err = g.Close(); err != nil {
		t.Fatal(err)
	}
	again, err := New(state, m, nil)
	if err != nil {
		t.Fatal(err)
	}
	again.Tailscale = g.Tailscale
	if err = again.Listen(context.Background()); err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	if again.LocalURL() != first {
		t.Fatalf("restarted on %s, was %s", again.LocalURL(), first)
	}
}

// Turning remote access on serves the gateway's own port over the tailnet,
// gives the phone an HTTPS URL, and is restored after a restart.
func TestRemoteAccessTurnsOnAndIsRestored(t *testing.T) {
	state := t.TempDir()
	m, _ := devices.Open(state)
	g, _ := New(state, m, nil)
	fake := &fakeTailscale{status: runningStatus, serve: `{}`}
	g.Tailscale = Tailscale{Run: fake.run}
	g.WebDir = newGateway(t).web
	if err := g.Listen(context.Background()); err != nil {
		t.Fatal(err)
	}
	local := g.LocalURL()
	fake.calls = nil
	if _, err := g.SetTailscale(context.Background(), true); err != nil {
		t.Fatal(err)
	}
	if got := fake.writes(); len(got) != 1 || got[0] != "serve --bg --https=8443 "+local {
		t.Fatalf("commands %q", got)
	}
	fake.serve = `{"TCP":{"8443":{"HTTPS":true}},"Web":{"laptop.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"` + local + `"}}}}}`
	access := g.Access(context.Background())
	if access.PhoneUrl != "https://laptop.example.ts.net:8443" || !access.Tailscale.Enabled || access.Problem != "" {
		t.Fatalf("access %+v", access)
	}
	_ = g.Close()
	again, _ := New(state, m, nil)
	again.Tailscale = g.Tailscale
	fake.calls = nil
	if err := again.Listen(context.Background()); err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	if !again.Access(context.Background()).Tailscale.Enabled {
		t.Fatal("remote access not restored")
	}
	fake.calls = nil
	if _, err := again.SetTailscale(context.Background(), false); err != nil {
		t.Fatal(err)
	}
	if got := fake.writes(); len(got) != 1 || got[0] != "serve --https=8443 off" {
		t.Fatalf("commands %q", got)
	}
}
