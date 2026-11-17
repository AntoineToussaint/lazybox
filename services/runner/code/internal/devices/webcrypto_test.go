package devices

import (
	"crypto/ecdsa"
	"crypto/ed25519"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"encoding/base64"
	"testing"
)

// A key and signature produced by WebCrypto (node's globalThis.crypto.subtle,
// the same API a phone's browser runs): generateKey ECDSA P-256, exportKey
// "spki", sign ECDSA SHA-256 over PairMessage("fixed-pairing", spki). The
// Runner must accept exactly that encoding, and nothing else.
const (
	webCryptoKey       = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEW-l1QNdkGUOZgVcVAIly2q6w44hnPg1lhUnMmRFlJbIONJ7_E1I-jYLjfM2kV8b8NLACITH7wzeW5m4GpqnJ7A"
	webCryptoSignature = "E3JEriy1iB-7S3R08OsjStP4tpYz7IDQ0KzHSkRCDn9BK56KfMSjDeOO640I731Ll-3re2vo0gsAU5Pvc0gQtQ"
)

func TestVerifiesWebCryptoSignatures(t *testing.T) {
	key, err := parseKey(webCryptoKey)
	if err != nil {
		t.Fatal(err)
	}
	if !verify(key, PairMessage("fixed-pairing", webCryptoKey), webCryptoSignature) {
		t.Fatal("a WebCrypto ECDSA P-256 signature did not verify")
	}
	if verify(key, PairMessage("other-pairing", webCryptoKey), webCryptoSignature) {
		t.Fatal("a signature verified over another message")
	}
	if verify(key, PairMessage("fixed-pairing", webCryptoKey), webCryptoSignature[:len(webCryptoSignature)-2]) {
		t.Fatal("a truncated signature verified")
	}
}

func TestRejectsKeysThatAreNotP256(t *testing.T) {
	p384, err := ecdsa.GenerateKey(elliptic.P384(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	edPublic, _, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	bad := []string{"", "not base64!", "AAAA"}
	for _, key := range []any{&p384.PublicKey, edPublic} {
		der, err := x509.MarshalPKIXPublicKey(key)
		if err != nil {
			t.Fatal(err)
		}
		bad = append(bad, base64.RawURLEncoding.EncodeToString(der))
	}
	for _, key := range bad {
		if _, err := parseKey(key); err == nil {
			t.Errorf("key %q accepted", key)
		}
	}
}
