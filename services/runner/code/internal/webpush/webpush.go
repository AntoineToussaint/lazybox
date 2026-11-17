// Package webpush sends Web Push messages (RFC 8030) to a paired phone
// (REMOTE.md): VAPID (RFC 8292) identifies this Runner to the push service,
// and every payload is encrypted to the phone's own key (RFC 8291,
// aes128gcm), so Apple's, Google's or Mozilla's push service relays a message
// it cannot read.
//
// The phone supplies the URL the Runner posts to, so it is checked against
// the known push services before every send, and the production client
// refuses to connect to loopback, private, link-local or tailnet addresses.
package webpush

import (
	"bytes"
	"context"
	"crypto/aes"
	"crypto/cipher"
	"crypto/ecdh"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/hkdf"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// Subject identifies the sending software to the push service operator. It
// names the project, never the person or their tailnet.
const Subject = "https://github.com/AntoineToussaint/lazybox"

const (
	jwtLifetime = 12 * time.Hour
	recordSize  = 4096
	// MaxPayload is what a push service must accept after encryption (4096)
	// less the aes128gcm header, tag and delimiter.
	MaxPayload = recordSize - 16 - 4 - 1 - 65 - 16 - 1
)

// ErrGone means the push service no longer knows the subscription (the phone
// unsubscribed, or the browser data was cleared): it must be forgotten.
var ErrGone = errors.New("the phone's push subscription has ended")

// Keys is this Runner's VAPID key pair.
type Keys struct {
	private *ecdsa.PrivateKey
}

// LoadOrCreateKey reads the VAPID private key at path, creating it on first
// use. A key that appears meanwhile is never replaced: every subscription a
// phone made is bound to the public key it saw.
func LoadOrCreateKey(path string) (*Keys, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		key, genErr := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
		if genErr != nil {
			return nil, genErr
		}
		raw, genErr := key.Bytes()
		if genErr != nil {
			return nil, genErr
		}
		f, createErr := os.CreateTemp(filepath.Dir(path), ".vapid-*")
		if createErr != nil {
			return nil, createErr
		}
		defer os.Remove(f.Name())
		if _, err = f.Write(raw); err == nil {
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
	key, err := ecdsa.ParseRawPrivateKey(elliptic.P256(), data)
	if err != nil {
		return nil, errors.New("the push notification key is corrupt; remove vapid.key, then turn notifications on again on each phone")
	}
	return &Keys{private: key}, nil
}

// PublicKey is the phone's applicationServerKey: the uncompressed P-256
// point, base64url without padding.
func (k *Keys) PublicKey() string {
	raw, err := k.private.PublicKey.Bytes()
	if err != nil {
		return ""
	}
	return base64.RawURLEncoding.EncodeToString(raw)
}

// authorization is the VAPID header for one push service origin.
func (k *Keys) authorization(endpoint *url.URL, now time.Time) (string, error) {
	enc := base64.RawURLEncoding
	header := enc.EncodeToString([]byte(`{"typ":"JWT","alg":"ES256"}`))
	claims, err := json.Marshal(map[string]any{"aud": endpoint.Scheme + "://" + endpoint.Host, "exp": now.Add(jwtLifetime).Unix(), "sub": Subject})
	if err != nil {
		return "", err
	}
	signing := header + "." + enc.EncodeToString(claims)
	digest := sha256.Sum256([]byte(signing))
	r, s, err := ecdsa.Sign(rand.Reader, k.private, digest[:])
	if err != nil {
		return "", err
	}
	signature := make([]byte, 64)
	r.FillBytes(signature[:32])
	s.FillBytes(signature[32:])
	return "vapid t=" + signing + "." + enc.EncodeToString(signature) + ", k=" + k.PublicKey(), nil
}

// Subscription is a browser's push subscription.
type Subscription struct {
	Endpoint string `json:"endpoint"`
	// P256dh is the browser's uncompressed P-256 public key, base64url.
	P256dh string `json:"p256dh"`
	// Auth is the browser's 16-byte authentication secret, base64url.
	Auth string `json:"auth"`
}

func decode(value string) ([]byte, error) {
	return base64.RawURLEncoding.DecodeString(strings.TrimRight(value, "="))
}

// CheckKeys reports whether a subscription's keys have the shapes RFC 8291 needs.
func CheckKeys(p256dh, auth string) error {
	key, err := decode(p256dh)
	if err != nil || len(key) != 65 || key[0] != 4 {
		return errors.New("the phone's push key is not an uncompressed P-256 key")
	}
	if _, err = ecdh.P256().NewPublicKey(key); err != nil {
		return errors.New("the phone's push key is not on the P-256 curve")
	}
	secret, err := decode(auth)
	if err != nil || len(secret) != 16 {
		return errors.New("the phone's push secret must be 16 bytes")
	}
	return nil
}

// Encrypt encrypts a payload to a subscription's keys (RFC 8291, aes128gcm),
// with a fresh ephemeral key and salt.
func Encrypt(p256dh, auth string, plaintext []byte) ([]byte, error) {
	ephemeral, err := ecdh.P256().GenerateKey(rand.Reader)
	if err != nil {
		return nil, err
	}
	salt := make([]byte, 16)
	if _, err = rand.Read(salt); err != nil {
		return nil, err
	}
	return encrypt(p256dh, auth, plaintext, ephemeral, salt)
}

func encrypt(p256dh, auth string, plaintext []byte, ephemeral *ecdh.PrivateKey, salt []byte) ([]byte, error) {
	if err := CheckKeys(p256dh, auth); err != nil {
		return nil, err
	}
	if len(plaintext) > MaxPayload {
		return nil, fmt.Errorf("a push payload holds at most %d bytes", MaxPayload)
	}
	uaRaw, _ := decode(p256dh)
	secret, _ := decode(auth)
	ua, err := ecdh.P256().NewPublicKey(uaRaw)
	if err != nil {
		return nil, err
	}
	shared, err := ephemeral.ECDH(ua)
	if err != nil {
		return nil, err
	}
	asPublic := ephemeral.PublicKey().Bytes()
	// IKM = HKDF(auth, ecdh_secret, "WebPush: info" || 0 || ua_public || as_public, 32)
	prkKey, err := hkdf.Extract(sha256.New, shared, secret)
	if err != nil {
		return nil, err
	}
	ikm, err := hkdf.Expand(sha256.New, prkKey, "WebPush: info\x00"+string(uaRaw)+string(asPublic), 32)
	if err != nil {
		return nil, err
	}
	prk, err := hkdf.Extract(sha256.New, ikm, salt)
	if err != nil {
		return nil, err
	}
	cek, err := hkdf.Expand(sha256.New, prk, "Content-Encoding: aes128gcm\x00", 16)
	if err != nil {
		return nil, err
	}
	nonce, err := hkdf.Expand(sha256.New, prk, "Content-Encoding: nonce\x00", 12)
	if err != nil {
		return nil, err
	}
	block, err := aes.NewCipher(cek)
	if err != nil {
		return nil, err
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return nil, err
	}
	// One record: the payload and the last-record delimiter 0x02, no padding.
	record := append(append([]byte(nil), plaintext...), 2)
	var out bytes.Buffer
	out.Write(salt)
	_ = binary.Write(&out, binary.BigEndian, uint32(recordSize))
	out.WriteByte(byte(len(asPublic)))
	out.Write(asPublic)
	out.Write(gcm.Seal(nil, nonce, record, nil))
	return out.Bytes(), nil
}

// pushServices are the hosts the Runner posts to, each matched exactly or as
// a dot-suffix: Apple (Safari, iOS), Google (Chrome, Android), Mozilla
// (Firefox) and Microsoft (Edge).
var pushServices = []string{"push.apple.com", "fcm.googleapis.com", "push.services.mozilla.com", "notify.windows.com"}

// ValidateEndpoint checks that a phone's subscription points at a known push
// service over HTTPS, and returns its host. The phone chooses this URL and the
// Runner posts to it, so anything else is refused.
func ValidateEndpoint(raw string) (string, error) {
	refused := errors.New("notifications only go through Apple's, Google's, Mozilla's or Microsoft's push service")
	if len(raw) > 2048 {
		return "", errors.New("the push endpoint is too long")
	}
	u, err := url.Parse(raw)
	if err != nil || u.Scheme != "https" || u.User != nil || u.Opaque != "" || u.Host == "" {
		return "", refused
	}
	if port := u.Port(); port != "" && port != "443" {
		return "", refused
	}
	host := strings.ToLower(u.Hostname())
	if net.ParseIP(host) != nil {
		return "", refused
	}
	for _, service := range pushServices {
		if host == service || strings.HasSuffix(host, "."+service) {
			return host, nil
		}
	}
	return "", refused
}

// Options are a message's delivery hints for the push service.
type Options struct {
	// TTL is how long the push service keeps a message for an offline phone.
	TTL time.Duration
	// Urgency is very-low, low, normal or high.
	Urgency string
	// Topic replaces an undelivered message with the same topic: at most 32
	// base64url characters.
	Topic string
}

// Sender posts encrypted messages to push services.
type Sender struct {
	Keys   *Keys
	Client *http.Client
	Now    func() time.Time
	// Validate checks an endpoint before every send; ValidateEndpoint unless a test replaces it.
	Validate func(string) (string, error)
}

// NewSender is the production sender: known push services only, over a
// client that refuses to reach this machine, its network or the tailnet.
func NewSender(keys *Keys) *Sender {
	dialer := &net.Dialer{Timeout: 5 * time.Second, Control: refusePrivate}
	transport := &http.Transport{Proxy: nil, DialContext: dialer.DialContext, ForceAttemptHTTP2: true, TLSHandshakeTimeout: 5 * time.Second, MaxIdleConns: 4, IdleConnTimeout: time.Minute}
	client := &http.Client{Transport: transport, Timeout: 10 * time.Second, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	return &Sender{Keys: keys, Client: client, Now: time.Now, Validate: ValidateEndpoint}
}

var tailnet = mustCIDR("100.64.0.0/10")

func mustCIDR(cidr string) *net.IPNet {
	_, n, _ := net.ParseCIDR(cidr)
	return n
}

// refusePrivate is defence in depth behind the host allowlist: whatever a
// push service's name resolves to, the Runner never connects to itself, the
// local network or the tailnet.
func refusePrivate(_, address string, _ syscall.RawConn) error {
	host, _, err := net.SplitHostPort(address)
	if err != nil {
		return err
	}
	ip := net.ParseIP(host)
	if ip == nil || ip.IsLoopback() || ip.IsPrivate() || ip.IsLinkLocalUnicast() || ip.IsLinkLocalMulticast() || ip.IsUnspecified() || ip.IsMulticast() || tailnet.Contains(ip) {
		return fmt.Errorf("refusing to send a notification to %s", host)
	}
	return nil
}

func validTopic(topic string) bool {
	if len(topic) > 32 {
		return false
	}
	for _, r := range topic {
		if !(r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' || r == '-' || r == '_') {
			return false
		}
	}
	return true
}

// Send encrypts payload to the subscription and posts it. It returns ErrGone
// when the push service no longer knows the subscription.
func (s *Sender) Send(ctx context.Context, sub Subscription, payload []byte, opts Options) error {
	validate := s.Validate
	if validate == nil {
		validate = ValidateEndpoint
	}
	if _, err := validate(sub.Endpoint); err != nil {
		return err
	}
	endpoint, err := url.Parse(sub.Endpoint)
	if err != nil {
		return err
	}
	if !validTopic(opts.Topic) {
		return errors.New("a push topic is at most 32 base64url characters")
	}
	switch opts.Urgency {
	case "", "very-low", "low", "normal", "high":
	default:
		return fmt.Errorf("unknown push urgency %q", opts.Urgency)
	}
	body, err := Encrypt(sub.P256dh, sub.Auth, payload)
	if err != nil {
		return err
	}
	now := time.Now
	if s.Now != nil {
		now = s.Now
	}
	auth, err := s.Keys.authorization(endpoint, now())
	if err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, sub.Endpoint, bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Encoding", "aes128gcm")
	req.Header.Set("Content-Type", "application/octet-stream")
	req.Header.Set("TTL", strconv.Itoa(int(opts.TTL/time.Second)))
	req.Header.Set("Authorization", auth)
	if opts.Urgency != "" {
		req.Header.Set("Urgency", opts.Urgency)
	}
	if opts.Topic != "" {
		req.Header.Set("Topic", opts.Topic)
	}
	client := s.Client
	if client == nil {
		client = http.DefaultClient
	}
	res, err := client.Do(req)
	if err != nil {
		return fmt.Errorf("the push service could not be reached: %w", err)
	}
	defer res.Body.Close()
	detail, _ := io.ReadAll(io.LimitReader(res.Body, 512))
	reason := strings.TrimSpace(string(detail))
	if len(reason) > 200 {
		reason = reason[:200]
	}
	if reason != "" {
		reason = " (" + reason + ")"
	}
	switch {
	case res.StatusCode >= 200 && res.StatusCode < 300:
		return nil
	case res.StatusCode == http.StatusNotFound || res.StatusCode == http.StatusGone:
		return ErrGone
	case res.StatusCode == http.StatusUnauthorized || res.StatusCode == http.StatusForbidden:
		return fmt.Errorf("the push service refused this Runner's key%s; turn notifications off and on again on the phone", reason)
	case res.StatusCode == http.StatusRequestEntityTooLarge:
		return errors.New("the push service refused the message as too large")
	case res.StatusCode == http.StatusTooManyRequests:
		return errors.New("the push service is rate limiting this Runner; try again later")
	case res.StatusCode >= 500:
		return fmt.Errorf("the push service failed (%d); it will be tried again", res.StatusCode)
	default:
		return fmt.Errorf("the push service refused the message (%d)%s", res.StatusCode, reason)
	}
}
