// Package webpushtest is a fake push service for tests: it holds a browser's
// keys, decrypts what the Runner sends (RFC 8291) and checks the VAPID
// header (RFC 8292) the way Apple's and Google's push services do.
package webpushtest

import (
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
	"math/big"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync"
	"time"
)

// Browser is a push subscriber's key material.
type Browser struct {
	Private *ecdh.PrivateKey
	Secret  []byte
}

// NewBrowser makes fresh subscriber keys.
func NewBrowser() *Browser {
	key, err := ecdh.P256().GenerateKey(rand.Reader)
	if err != nil {
		panic(err)
	}
	secret := make([]byte, 16)
	_, _ = rand.Read(secret)
	return &Browser{Private: key, Secret: secret}
}

// P256dh and Auth are the subscription's keys, base64url.
func (b *Browser) P256dh() string {
	return base64.RawURLEncoding.EncodeToString(b.Private.PublicKey().Bytes())
}
func (b *Browser) Auth() string { return base64.RawURLEncoding.EncodeToString(b.Secret) }

// Decrypt reads an aes128gcm body sent to this browser.
func (b *Browser) Decrypt(body []byte) ([]byte, error) {
	if len(body) < 21 {
		return nil, errors.New("short body")
	}
	salt, rs, idlen := body[:16], binary.BigEndian.Uint32(body[16:20]), int(body[20])
	if rs < 18 || len(body) < 21+idlen {
		return nil, errors.New("bad header")
	}
	asPublic, ciphertext := body[21:21+idlen], body[21+idlen:]
	peer, err := ecdh.P256().NewPublicKey(asPublic)
	if err != nil {
		return nil, err
	}
	shared, err := b.Private.ECDH(peer)
	if err != nil {
		return nil, err
	}
	prkKey, _ := hkdf.Extract(sha256.New, shared, b.Secret)
	ikm, _ := hkdf.Expand(sha256.New, prkKey, "WebPush: info\x00"+string(b.Private.PublicKey().Bytes())+string(asPublic), 32)
	prk, _ := hkdf.Extract(sha256.New, ikm, salt)
	cek, _ := hkdf.Expand(sha256.New, prk, "Content-Encoding: aes128gcm\x00", 16)
	nonce, _ := hkdf.Expand(sha256.New, prk, "Content-Encoding: nonce\x00", 12)
	block, err := aes.NewCipher(cek)
	if err != nil {
		return nil, err
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return nil, err
	}
	plain, err := gcm.Open(nil, nonce, ciphertext, nil)
	if err != nil {
		return nil, err
	}
	end := len(plain) - 1
	for end >= 0 && plain[end] == 0 {
		end--
	}
	if end < 0 || plain[end] != 2 {
		return nil, errors.New("missing last-record delimiter")
	}
	return plain[:end], nil
}

// Message is one delivery the fake service accepted.
type Message struct {
	Path    string
	Payload []byte
	Header  http.Header
}

// Service is a fake push service on a TLS test server.
type Service struct {
	*httptest.Server
	// PublicKey is the VAPID key the Runner must sign with.
	PublicKey string
	mu        sync.Mutex
	browsers  map[string]*Browser
	messages  []Message
	status    map[string]int
	Failures  []string
}

// NewService starts a fake push service expecting VAPID from publicKey.
func NewService(publicKey string) *Service {
	s := &Service{PublicKey: publicKey, browsers: map[string]*Browser{}, status: map[string]int{}}
	s.Server = httptest.NewTLSServer(http.HandlerFunc(s.serve))
	return s
}

// Subscribe registers a browser at a path and returns its endpoint URL.
func (s *Service) Subscribe(path string, b *Browser) string {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.browsers["/"+path] = b
	return s.URL + "/" + path
}

// Respond makes deliveries to path answer with code (e.g. 410).
func (s *Service) Respond(path string, code int) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.status["/"+path] = code
}

// Messages are the deliveries so far.
func (s *Service) Messages() []Message {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append([]Message(nil), s.messages...)
}

func (s *Service) fail(w http.ResponseWriter, format string, args ...any) {
	s.mu.Lock()
	s.Failures = append(s.Failures, fmt.Sprintf(format, args...))
	s.mu.Unlock()
	http.Error(w, fmt.Sprintf(format, args...), http.StatusBadRequest)
}

func (s *Service) serve(w http.ResponseWriter, r *http.Request) {
	s.mu.Lock()
	b, code := s.browsers[r.URL.Path], s.status[r.URL.Path]
	s.mu.Unlock()
	if code != 0 {
		w.WriteHeader(code)
		return
	}
	if b == nil {
		w.WriteHeader(http.StatusGone)
		return
	}
	if r.Method != http.MethodPost || r.Header.Get("Content-Encoding") != "aes128gcm" || r.Header.Get("TTL") == "" {
		s.fail(w, "bad method or headers: %s %v", r.Method, r.Header)
		return
	}
	if err := s.checkVAPID(r); err != nil {
		w.WriteHeader(http.StatusForbidden)
		s.mu.Lock()
		s.Failures = append(s.Failures, err.Error())
		s.mu.Unlock()
		return
	}
	body, _ := io.ReadAll(r.Body)
	payload, err := b.Decrypt(body)
	if err != nil {
		s.fail(w, "decrypt: %v", err)
		return
	}
	s.mu.Lock()
	s.messages = append(s.messages, Message{Path: strings.TrimPrefix(r.URL.Path, "/"), Payload: payload, Header: r.Header.Clone()})
	s.mu.Unlock()
	w.WriteHeader(http.StatusCreated)
}

func (s *Service) checkVAPID(r *http.Request) error {
	value, ok := strings.CutPrefix(r.Header.Get("Authorization"), "vapid ")
	if !ok {
		return errors.New("no vapid authorization")
	}
	var token, key string
	for _, part := range strings.Split(value, ",") {
		part = strings.TrimSpace(part)
		if v, ok := strings.CutPrefix(part, "t="); ok {
			token = v
		} else if v, ok := strings.CutPrefix(part, "k="); ok {
			key = v
		}
	}
	if key != s.PublicKey {
		return errors.New("vapid key does not match the subscription's applicationServerKey")
	}
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		return errors.New("malformed JWT")
	}
	raw, err := base64.RawURLEncoding.DecodeString(key)
	if err != nil {
		return err
	}
	pub, err := ecdsa.ParseUncompressedPublicKey(elliptic.P256(), raw)
	if err != nil {
		return err
	}
	signature, err := base64.RawURLEncoding.DecodeString(parts[2])
	if err != nil || len(signature) != 64 {
		return errors.New("bad JWT signature encoding")
	}
	digest := sha256.Sum256([]byte(parts[0] + "." + parts[1]))
	if !ecdsa.Verify(pub, digest[:], new(big.Int).SetBytes(signature[:32]), new(big.Int).SetBytes(signature[32:])) {
		return errors.New("JWT signature does not verify")
	}
	header, _ := base64.RawURLEncoding.DecodeString(parts[0])
	if !strings.Contains(string(header), `"ES256"`) {
		return errors.New("JWT is not ES256")
	}
	claimsJSON, _ := base64.RawURLEncoding.DecodeString(parts[1])
	var claims struct {
		Aud string `json:"aud"`
		Exp int64  `json:"exp"`
		Sub string `json:"sub"`
	}
	if err = json.Unmarshal(claimsJSON, &claims); err != nil {
		return err
	}
	u, _ := url.Parse(s.URL)
	if claims.Aud != u.Scheme+"://"+u.Host {
		return fmt.Errorf("JWT aud %q is not the push service origin", claims.Aud)
	}
	exp := time.Unix(claims.Exp, 0)
	if exp.Before(time.Now()) || exp.After(time.Now().Add(24*time.Hour)) {
		return fmt.Errorf("JWT exp %v out of range", exp)
	}
	if !strings.HasPrefix(claims.Sub, "https:") && !strings.HasPrefix(claims.Sub, "mailto:") {
		return errors.New("JWT sub is not a contact URL")
	}
	return nil
}
