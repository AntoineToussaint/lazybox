package link

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"github.com/flynn/noise"
)

// Encode is how keys travel in files, flags and JSON: unpadded base64url, so
// a key is one shell-safe word on an SSH command line.
func Encode(b []byte) string { return base64.RawURLEncoding.EncodeToString(b) }

// Decode reads Encode's form (and tolerates padding or the standard alphabet).
func Decode(s string) ([]byte, error) {
	s = strings.TrimRight(strings.TrimSpace(s), "=")
	s = strings.NewReplacer("+", "-", "/", "_").Replace(s)
	return base64.RawURLEncoding.DecodeString(s)
}

// Fingerprint is how a person compares a key by eye: SHA256 and base64, as
// ssh prints host keys.
func Fingerprint(public []byte) string {
	sum := sha256.Sum256(public)
	return "SHA256:" + base64.RawStdEncoding.EncodeToString(sum[:])
}

// KeyID names a client key in owners.json and the audit: the first 16 bytes
// of its channel key's SHA-256, base64url.
func KeyID(channelPublic []byte) string {
	sum := sha256.Sum256(channelPublic)
	return Encode(sum[:16])
}

// RunnerKeyFile is the Runner's static link key in its state directory.
const RunnerKeyFile = "link-noise.key"

type storedDH struct {
	Private string `json:"private"`
	Public  string `json:"public"`
}

// LoadOrCreateRunnerKey is the Runner's static X25519 key: made once (0600),
// never replaced, so every client that pinned it keeps trusting this Runner.
func LoadOrCreateRunnerKey(state string) (noise.DHKey, error) {
	return loadOrCreateDH(filepath.Join(state, RunnerKeyFile))
}

func loadOrCreateDH(path string) (noise.DHKey, error) {
	if key, err := readDH(path); err == nil {
		return key, nil
	} else if !errors.Is(err, fs.ErrNotExist) {
		return noise.DHKey{}, err
	}
	key, err := suite.GenerateKeypair(rand.Reader)
	if err != nil {
		return noise.DHKey{}, err
	}
	data, err := json.Marshal(storedDH{Private: Encode(key.Private), Public: Encode(key.Public)})
	if err != nil {
		return noise.DHKey{}, err
	}
	if err = os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return noise.DHKey{}, err
	}
	// O_EXCL: two processes making it at once agree on the first one's.
	f, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if errors.Is(err, fs.ErrExist) {
		return readDH(path)
	}
	if err != nil {
		return noise.DHKey{}, err
	}
	_, err = f.Write(data)
	if err = errors.Join(err, f.Sync(), f.Close()); err != nil {
		_ = os.Remove(path)
		return noise.DHKey{}, err
	}
	return key, nil
}

func readDH(path string) (noise.DHKey, error) {
	if err := privateFile(path); err != nil {
		return noise.DHKey{}, err
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return noise.DHKey{}, err
	}
	var stored storedDH
	if err = json.Unmarshal(data, &stored); err != nil {
		return noise.DHKey{}, fmt.Errorf("%s is not a link key: %w", path, err)
	}
	private, err := Decode(stored.Private)
	if err != nil || len(private) != 32 {
		return noise.DHKey{}, fmt.Errorf("%s is not a link key", path)
	}
	public, err := Decode(stored.Public)
	if err != nil || len(public) != 32 {
		return noise.DHKey{}, fmt.Errorf("%s is not a link key", path)
	}
	return noise.DHKey{Private: private, Public: public}, nil
}

// privateFile refuses a key file other users could read.
func privateFile(path string) error {
	info, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if !info.Mode().IsRegular() {
		return fmt.Errorf("%s is not a regular file", path)
	}
	if info.Mode().Perm()&0o077 != 0 {
		return fmt.Errorf("%s can be read by other users; it holds a private key (chmod 600 it)", path)
	}
	return nil
}

// ClientKeys are one client install's keys (REMOTE_RUNNERS §2.4): the X25519
// channel key Noise authenticates, and the P-256 identity key that signs
// every handshake's hash. Neither ever leaves this computer.
type ClientKeys struct {
	Channel  noise.DHKey
	Identity *ecdsa.PrivateKey
}

// ChannelPublic is the channel key's public half, as owners.json records it.
func (k ClientKeys) ChannelPublic() []byte { return k.Channel.Public }

// IdentityPublic is the identity key's public half (PKIX DER).
func (k ClientKeys) IdentityPublic() ([]byte, error) {
	return x509.MarshalPKIXPublicKey(&k.Identity.PublicKey)
}

// ID is this client's key id.
func (k ClientKeys) ID() string { return KeyID(k.Channel.Public) }

// KeyProvider holds a client's keys: a 0600 file today; the Keychain or the
// Secure Enclave behind the same interface later (REMOTE_RUNNERS decision 10).
type KeyProvider interface {
	Keys() (ClientKeys, error)
}

// FileKeys keeps a client's keys in one 0600 file in a 0700 directory
// (`<config>/keys/link-client.json`), made on first use.
type FileKeys struct{ Path string }

type storedClient struct {
	Channel  storedDH `json:"channel"`
	Identity string   `json:"identity"`
}

func (f FileKeys) Keys() (ClientKeys, error) {
	if keys, err := f.read(); err == nil {
		return keys, nil
	} else if !errors.Is(err, fs.ErrNotExist) {
		return ClientKeys{}, err
	}
	channel, err := suite.GenerateKeypair(rand.Reader)
	if err != nil {
		return ClientKeys{}, err
	}
	identity, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return ClientKeys{}, err
	}
	der, err := x509.MarshalPKCS8PrivateKey(identity)
	if err != nil {
		return ClientKeys{}, err
	}
	data, err := json.MarshalIndent(storedClient{Channel: storedDH{Private: Encode(channel.Private), Public: Encode(channel.Public)}, Identity: Encode(der)}, "", "  ")
	if err != nil {
		return ClientKeys{}, err
	}
	if err = os.MkdirAll(filepath.Dir(f.Path), 0o700); err != nil {
		return ClientKeys{}, err
	}
	file, err := os.OpenFile(f.Path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if errors.Is(err, fs.ErrExist) {
		return f.read()
	}
	if err != nil {
		return ClientKeys{}, err
	}
	_, err = file.Write(append(data, '\n'))
	if err = errors.Join(err, file.Sync(), file.Close()); err != nil {
		_ = os.Remove(f.Path)
		return ClientKeys{}, err
	}
	return ClientKeys{Channel: channel, Identity: identity}, nil
}

func (f FileKeys) read() (ClientKeys, error) {
	if err := privateFile(f.Path); err != nil {
		return ClientKeys{}, err
	}
	data, err := os.ReadFile(f.Path)
	if err != nil {
		return ClientKeys{}, err
	}
	var stored storedClient
	if err = json.Unmarshal(data, &stored); err != nil {
		return ClientKeys{}, fmt.Errorf("%s is not a client key file: %w", f.Path, err)
	}
	private, err1 := Decode(stored.Channel.Private)
	public, err2 := Decode(stored.Channel.Public)
	der, err3 := Decode(stored.Identity)
	if err = errors.Join(err1, err2, err3); err != nil || len(private) != 32 || len(public) != 32 {
		return ClientKeys{}, fmt.Errorf("%s is not a client key file", f.Path)
	}
	parsed, err := x509.ParsePKCS8PrivateKey(der)
	if err != nil {
		return ClientKeys{}, fmt.Errorf("%s: %w", f.Path, err)
	}
	identity, ok := parsed.(*ecdsa.PrivateKey)
	if !ok || identity.Curve != elliptic.P256() {
		return ClientKeys{}, fmt.Errorf("%s: the identity key is not P-256", f.Path)
	}
	return ClientKeys{Channel: noise.DHKey{Private: private, Public: public}, Identity: identity}, nil
}

// parseIdentity reads an enrolled identity public key.
func parseIdentity(der []byte) (*ecdsa.PublicKey, error) {
	parsed, err := x509.ParsePKIXPublicKey(der)
	if err != nil {
		return nil, err
	}
	key, ok := parsed.(*ecdsa.PublicKey)
	if !ok || key.Curve != elliptic.P256() {
		return nil, errors.New("the identity key is not P-256")
	}
	return key, nil
}

// signedMessage is what a client's identity key signs: the protocol, the
// Runner's static key (what the client pinned) and the handshake hash, so a
// signature is good for this handshake with this Runner only.
func signedMessage(runnerKey, handshakeHash []byte) []byte {
	sum := sha256.New()
	sum.Write([]byte(Protocol))
	sum.Write([]byte{0})
	sum.Write(runnerKey)
	sum.Write(handshakeHash)
	return sum.Sum(nil)
}
