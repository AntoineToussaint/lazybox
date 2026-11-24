// Package devices owns the phones and tablets that control a Runner remotely
// (REMOTE.md): one-time pairing offers, paired device records, and the
// short-lived access credentials a paired device obtains by proving it holds
// its own key.
//
// A device never receives the owner's bearer. Pairing binds a device to an
// ECDSA P-256 public key the device generated itself (in a browser, a
// non-extractable WebCrypto key). Every access credential afterwards is issued
// only against a fresh, single-use challenge signed with that key, lives
// fifteen minutes, and is checked against the device record on every call, so
// revoking a device ends its access at its next request and ends the streams
// it has open (Bind).
package devices

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"crypto/x509"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"math/big"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"
	"unicode/utf8"
)

// Grants: what a paired device may do. Read is always granted; the rest are
// chosen by the owner when pairing.
const (
	// GrantRead lists conversations and workspaces, follows a conversation
	// and its command output, and reads workspace changes.
	GrantRead = "read"
	// GrantRespond sends messages, answers questions, approves or declines
	// requests, and interrupts a turn.
	GrantRespond = "respond"
	// GrantStart starts a conversation in an existing workspace and continues
	// an ended one: both start an agent process on this machine.
	GrantStart = "start"
)

// DefaultGrants is what a pairing grants unless the owner chooses otherwise.
var DefaultGrants = []string{GrantRead, GrantRespond}

var knownGrants = map[string]int{GrantRead: 0, GrantRespond: 1, GrantStart: 2}

// Offer and session lifetimes. An offer is shown as a QR code on the owner's
// screen; a challenge is answered by the device at once; an access credential
// is renewed by the device well before it ends.
const (
	OfferLifetime      = 5 * time.Minute
	ChallengeLifetime  = time.Minute
	CredentialLifetime = 15 * time.Minute
	offerRetention     = 10 * time.Minute
	maxPendingOffers   = 8
	maxOfferFailures   = 5
	// maxOfferAttempts bounds the pairing requests one offer answers per
	// minute, whatever they carry. It is per offer, so only someone who has
	// the pairing code can spend it: no tailnet peer can block pairing by
	// flooding the endpoint (a shared budget let any of them).
	maxOfferAttempts = 30
	maxDevices       = 32
	// maxDeviceChallenges bounds one device's pending sign-in challenges.
	// The challenge endpoint is unauthenticated, so the bound is per device:
	// whoever floods one device's challenges cannot keep another device from
	// signing in. The whole table holds at most maxDevices of these.
	maxDeviceChallenges = 8
	lastSeenPersistence = time.Minute
	keyBytes            = 32
	// TokenPrefix marks a device access credential; no other bearer has it.
	TokenPrefix = "lbx-device."
)

// Pairing states.
const (
	OfferPending   = "pending"
	OfferPaired    = "paired"
	OfferExpired   = "expired"
	OfferCancelled = "cancelled"
	// OfferFailed is an offer burned by repeated wrong secrets.
	OfferFailed = "failed"
)

// Domain separation for what a device signs.
const (
	pairDomain    = "lazybox-device-pair-v1\x00"
	sessionDomain = "lazybox-device-session-v1\x00"
	tokenLabel    = "lazybox v2 device credential\x00"
)

var (
	ErrUnknownOffer   = errors.New("this pairing code is not valid; show a new one on your computer")
	ErrOfferUsed      = errors.New("this pairing code was already used; show a new one on your computer")
	ErrOfferExpired   = errors.New("this pairing code expired; show a new one on your computer")
	ErrOfferCancelled = errors.New("this pairing code was cancelled on your computer")
	ErrUnknownDevice  = errors.New("this device is not paired with this Runner")
	ErrBadSignature   = errors.New("the device key did not sign this request")
	ErrBadChallenge   = errors.New("the sign-in challenge is unknown, used or expired")
	ErrBadCredential  = errors.New("invalid device credential")
	ErrTooMany        = errors.New("too many pending requests; try again shortly")
	// ErrTooManyAttempts refuses a pairing offer's requests past its budget.
	ErrTooManyAttempts = errors.New("too many pairing attempts; wait a minute")
	// ErrCredentialEnded is the cause of a bound context (Bind) ending
	// because its credential expired or its device was revoked.
	ErrCredentialEnded = errors.New("this device's credential ended; sign in again from the phone")
)

// Device is one paired phone or tablet.
type Device struct {
	ID   string `json:"id"`
	Name string `json:"name"`
	// PublicKey is the device's ECDSA P-256 key, PKIX (SPKI) DER, base64url.
	PublicKey  string    `json:"publicKey"`
	Grants     []string  `json:"grants"`
	PairedAt   time.Time `json:"pairedAt"`
	LastSeenAt time.Time `json:"lastSeenAt"`
	// Push is the device's notification subscription and preferences
	// (push.go); nil until the phone first sets them.
	Push *Push `json:"push,omitempty"`
}

// Has reports whether the device holds a grant.
func (d Device) Has(grant string) bool {
	for _, g := range d.Grants {
		if g == grant {
			return true
		}
	}
	return false
}

// Fingerprint is a short, human-comparable digest of the device's key; the
// phone shows the same value.
func (d Device) Fingerprint() string {
	der, err := base64.RawURLEncoding.DecodeString(d.PublicKey)
	if err != nil {
		return ""
	}
	return Fingerprint(der)
}

// Fingerprint formats the first 8 bytes of SHA-256(SPKI) as four groups.
func Fingerprint(spki []byte) string {
	sum := sha256.Sum256(spki)
	h := strings.ToUpper(hex.EncodeToString(sum[:8]))
	return h[0:4] + " " + h[4:8] + " " + h[8:12] + " " + h[12:16]
}

// Offer is one pairing offer, as the owner's screen sees it. The secret is
// only ever returned by Begin.
type Offer struct {
	ID        string
	State     string
	Grants    []string
	ExpiresAt time.Time
	// Device is set once the offer was used.
	Device *Device
}

type offer struct {
	id       string
	secret   [sha256.Size]byte
	grants   []string
	expires  time.Time
	state    string
	failures int
	// attempts counts the requests naming this offer since window began.
	window   time.Time
	attempts int
	device   string
	ended    time.Time
}

type challenge struct {
	device  string
	expires time.Time
}

// Manager holds a Runner's paired devices. It is safe for concurrent use.
type Manager struct {
	path string
	key  []byte
	// Now is the clock; tests replace it.
	Now func() time.Time

	mu         sync.Mutex
	devices    map[string]*Device
	persisted  map[string]time.Time
	offers     map[string]*offer
	challenges map[string]challenge
	// streams: per device, the cancel funcs of every context bound to one
	// of its credentials (Bind), ended by Revoke.
	streams map[string]map[*binding]struct{}
	// pushChanges: per device, when it last changed its notifications
	// (SetPush, pushChangesPerMinute).
	pushChanges map[string][]time.Time
}

// binding is one context bound to a device credential.
type binding struct{ cancel context.CancelCauseFunc }

// Open loads the devices paired with the Runner whose state directory is
// state, creating the credential key on first use.
func Open(state string) (*Manager, error) {
	key, err := loadKey(filepath.Join(state, "device-credential.key"))
	if err != nil {
		return nil, err
	}
	m := &Manager{path: filepath.Join(state, "devices.json"), key: key, Now: time.Now, devices: map[string]*Device{}, persisted: map[string]time.Time{}, offers: map[string]*offer{}, challenges: map[string]challenge{}, streams: map[string]map[*binding]struct{}{}, pushChanges: map[string][]time.Time{}}
	data, err := os.ReadFile(m.path)
	if errors.Is(err, fs.ErrNotExist) {
		return m, nil
	}
	if err != nil {
		return nil, err
	}
	var file struct {
		Version int      `json:"version"`
		Devices []Device `json:"devices"`
	}
	if err = json.Unmarshal(data, &file); err != nil {
		return nil, fmt.Errorf("paired devices unreadable: %w", err)
	}
	if file.Version != 1 {
		return nil, fmt.Errorf("paired devices file version %d is not supported", file.Version)
	}
	for i := range file.Devices {
		d := file.Devices[i]
		if !validID(d.ID) {
			return nil, errors.New("paired devices file holds an invalid device")
		}
		if _, err = parseKey(d.PublicKey); err != nil {
			return nil, fmt.Errorf("paired device %s: %w", d.ID, err)
		}
		m.devices[d.ID] = &d
		m.persisted[d.ID] = d.LastSeenAt
	}
	return m, nil
}

func loadKey(path string) ([]byte, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		key := make([]byte, keyBytes)
		if _, err = rand.Read(key); err != nil {
			return nil, err
		}
		var f *os.File
		if f, err = os.CreateTemp(filepath.Dir(path), ".device-credential-*"); err != nil {
			return nil, err
		}
		defer os.Remove(f.Name())
		if _, err = f.Write(key); err == nil {
			err = f.Sync()
		}
		if closeErr := f.Close(); err == nil {
			err = closeErr
		}
		if err != nil {
			return nil, err
		}
		// Link, not rename: it never replaces a key that appeared meanwhile.
		if err = os.Link(f.Name(), path); err != nil && !errors.Is(err, fs.ErrExist) {
			return nil, err
		}
		data, err = os.ReadFile(path)
	}
	if err != nil {
		return nil, err
	}
	if len(data) != keyBytes {
		return nil, errors.New("device credential key is corrupt; remove it to sign every device out")
	}
	return data, nil
}

func randomID(n int) (string, error) {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return base64.RawURLEncoding.EncodeToString(b), nil
}

func validID(id string) bool {
	if id == "" || len(id) > 64 {
		return false
	}
	for _, r := range id {
		if !(r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' || r == '-' || r == '_') {
			return false
		}
	}
	return true
}

// NormalizeGrants validates grants and returns them in canonical order with
// read always included.
func NormalizeGrants(grants []string) ([]string, error) {
	set := map[string]bool{GrantRead: true}
	for _, g := range grants {
		if _, ok := knownGrants[g]; !ok {
			return nil, fmt.Errorf("unknown device grant %q", g)
		}
		set[g] = true
	}
	out := make([]string, 0, len(set))
	for g := range set {
		out = append(out, g)
	}
	sort.Slice(out, func(i, j int) bool { return knownGrants[out[i]] < knownGrants[out[j]] })
	return out, nil
}

// expire marks ended offers and forgets old ones and old challenges. Caller holds mu.
func (m *Manager) expire(now time.Time) {
	for id, o := range m.offers {
		if o.state == OfferPending && !now.Before(o.expires) {
			o.state, o.ended = OfferExpired, now
		}
		if o.state != OfferPending && now.Sub(o.ended) > offerRetention {
			delete(m.offers, id)
		}
	}
	for id, c := range m.challenges {
		if !now.Before(c.expires) {
			delete(m.challenges, id)
		}
	}
}

// Begin creates a one-time pairing offer and returns it with its secret.
// The secret is never stored, only its digest.
func (m *Manager) Begin(grants []string) (Offer, string, error) {
	grants, err := NormalizeGrants(grants)
	if err != nil {
		return Offer{}, "", err
	}
	id, err := randomID(16)
	if err != nil {
		return Offer{}, "", err
	}
	secret, err := randomID(32)
	if err != nil {
		return Offer{}, "", err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	now := m.Now()
	m.expire(now)
	pending := 0
	for _, o := range m.offers {
		if o.state == OfferPending {
			pending++
		}
	}
	if pending >= maxPendingOffers {
		return Offer{}, "", ErrTooMany
	}
	if len(m.devices) >= maxDevices {
		return Offer{}, "", fmt.Errorf("at most %d devices can be paired; revoke one first", maxDevices)
	}
	o := &offer{id: id, secret: sha256.Sum256([]byte(secret)), grants: grants, expires: now.Add(OfferLifetime), state: OfferPending}
	m.offers[id] = o
	return m.view(o), secret, nil
}

func (m *Manager) view(o *offer) Offer {
	v := Offer{ID: o.id, State: o.state, Grants: append([]string(nil), o.grants...), ExpiresAt: o.expires}
	if d := m.devices[o.device]; d != nil {
		c := d.clone()
		v.Device = &c
	}
	return v
}

// Pairing returns an offer's current state, for the owner's screen.
func (m *Manager) Pairing(id string) (Offer, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.expire(m.Now())
	o := m.offers[id]
	if o == nil {
		return Offer{}, ErrUnknownOffer
	}
	return m.view(o), nil
}

// Cancel withdraws a pending offer. Cancelling an offer that already ended
// returns its final state.
func (m *Manager) Cancel(id string) (Offer, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	now := m.Now()
	m.expire(now)
	o := m.offers[id]
	if o == nil {
		return Offer{}, ErrUnknownOffer
	}
	if o.state == OfferPending {
		o.state, o.ended = OfferCancelled, now
	}
	return m.view(o), nil
}

// PairRequest is what a device sends to complete pairing.
type PairRequest struct {
	Pairing string `json:"pairing"`
	Secret  string `json:"secret"`
	Name    string `json:"name"`
	// PublicKey is PKIX (SPKI) DER, base64url, of an ECDSA P-256 key.
	PublicKey string `json:"publicKey"`
	// Signature is IEEE P1363 (r||s), base64url, over PairMessage.
	Signature string `json:"signature"`
}

// PairMessage is what a device signs to prove it holds the key it pairs.
func PairMessage(pairing, publicKey string) []byte {
	return []byte(pairDomain + pairing + "\x00" + publicKey)
}

// SessionMessage is what a device signs to answer a sign-in challenge.
func SessionMessage(device, challenge string) []byte {
	return []byte(sessionDomain + device + "\x00" + challenge)
}

// Complete uses an offer: it checks the one-time secret, the key's proof of
// possession and the offer's lifetime, and pairs the device. An offer is used
// at most once; repeated wrong secrets burn it.
func (m *Manager) Complete(r PairRequest) (Device, error) {
	key, err := parseKey(r.PublicKey)
	if err != nil {
		return Device{}, err
	}
	name, err := deviceName(r.Name)
	if err != nil {
		return Device{}, err
	}
	presented := sha256.Sum256([]byte(r.Secret))
	m.mu.Lock()
	defer m.mu.Unlock()
	now := m.Now()
	m.expire(now)
	o := m.offers[r.Pairing]
	if o == nil {
		return Device{}, ErrUnknownOffer
	}
	switch o.state {
	case OfferPaired:
		return Device{}, ErrOfferUsed
	case OfferExpired:
		return Device{}, ErrOfferExpired
	case OfferCancelled:
		return Device{}, ErrOfferCancelled
	case OfferFailed:
		return Device{}, ErrUnknownOffer
	}
	if now.Sub(o.window) >= time.Minute {
		o.window, o.attempts = now, 0
	}
	if o.attempts++; o.attempts > maxOfferAttempts {
		return Device{}, ErrTooManyAttempts
	}
	if subtle.ConstantTimeCompare(presented[:], o.secret[:]) != 1 {
		o.failures++
		if o.failures >= maxOfferFailures {
			o.state, o.ended = OfferFailed, now
		}
		return Device{}, ErrUnknownOffer
	}
	if !verify(key, PairMessage(r.Pairing, r.PublicKey), r.Signature) {
		return Device{}, ErrBadSignature
	}
	if len(m.devices) >= maxDevices {
		return Device{}, fmt.Errorf("at most %d devices can be paired; revoke one first", maxDevices)
	}
	id, err := randomID(12)
	if err != nil {
		return Device{}, err
	}
	d := &Device{ID: id, Name: name, PublicKey: r.PublicKey, Grants: append([]string(nil), o.grants...), PairedAt: now, LastSeenAt: now}
	m.devices[id] = d
	if err = m.save(); err != nil {
		delete(m.devices, id)
		return Device{}, err
	}
	o.state, o.device, o.ended = OfferPaired, id, now
	m.persisted[id] = now
	return d.clone(), nil
}

func deviceName(name string) (string, error) {
	name = strings.TrimSpace(name)
	if name == "" {
		return "Phone", nil
	}
	if utf8.RuneCountInString(name) > 64 {
		return "", errors.New("a device name has at most 64 characters")
	}
	for _, r := range name {
		if !unicode.IsPrint(r) {
			return "", errors.New("a device name cannot hold control characters")
		}
	}
	return name, nil
}

func parseKey(encoded string) (*ecdsa.PublicKey, error) {
	der, err := base64.RawURLEncoding.DecodeString(encoded)
	if err != nil || len(der) == 0 || len(der) > 512 {
		return nil, errors.New("the device key is not encoded as expected")
	}
	parsed, err := x509.ParsePKIXPublicKey(der)
	if err != nil {
		return nil, errors.New("the device key is not a public key")
	}
	key, ok := parsed.(*ecdsa.PublicKey)
	if !ok || key.Curve != elliptic.P256() {
		return nil, errors.New("the device key must be ECDSA P-256")
	}
	return key, nil
}

// verify checks an IEEE P1363 (r||s) ECDSA P-256 signature over SHA-256(message),
// the format WebCrypto produces.
func verify(key *ecdsa.PublicKey, message []byte, signature string) bool {
	raw, err := base64.RawURLEncoding.DecodeString(signature)
	if err != nil || len(raw) != 64 {
		return false
	}
	digest := sha256.Sum256(message)
	r, s := new(big.Int).SetBytes(raw[:32]), new(big.Int).SetBytes(raw[32:])
	return ecdsa.Verify(key, digest[:], r, s)
}

// Challenge issues a single-use sign-in challenge to a paired device.
func (m *Manager) Challenge(device string) (string, time.Time, error) {
	value, err := randomID(32)
	if err != nil {
		return "", time.Time{}, err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	now := m.Now()
	m.expire(now)
	if m.devices[device] == nil {
		return "", time.Time{}, ErrUnknownDevice
	}
	pending := 0
	for _, c := range m.challenges {
		if c.device == device {
			pending++
		}
	}
	if pending >= maxDeviceChallenges {
		return "", time.Time{}, ErrTooMany
	}
	expires := now.Add(ChallengeLifetime)
	m.challenges[value] = challenge{device: device, expires: expires}
	return value, expires, nil
}

// Session answers a challenge: a valid signature by the device's key yields
// a fresh access credential. The challenge is consumed whatever the outcome.
func (m *Manager) Session(device, value, signature string) (string, time.Time, Device, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	now := m.Now()
	m.expire(now)
	c, ok := m.challenges[value]
	delete(m.challenges, value)
	if !ok || c.device != device {
		return "", time.Time{}, Device{}, ErrBadChallenge
	}
	d := m.devices[device]
	if d == nil {
		return "", time.Time{}, Device{}, ErrUnknownDevice
	}
	key, err := parseKey(d.PublicKey)
	if err != nil {
		return "", time.Time{}, Device{}, err
	}
	if !verify(key, SessionMessage(device, value), signature) {
		return "", time.Time{}, Device{}, ErrBadSignature
	}
	expires := now.Add(CredentialLifetime).Truncate(time.Second)
	m.touch(d, now)
	return m.token(device, expires), expires, d.clone(), nil
}

func (m *Manager) mac(device string, expires int64) []byte {
	h := hmac.New(sha256.New, m.key)
	h.Write([]byte(tokenLabel))
	h.Write([]byte(device))
	h.Write([]byte{0})
	h.Write([]byte(strconv.FormatInt(expires, 10)))
	return h.Sum(nil)
}

func (m *Manager) token(device string, expires time.Time) string {
	unix := expires.Unix()
	return TokenPrefix + device + "." + strconv.FormatInt(unix, 10) + "." + base64.RawURLEncoding.EncodeToString(m.mac(device, unix))
}

// parseToken checks an access credential's signature and returns its device
// and expiry (Unix seconds). It does not look the device up.
func (m *Manager) parseToken(token string) (string, int64, error) {
	rest, ok := strings.CutPrefix(token, TokenPrefix)
	if !ok {
		return "", 0, ErrBadCredential
	}
	parts := strings.Split(rest, ".")
	if len(parts) != 3 || !validID(parts[0]) {
		return "", 0, ErrBadCredential
	}
	expires, err := strconv.ParseInt(parts[1], 10, 64)
	if err != nil {
		return "", 0, ErrBadCredential
	}
	presented, err := base64.RawURLEncoding.DecodeString(parts[2])
	if err != nil || !hmac.Equal(presented, m.mac(parts[0], expires)) {
		return "", 0, ErrBadCredential
	}
	return parts[0], expires, nil
}

// Authenticate resolves an access credential to its device. It fails for a
// forged, expired or revoked one, and records that the device was seen.
func (m *Manager) Authenticate(token string) (Device, error) {
	id, expires, err := m.parseToken(token)
	if err != nil {
		return Device{}, err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.live(id, expires)
}

// live returns the device of an unexpired credential and records that it was
// seen. Caller holds mu.
func (m *Manager) live(id string, expires int64) (Device, error) {
	now := m.Now()
	if now.Unix() >= expires {
		return Device{}, errors.New("device credential expired")
	}
	d := m.devices[id]
	if d == nil {
		return Device{}, errors.New("device credential revoked")
	}
	m.touch(d, now)
	return d.clone(), nil
}

// Bind authenticates an access credential for a long-lived call (a stream)
// and returns a context derived from parent that ends, with cause
// ErrCredentialEnded, when the credential expires or its device is revoked:
// a stream opened with a credential lasts no longer than the credential
// would on its next request. release must be called when the call ends.
func (m *Manager) Bind(parent context.Context, token string) (ctx context.Context, release context.CancelFunc, d Device, err error) {
	id, expires, err := m.parseToken(token)
	if err != nil {
		return nil, nil, Device{}, err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if d, err = m.live(id, expires); err != nil {
		return nil, nil, Device{}, err
	}
	// The deadline follows the Manager's clock (tests replace it) on the
	// real one a context uses.
	deadline := time.Now().Add(time.Unix(expires, 0).Sub(m.Now()))
	expiring, stopDeadline := context.WithDeadlineCause(parent, deadline, ErrCredentialEnded)
	bound, cancel := context.WithCancelCause(expiring)
	b := &binding{cancel: cancel}
	if m.streams[id] == nil {
		m.streams[id] = map[*binding]struct{}{}
	}
	m.streams[id][b] = struct{}{}
	release = func() {
		m.mu.Lock()
		delete(m.streams[id], b)
		if len(m.streams[id]) == 0 {
			delete(m.streams, id)
		}
		m.mu.Unlock()
		cancel(context.Canceled)
		stopDeadline()
	}
	return bound, release, d, nil
}

// touch records activity, writing it at most once a minute. Caller holds mu.
func (m *Manager) touch(d *Device, now time.Time) {
	d.LastSeenAt = now
	if now.Sub(m.persisted[d.ID]) >= lastSeenPersistence {
		m.persisted[d.ID] = now
		_ = m.save() // Last seen is advisory; the next write retries.
	}
}

// List returns paired devices, oldest first.
func (m *Manager) List() []Device {
	m.mu.Lock()
	defer m.mu.Unlock()
	out := make([]Device, 0, len(m.devices))
	for _, d := range m.devices {
		out = append(out, d.clone())
	}
	sort.Slice(out, func(i, j int) bool {
		if !out[i].PairedAt.Equal(out[j].PairedAt) {
			return out[i].PairedAt.Before(out[j].PairedAt)
		}
		return out[i].ID < out[j].ID
	})
	return out
}

// Revoke unpairs a device. Its access credential stops working at once and
// its key can no longer obtain another; pairing again needs a new offer.
func (m *Manager) Revoke(id string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	d := m.devices[id]
	if d == nil {
		return ErrUnknownDevice
	}
	delete(m.devices, id)
	if err := m.save(); err != nil {
		m.devices[id] = d
		return err
	}
	delete(m.persisted, id)
	for value, c := range m.challenges {
		if c.device == id {
			delete(m.challenges, value)
		}
	}
	// Streams it opened end now, not at their credential's expiry.
	for b := range m.streams[id] {
		b.cancel(ErrCredentialEnded)
	}
	delete(m.streams, id)
	delete(m.pushChanges, id)
	return nil
}

// save writes every device atomically, owner-readable only. Caller holds mu.
func (m *Manager) save() error {
	file := struct {
		Version int      `json:"version"`
		Devices []Device `json:"devices"`
	}{Version: 1, Devices: []Device{}}
	for _, d := range m.devices {
		file.Devices = append(file.Devices, d.clone())
	}
	sort.Slice(file.Devices, func(i, j int) bool { return file.Devices[i].ID < file.Devices[j].ID })
	data, err := json.MarshalIndent(file, "", "  ")
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(m.path), ".devices-*")
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
	return os.Rename(f.Name(), m.path)
}
