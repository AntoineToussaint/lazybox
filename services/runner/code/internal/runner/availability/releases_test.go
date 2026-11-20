package availability

import (
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestOfficialReleaseParsing(t *testing.T) {
	for _, test := range []struct{ body, want string }{
		{`{"tag_name":"rust-v0.171.0"}`, "0.171.0"},
		{`{"tag_name":"v2.1.420"}`, "2.1.420"},
		{`{"tag_name":"v2.1.420","prerelease":true}`, ""},
		{`{"tag_name":"rust-v0.172.0-alpha.1"}`, ""},
	} {
		server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
			_, _ = io.WriteString(w, test.body)
		}))
		got, err := checkRelease(context.Background(), server.Client(), server.URL)
		server.Close()
		if got != test.want || (err != nil) != (test.want == "") {
			t.Errorf("%s: got %q (%v), want %q", test.body, got, err, test.want)
		}
	}
}

type releaseTransport func(*http.Request) (*http.Response, error)

func (f releaseTransport) RoundTrip(r *http.Request) (*http.Response, error) { return f(r) }

func TestReleaseChecksAreCachedPerProvider(t *testing.T) {
	var requests int
	cache := ReleaseCache{client: &http.Client{Transport: releaseTransport(func(r *http.Request) (*http.Response, error) {
		requests++
		return &http.Response{StatusCode: http.StatusOK, Body: io.NopCloser(strings.NewReader(`{"tag_name":"v2.1.420"}`))}, nil
	})}}
	for _, provider := range []string{"claude", "claude", "codex", "codex"} {
		if got := cache.Latest(context.Background(), provider); got != "2.1.420" {
			t.Fatalf("latest %s = %q", provider, got)
		}
	}
	if got := cache.Latest(context.Background(), "acp:fixture"); got != "" || requests != 2 {
		t.Fatalf("unknown provider returned %q, %d network requests total", got, requests)
	}
}
