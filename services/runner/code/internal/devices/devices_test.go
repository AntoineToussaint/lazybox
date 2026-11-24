package devices_test

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices/devicetest"
)

type clock struct {
	mu  sync.Mutex
	now time.Time
}

func (c *clock) Now() time.Time          { c.mu.Lock(); defer c.mu.Unlock(); return c.now }
func (c *clock) Advance(d time.Duration) { c.mu.Lock(); c.now = c.now.Add(d); c.mu.Unlock() }

func open(t *testing.T, state string) (*devices.Manager, *clock) {
	t.Helper()
	m, err := devices.Open(state)
	if err != nil {
		t.Fatal(err)
	}
	c := &clock{now: time.Unix(1_800_000_000, 0)}
	m.Now = c.Now
	return m, c
}

func phone(t *testing.T) *devicetest.Phone {
	t.Helper()
	p, err := devicetest.New()
	if err != nil {
		t.Fatal(err)
	}
	return p
}

func pair(t *testing.T, m *devices.Manager, p *devicetest.Phone, grants ...string) devices.Device {
	t.Helper()
	offer, secret, err := m.Begin(grants)
	if err != nil {
		t.Fatal(err)
	}
	d, err := m.Complete(p.PairRequest(offer.ID, secret, "Antoine's iPhone"))
	if err != nil {
		t.Fatal(err)
	}
	return d
}

func signIn(t *testing.T, m *devices.Manager, p *devicetest.Phone, device string) string {
	t.Helper()
	challenge, _, err := m.Challenge(device)
	if err != nil {
		t.Fatal(err)
	}
	token, _, _, err := m.Session(device, challenge, p.Answer(device, challenge))
	if err != nil {
		t.Fatal(err)
	}
	return token
}

func TestPairingIssuesADeviceBoundToItsKeyAndOffer(t *testing.T) {
	m, _ := open(t, t.TempDir())
	p := phone(t)
	offer, secret, err := m.Begin([]string{devices.GrantRespond})
	if err != nil {
		t.Fatal(err)
	}
	if offer.State != devices.OfferPending || len(secret) < 40 || strings.Contains(offer.ID, secret) {
		t.Fatalf("offer %+v with secret of %d chars", offer, len(secret))
	}
	d, err := m.Complete(p.PairRequest(offer.ID, secret, "  Antoine's iPhone  "))
	if err != nil {
		t.Fatal(err)
	}
	if d.Name != "Antoine's iPhone" || d.PublicKey != p.PublicKey || strings.Join(d.Grants, ",") != "read,respond" {
		t.Fatalf("device %+v", d)
	}
	status, err := m.Pairing(offer.ID)
	if err != nil || status.State != devices.OfferPaired || status.Device == nil || status.Device.ID != d.ID {
		t.Fatalf("owner sees %+v, %v", status, err)
	}
	if d.Fingerprint() == "" || len(d.Fingerprint()) != 19 {
		t.Fatalf("fingerprint %q", d.Fingerprint())
	}
}

func TestAnOfferIsUsedOnce(t *testing.T) {
	m, _ := open(t, t.TempDir())
	first, second := phone(t), phone(t)
	offer, secret, err := m.Begin(nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = m.Complete(first.PairRequest(offer.ID, secret, "first")); err != nil {
		t.Fatal(err)
	}
	// The same QR code scanned again, by the same phone or another, pairs nothing.
	for _, p := range []*devicetest.Phone{first, second} {
		if _, err = m.Complete(p.PairRequest(offer.ID, secret, "again")); !errors.Is(err, devices.ErrOfferUsed) {
			t.Fatalf("replayed offer: %v", err)
		}
	}
	if n := len(m.List()); n != 1 {
		t.Fatalf("%d devices after a replay", n)
	}
}

func TestAnOfferExpires(t *testing.T) {
	m, c := open(t, t.TempDir())
	offer, secret, err := m.Begin(nil)
	if err != nil {
		t.Fatal(err)
	}
	c.Advance(devices.OfferLifetime)
	if _, err = m.Complete(phone(t).PairRequest(offer.ID, secret, "late")); !errors.Is(err, devices.ErrOfferExpired) {
		t.Fatalf("expired offer: %v", err)
	}
	if status, _ := m.Pairing(offer.ID); status.State != devices.OfferExpired {
		t.Fatalf("owner sees %q", status.State)
	}
}

func TestACancelledOfferPairsNothing(t *testing.T) {
	m, _ := open(t, t.TempDir())
	offer, secret, err := m.Begin(nil)
	if err != nil {
		t.Fatal(err)
	}
	if status, err := m.Cancel(offer.ID); err != nil || status.State != devices.OfferCancelled {
		t.Fatalf("cancel: %+v %v", status, err)
	}
	if _, err = m.Complete(phone(t).PairRequest(offer.ID, secret, "x")); !errors.Is(err, devices.ErrOfferCancelled) {
		t.Fatalf("cancelled offer: %v", err)
	}
}

func TestWrongSecretsBurnAnOffer(t *testing.T) {
	m, _ := open(t, t.TempDir())
	p := phone(t)
	offer, secret, err := m.Begin(nil)
	if err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 5; i++ {
		if _, err = m.Complete(p.PairRequest(offer.ID, secret+"x", "guess")); !errors.Is(err, devices.ErrUnknownOffer) {
			t.Fatalf("wrong secret: %v", err)
		}
	}
	// Even the right secret no longer works: guessing has a hard budget.
	if _, err = m.Complete(p.PairRequest(offer.ID, secret, "right")); err == nil {
		t.Fatal("burned offer accepted")
	}
	if status, _ := m.Pairing(offer.ID); status.State != devices.OfferFailed {
		t.Fatalf("owner sees %q", status.State)
	}
}

func TestPairingRequiresProofOfTheKey(t *testing.T) {
	m, _ := open(t, t.TempDir())
	p, other := phone(t), phone(t)
	offer, secret, err := m.Begin(nil)
	if err != nil {
		t.Fatal(err)
	}
	r := p.PairRequest(offer.ID, secret, "x")
	r.PublicKey = other.PublicKey // Claims a key it cannot sign with.
	if _, err = m.Complete(r); !errors.Is(err, devices.ErrBadSignature) {
		t.Fatalf("key without proof: %v", err)
	}
	// A failed proof does not consume the offer: the real phone still pairs.
	if _, err = m.Complete(p.PairRequest(offer.ID, secret, "x")); err != nil {
		t.Fatal(err)
	}
}

func TestPairingValidatesGrantsAndNames(t *testing.T) {
	m, _ := open(t, t.TempDir())
	if _, _, err := m.Begin([]string{"settings"}); err == nil {
		t.Fatal("unknown grant accepted")
	}
	offer, secret, err := m.Begin(nil)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Join(offer.Grants, ",") != "read" {
		t.Fatalf("grants %v", offer.Grants)
	}
	p := phone(t)
	for _, name := range []string{strings.Repeat("x", 65), "bell\a"} {
		if _, err = m.Complete(p.PairRequest(offer.ID, secret, name)); err == nil {
			t.Fatalf("name %q accepted", name)
		}
	}
}

func TestSessionsNeedAFreshSingleUseChallenge(t *testing.T) {
	m, c := open(t, t.TempDir())
	p := phone(t)
	d := pair(t, m, p)
	challenge, _, err := m.Challenge(d.ID)
	if err != nil {
		t.Fatal(err)
	}
	signature := p.Answer(d.ID, challenge)
	token, expires, device, err := m.Session(d.ID, challenge, signature)
	if err != nil || device.ID != d.ID || !strings.HasPrefix(token, devices.TokenPrefix) {
		t.Fatalf("session: %v %+v", err, device)
	}
	if got := expires.Sub(c.Now()); got > devices.CredentialLifetime || got < devices.CredentialLifetime-time.Second {
		t.Fatalf("credential lifetime %v", got)
	}
	// Replaying a captured answer gets nothing.
	if _, _, _, err = m.Session(d.ID, challenge, signature); !errors.Is(err, devices.ErrBadChallenge) {
		t.Fatalf("replayed challenge: %v", err)
	}
	// Another key cannot answer, and a failed answer consumes the challenge.
	challenge, _, _ = m.Challenge(d.ID)
	if _, _, _, err = m.Session(d.ID, challenge, phone(t).Answer(d.ID, challenge)); !errors.Is(err, devices.ErrBadSignature) {
		t.Fatalf("wrong key: %v", err)
	}
	if _, _, _, err = m.Session(d.ID, challenge, p.Answer(d.ID, challenge)); !errors.Is(err, devices.ErrBadChallenge) {
		t.Fatalf("challenge reused after a failure: %v", err)
	}
	// A challenge expires.
	challenge, _, _ = m.Challenge(d.ID)
	c.Advance(devices.ChallengeLifetime)
	if _, _, _, err = m.Session(d.ID, challenge, p.Answer(d.ID, challenge)); !errors.Is(err, devices.ErrBadChallenge) {
		t.Fatalf("expired challenge: %v", err)
	}
	// A challenge issued to one device cannot sign in another.
	q := phone(t)
	e := pair(t, m, q)
	challenge, _, _ = m.Challenge(d.ID)
	if _, _, _, err = m.Session(e.ID, challenge, q.Answer(e.ID, challenge)); !errors.Is(err, devices.ErrBadChallenge) {
		t.Fatalf("cross-device challenge: %v", err)
	}
	if _, _, err = m.Challenge("unknown"); !errors.Is(err, devices.ErrUnknownDevice) {
		t.Fatalf("challenge for an unknown device: %v", err)
	}
}

func TestCredentialsExpireAndCannotBeForged(t *testing.T) {
	m, c := open(t, t.TempDir())
	p := phone(t)
	d := pair(t, m, p)
	other := pair(t, m, phone(t))
	token := signIn(t, m, p, d.ID)
	if got, err := m.Authenticate(token); err != nil || got.ID != d.ID {
		t.Fatalf("authenticate: %+v %v", got, err)
	}
	parts := strings.Split(strings.TrimPrefix(token, devices.TokenPrefix), ".")
	forgeries := []string{
		token + "x",
		devices.TokenPrefix + other.ID + "." + parts[1] + "." + parts[2],     // Rebound to another device.
		devices.TokenPrefix + parts[0] + "." + "9999999999" + "." + parts[2], // Extended.
		strings.TrimPrefix(token, devices.TokenPrefix),                       // No prefix.
		"lbx-agent." + strings.TrimPrefix(token, devices.TokenPrefix),        // Another kind.
		devices.TokenPrefix + parts[0] + "." + parts[1],                      // No MAC.
		devices.TokenPrefix + "../x." + parts[1] + "." + parts[2], "", devices.TokenPrefix,
	}
	for _, bad := range forgeries {
		if _, err := m.Authenticate(bad); err == nil {
			t.Errorf("forged credential %q accepted", bad)
		}
	}
	c.Advance(devices.CredentialLifetime)
	if _, err := m.Authenticate(token); err == nil {
		t.Fatal("expired credential accepted")
	}
	// A new sign-in with the device key works again.
	if _, err := m.Authenticate(signIn(t, m, p, d.ID)); err != nil {
		t.Fatal(err)
	}
}

func TestRevokingADeviceEndsItsAccessAtOnce(t *testing.T) {
	state := t.TempDir()
	m, _ := open(t, state)
	p := phone(t)
	d := pair(t, m, p)
	token := signIn(t, m, p, d.ID)
	pending, _, err := m.Challenge(d.ID)
	if err != nil {
		t.Fatal(err)
	}
	if err = m.Revoke(d.ID); err != nil {
		t.Fatal(err)
	}
	if _, err = m.Authenticate(token); err == nil {
		t.Fatal("revoked device's live credential accepted")
	}
	if _, _, _, err = m.Session(d.ID, pending, p.Answer(d.ID, pending)); err == nil {
		t.Fatal("revoked device signed in with a challenge issued before revocation")
	}
	if _, _, err = m.Challenge(d.ID); !errors.Is(err, devices.ErrUnknownDevice) {
		t.Fatalf("revoked device got a challenge: %v", err)
	}
	if err = m.Revoke(d.ID); !errors.Is(err, devices.ErrUnknownDevice) {
		t.Fatalf("second revoke: %v", err)
	}
	// Revocation survives a Runner restart.
	again, _ := open(t, state)
	if len(again.List()) != 0 {
		t.Fatal("revoked device came back after restart")
	}
}

func TestDevicesAndCredentialsSurviveARunnerRestart(t *testing.T) {
	state := t.TempDir()
	m, c := open(t, state)
	p := phone(t)
	d := pair(t, m, p, devices.GrantRespond, devices.GrantStart)
	token := signIn(t, m, p, d.ID)
	c.Advance(2 * time.Minute)
	if _, err := m.Authenticate(token); err != nil {
		t.Fatal(err)
	}
	again, c2 := open(t, state)
	c2.now = c.Now()
	list := again.List()
	if len(list) != 1 || list[0].ID != d.ID || strings.Join(list[0].Grants, ",") != "read,respond,start" || !list[0].LastSeenAt.Equal(c.Now()) {
		t.Fatalf("after restart: %+v", list)
	}
	// The credential key is kept, so a live phone keeps working through a restart.
	if _, err := again.Authenticate(token); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"devices.json", "device-credential.key"} {
		info, err := os.Stat(filepath.Join(state, name))
		if err != nil || info.Mode().Perm() != 0o600 {
			t.Fatalf("%s: %v %v", name, info.Mode(), err)
		}
	}
	data, _ := os.ReadFile(filepath.Join(state, "devices.json"))
	if strings.Contains(string(data), token) {
		t.Fatal("an access credential was written to disk")
	}
}

// The challenge endpoint answers anyone on the tailnet who names a paired
// device. Flooding one device's challenges is refused for that device alone:
// another device still signs in, and the flooded one does once they expire.
func TestChallengesAreBoundedPerDevice(t *testing.T) {
	m, c := open(t, t.TempDir())
	p, q := phone(t), phone(t)
	flooded, other := pair(t, m, p), pair(t, m, q)
	refused := false
	for i := 0; i < 2048 && !refused; i++ {
		_, _, err := m.Challenge(flooded.ID)
		if errors.Is(err, devices.ErrTooMany) {
			refused = true
		} else if err != nil {
			t.Fatal(err)
		}
	}
	if !refused {
		t.Fatal("one device's pending challenges are unbounded")
	}
	signIn(t, m, q, other.ID)
	c.Advance(devices.ChallengeLifetime)
	signIn(t, m, p, flooded.ID)
}

func TestPendingOffersAreBounded(t *testing.T) {
	m, c := open(t, t.TempDir())
	for i := 0; i < 8; i++ {
		if _, _, err := m.Begin(nil); err != nil {
			t.Fatal(err)
		}
	}
	if _, _, err := m.Begin(nil); !errors.Is(err, devices.ErrTooMany) {
		t.Fatalf("ninth pending offer: %v", err)
	}
	c.Advance(devices.OfferLifetime)
	if _, _, err := m.Begin(nil); err != nil {
		t.Fatal("expired offers still count", err)
	}
}
