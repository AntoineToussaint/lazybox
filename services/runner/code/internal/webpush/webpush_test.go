package webpush_test

import (
	"bytes"
	"context"
	"crypto/ecdh"
	"encoding/base64"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/webpush/webpushtest"
)

func b64(t *testing.T, s string) []byte {
	t.Helper()
	b, err := base64.RawURLEncoding.DecodeString(s)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

// RFC 8291 Appendix A: the published example encrypts to exactly this body.
func TestEncryptMatchesRFC8291Example(t *testing.T) {
	asPrivate, err := ecdh.P256().NewPrivateKey(b64(t, "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"))
	if err != nil {
		t.Fatal(err)
	}
	if got := base64.RawURLEncoding.EncodeToString(asPrivate.PublicKey().Bytes()); got != "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8" {
		t.Fatalf("as_public = %s", got)
	}
	body, err := webpush.EncryptWith("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4", "BTBZMqHH6r4Tts7J_aSIgg",
		b64(t, "V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24"), asPrivate, b64(t, "DGv6ra1nlYgDCS1FRnbzlw"))
	if err != nil {
		t.Fatal(err)
	}
	want := "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
	if got := base64.RawURLEncoding.EncodeToString(body); got != want {
		t.Fatalf("body\n got %s\nwant %s", got, want)
	}
	// And the receiving side decrypts it.
	uaPrivate, err := ecdh.P256().NewPrivateKey(b64(t, "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94"))
	if err != nil {
		t.Fatal(err)
	}
	plain, err := (&webpushtest.Browser{Private: uaPrivate, Secret: b64(t, "BTBZMqHH6r4Tts7J_aSIgg")}).Decrypt(body)
	if err != nil || string(plain) != "When I grow up, I want to be a watermelon" {
		t.Fatalf("decrypt = %q, %v", plain, err)
	}
}

func TestEncryptRoundTripsWithFreshKeys(t *testing.T) {
	b := webpushtest.NewBrowser()
	one, err := webpush.Encrypt(b.P256dh(), b.Auth(), []byte(`{"kind":"test"}`))
	if err != nil {
		t.Fatal(err)
	}
	two, _ := webpush.Encrypt(b.P256dh(), b.Auth(), []byte(`{"kind":"test"}`))
	if bytes.Equal(one, two) {
		t.Fatal("two encryptions of the same payload must differ (fresh salt and key)")
	}
	plain, err := b.Decrypt(one)
	if err != nil || string(plain) != `{"kind":"test"}` {
		t.Fatalf("%q %v", plain, err)
	}
	if _, err = webpush.Encrypt(b.P256dh(), b.Auth(), make([]byte, webpush.MaxPayload+1)); err == nil {
		t.Fatal("an oversized payload must be refused")
	}
	if _, err = (&webpushtest.Browser{Private: webpushtest.NewBrowser().Private, Secret: b.Secret}).Decrypt(one); err == nil {
		t.Fatal("another browser's key must not decrypt it")
	}
}

func TestCheckKeys(t *testing.T) {
	b := webpushtest.NewBrowser()
	if err := webpush.CheckKeys(b.P256dh(), b.Auth()); err != nil {
		t.Fatal(err)
	}
	for _, c := range [][2]string{{"", b.Auth()}, {b.P256dh(), ""}, {b.P256dh()[:20], b.Auth()}, {b.P256dh(), b.Auth() + "AAAA"}, {"B" + strings.Repeat("A", 86), b.Auth()}} {
		if webpush.CheckKeys(c[0], c[1]) == nil {
			t.Fatalf("accepted %q %q", c[0], c[1])
		}
	}
}

func TestKeyIsCreatedOnceAndKept(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "vapid.key")
	k1, err := webpush.LoadOrCreateKey(path)
	if err != nil {
		t.Fatal(err)
	}
	info, err := os.Stat(path)
	if err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("key mode %v %v", info.Mode(), err)
	}
	if raw := b64(t, k1.PublicKey()); len(raw) != 65 || raw[0] != 4 {
		t.Fatalf("public key is not an uncompressed point: %d bytes", len(raw))
	}
	k2, err := webpush.LoadOrCreateKey(path)
	if err != nil || k2.PublicKey() != k1.PublicKey() {
		t.Fatal("the key must survive a restart")
	}
	if err = os.WriteFile(path, []byte("corrupt"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err = webpush.LoadOrCreateKey(path); err == nil {
		t.Fatal("a corrupt key must be refused, not replaced")
	}
}

func TestValidateEndpoint(t *testing.T) {
	ok := map[string]string{
		"https://web.push.apple.com/QAbc":                         "web.push.apple.com",
		"https://fcm.googleapis.com/fcm/send/abc":                 "fcm.googleapis.com",
		"https://updates.push.services.mozilla.com/wpush/v2/gAAA": "updates.push.services.mozilla.com",
		"https://wns2-par02p.notify.windows.com/w/?token=x":       "wns2-par02p.notify.windows.com",
		"https://FCM.googleapis.com:443/fcm/send/abc":             "fcm.googleapis.com",
	}
	for raw, host := range ok {
		got, err := webpush.ValidateEndpoint(raw)
		if err != nil || got != host {
			t.Fatalf("%s: %q %v", raw, got, err)
		}
	}
	for _, raw := range []string{
		"http://fcm.googleapis.com/fcm/send/abc",
		"https://fcm.googleapis.com:8443/x",
		"https://evilfcm.googleapis.com.attacker.example/x",
		"https://attackerpush.apple.com/x",
		"https://127.0.0.1/x",
		"https://[::1]/x",
		"https://localhost/x",
		"https://user:pw@fcm.googleapis.com/x",
		"https://laptop.tailnet.ts.net:8443/lazybox.bench.v1.RunnerService/Info",
		"file:///etc/passwd",
		"",
		"https://fcm.googleapis.com/" + strings.Repeat("a", 3000),
	} {
		if _, err := webpush.ValidateEndpoint(raw); err == nil {
			t.Fatalf("accepted %s", raw)
		}
	}
}

func TestProductionDialerRefusesPrivateAddresses(t *testing.T) {
	for _, address := range []string{"127.0.0.1:443", "[::1]:443", "10.1.2.3:443", "192.168.1.5:443", "172.16.0.1:443", "169.254.169.254:80", "100.101.102.103:443", "0.0.0.0:443", "[fd7a:115c:a1e0::1]:443"} {
		if webpush.RefusePrivate("tcp", address, nil) == nil {
			t.Fatalf("dialer would connect to %s", address)
		}
	}
	if err := webpush.RefusePrivate("tcp", "142.250.74.10:443", syscall.RawConn(nil)); err != nil {
		t.Fatalf("a public address was refused: %v", err)
	}
}

func sender(t *testing.T, service *webpushtest.Service, keys *webpush.Keys) *webpush.Sender {
	s := webpush.NewSender(keys)
	s.Client = service.Client()
	s.Validate = func(string) (string, error) { return "127.0.0.1", nil }
	return s
}

func TestSendDeliversEncryptedPayloadWithVAPID(t *testing.T) {
	keys, err := webpush.LoadOrCreateKey(filepath.Join(t.TempDir(), "vapid.key"))
	if err != nil {
		t.Fatal(err)
	}
	service := webpushtest.NewService(keys.PublicKey())
	defer service.Close()
	b := webpushtest.NewBrowser()
	sub := webpush.Subscription{Endpoint: service.Subscribe("phone", b), P256dh: b.P256dh(), Auth: b.Auth()}
	err = sender(t, service, keys).Send(context.Background(), sub, []byte(`{"kind":"approval"}`), webpush.Options{TTL: time.Hour, Urgency: "high", Topic: "abc_-1"})
	if err != nil {
		t.Fatal(err, service.Failures)
	}
	got := service.Messages()
	if len(got) != 1 || string(got[0].Payload) != `{"kind":"approval"}` {
		t.Fatalf("%+v %v", got, service.Failures)
	}
	h := got[0].Header
	if h.Get("TTL") != "3600" || h.Get("Urgency") != "high" || h.Get("Topic") != "abc_-1" || h.Get("Content-Type") != "application/octet-stream" {
		t.Fatalf("headers %v", h)
	}
}

func TestSendReportsOutcomes(t *testing.T) {
	keys, _ := webpush.LoadOrCreateKey(filepath.Join(t.TempDir(), "vapid.key"))
	other, _ := webpush.LoadOrCreateKey(filepath.Join(t.TempDir(), "vapid.key"))
	service := webpushtest.NewService(keys.PublicKey())
	defer service.Close()
	b := webpushtest.NewBrowser()
	sub := func(path string) webpush.Subscription {
		return webpush.Subscription{Endpoint: service.Subscribe(path, b), P256dh: b.P256dh(), Auth: b.Auth()}
	}
	ctx := context.Background()
	gone := sub("gone")
	service.Respond("gone", http.StatusGone)
	if err := sender(t, service, keys).Send(ctx, gone, []byte("x"), webpush.Options{}); !errors.Is(err, webpush.ErrGone) {
		t.Fatalf("410: %v", err)
	}
	service.Respond("missing", http.StatusNotFound)
	if err := sender(t, service, keys).Send(ctx, sub("missing"), []byte("x"), webpush.Options{}); !errors.Is(err, webpush.ErrGone) {
		t.Fatalf("404: %v", err)
	}
	// Signed with another Runner's key: the push service refuses it.
	if err := sender(t, service, other).Send(ctx, sub("phone"), []byte("x"), webpush.Options{}); err == nil || !strings.Contains(err.Error(), "refused this Runner's key") {
		t.Fatalf("403: %v", err)
	}
	service.Respond("busy", http.StatusTooManyRequests)
	if err := sender(t, service, keys).Send(ctx, sub("busy"), []byte("x"), webpush.Options{}); err == nil || !strings.Contains(err.Error(), "rate limiting") {
		t.Fatalf("429: %v", err)
	}
	if err := sender(t, service, keys).Send(ctx, sub("phone"), []byte("x"), webpush.Options{Topic: "not a topic!"}); err == nil {
		t.Fatal("an invalid topic must be refused")
	}
	// The production validator runs before anything is sent.
	production := webpush.NewSender(keys)
	production.Client = service.Client()
	if err := production.Send(ctx, sub("phone"), []byte("x"), webpush.Options{}); err == nil || !strings.Contains(err.Error(), "push service") {
		t.Fatalf("a test server endpoint must be refused in production: %v", err)
	}
	if n := len(service.Messages()); n != 0 {
		t.Fatalf("%d messages delivered", n)
	}
}
