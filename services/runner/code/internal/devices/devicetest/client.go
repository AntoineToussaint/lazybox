// Package devicetest is a phone for tests: it holds an ECDSA P-256 key and
// signs pairing and sign-in messages exactly as the browser app does with
// WebCrypto (SPKI public key, IEEE P1363 signatures, base64url).
package devicetest

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
)

// Phone is one device key.
type Phone struct {
	Key       *ecdsa.PrivateKey
	PublicKey string
}

// New makes a phone with a fresh key.
func New() (*Phone, error) {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return nil, err
	}
	der, err := x509.MarshalPKIXPublicKey(&key.PublicKey)
	if err != nil {
		return nil, err
	}
	return &Phone{Key: key, PublicKey: base64.RawURLEncoding.EncodeToString(der)}, nil
}

// Sign signs message as WebCrypto ECDSA/SHA-256 does: r||s, 32 bytes each.
func (p *Phone) Sign(message []byte) string {
	digest := sha256.Sum256(message)
	r, s, err := ecdsa.Sign(rand.Reader, p.Key, digest[:])
	if err != nil {
		panic(err)
	}
	out := make([]byte, 64)
	r.FillBytes(out[:32])
	s.FillBytes(out[32:])
	return base64.RawURLEncoding.EncodeToString(out)
}

// PairRequest answers an offer with this phone's key.
func (p *Phone) PairRequest(pairing, secret, name string) devices.PairRequest {
	return devices.PairRequest{Pairing: pairing, Secret: secret, Name: name, PublicKey: p.PublicKey, Signature: p.Sign(devices.PairMessage(pairing, p.PublicKey))}
}

// Answer signs a sign-in challenge.
func (p *Phone) Answer(device, challenge string) string {
	return p.Sign(devices.SessionMessage(device, challenge))
}
