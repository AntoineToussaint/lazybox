// Package link is the owner link to a Runner on another computer
// (REMOTE_RUNNERS.md, P0): one byte stream from a client (the desktop app or
// the CLI) to a Runner's link endpoint, a Noise IK handshake that pins the
// Runner's key, a signature by the client's identity key over the handshake
// hash, and then HTTP/2 inside the encrypted channel carrying the Runner's
// Connect RPCs.
//
// The route that carries the stream (SSH stdio today; the tailnet, a relay
// or `docker exec` later) is never trusted: every route lands in the same
// Serve, with the same owner records and the same checks. The Runner opens
// no listener for it: `lazybox link serve --stdio` is the endpoint, run by
// sshd as the person's login, and it proxies to the loopback Runner with the
// owner bearer it reads on that host. The bearer never leaves the host.
package link

import (
	"crypto/rand"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net"
	"sync"
	"time"

	"github.com/flynn/noise"
)

// Protocol is the link protocol's name and version; it is the Noise prologue
// and the domain separator of the identity signature, so a handshake or a
// signature of another protocol (or version) never verifies as this one.
const Protocol = "lazybox-link-v1"

// The parameters V1's crates/e2e-channel uses and MOBILE_CLOUD_HUB's LBR/1
// names: Noise_IK_25519_ChaChaPoly_BLAKE2s, each message framed as
// [u16 big-endian length][message].
var suite = noise.NewCipherSuite(noise.DH25519, noise.CipherChaChaPoly, noise.HashBLAKE2s)

const (
	maxMessage = 65535
	tagSize    = 16
	maxPayload = maxMessage - tagSize
)

// ErrHandshake is a handshake the peer did not complete: a wrong pinned key,
// a client key the Runner does not know, a corrupt stream.
var ErrHandshake = errors.New("the link handshake failed")

func writeFrame(w io.Writer, message []byte) error {
	if len(message) > maxMessage {
		return fmt.Errorf("link message of %d bytes is too large", len(message))
	}
	frame := make([]byte, 2+len(message))
	binary.BigEndian.PutUint16(frame, uint16(len(message)))
	copy(frame[2:], message)
	_, err := w.Write(frame)
	return err
}

func readFrame(r io.Reader) ([]byte, error) {
	var header [2]byte
	if _, err := io.ReadFull(r, header[:]); err != nil {
		return nil, err
	}
	message := make([]byte, binary.BigEndian.Uint16(header[:]))
	if _, err := io.ReadFull(r, message); err != nil {
		if errors.Is(err, io.EOF) {
			err = io.ErrUnexpectedEOF
		}
		return nil, err
	}
	return message, nil
}

// Conn is the encrypted channel after the handshake: a net.Conn whose every
// write is one or more Noise transport messages. It is what HTTP/2 runs on.
type Conn struct {
	stream     io.ReadWriteCloser
	send, recv *noise.CipherState
	wmu, rmu   sync.Mutex
	pending    []byte
	closeOnce  sync.Once
	closed     chan struct{}
	route      string
}

func newConn(stream io.ReadWriteCloser, send, recv *noise.CipherState, route string) *Conn {
	return &Conn{stream: stream, send: send, recv: recv, closed: make(chan struct{}), route: route}
}

// writeMessage sends one transport message (at most maxPayload bytes).
func (c *Conn) writeMessage(plain []byte) error {
	c.wmu.Lock()
	defer c.wmu.Unlock()
	return c.writeLocked(plain)
}

func (c *Conn) writeLocked(plain []byte) error {
	sealed, err := c.send.Encrypt(nil, nil, plain)
	if err != nil {
		return err
	}
	return writeFrame(c.stream, sealed)
}

// readMessage reads one whole transport message.
func (c *Conn) readMessage() ([]byte, error) {
	c.rmu.Lock()
	defer c.rmu.Unlock()
	return c.readLocked()
}

func (c *Conn) readLocked() ([]byte, error) {
	frame, err := readFrame(c.stream)
	if err != nil {
		return nil, err
	}
	plain, err := c.recv.Decrypt(nil, nil, frame)
	if err != nil {
		return nil, fmt.Errorf("%w: a message did not decrypt", ErrHandshake)
	}
	return plain, nil
}

func (c *Conn) Read(p []byte) (int, error) {
	c.rmu.Lock()
	defer c.rmu.Unlock()
	for len(c.pending) == 0 {
		plain, err := c.readLocked()
		if err != nil {
			return 0, err
		}
		c.pending = plain
	}
	n := copy(p, c.pending)
	c.pending = c.pending[n:]
	return n, nil
}

func (c *Conn) Write(p []byte) (int, error) {
	c.wmu.Lock()
	defer c.wmu.Unlock()
	written := 0
	for len(p) > 0 {
		n := min(len(p), maxPayload)
		if err := c.writeLocked(p[:n]); err != nil {
			return written, err
		}
		written += n
		p = p[n:]
	}
	return written, nil
}

// Close closes the stream underneath, which ends any read in progress.
func (c *Conn) Close() error {
	var err error
	c.closeOnce.Do(func() { close(c.closed); err = c.stream.Close() })
	return err
}

// Done is closed once the channel is closed.
func (c *Conn) Done() <-chan struct{} { return c.closed }

type linkAddr string

func (a linkAddr) Network() string { return "lazybox-link" }
func (a linkAddr) String() string  { return string(a) }

func (c *Conn) LocalAddr() net.Addr  { return linkAddr("local") }
func (c *Conn) RemoteAddr() net.Addr { return linkAddr(c.route) }

// Deadlines are not supported by the streams a link runs on (ssh's pipes);
// HTTP/2's own pings detect a dead link and Close ends it.
func (c *Conn) SetDeadline(time.Time) error      { return nil }
func (c *Conn) SetReadDeadline(time.Time) error  { return nil }
func (c *Conn) SetWriteDeadline(time.Time) error { return nil }

func newHandshake(initiator bool, static noise.DHKey, peer []byte) (*noise.HandshakeState, error) {
	return noise.NewHandshakeState(noise.Config{
		CipherSuite:   suite,
		Random:        rand.Reader,
		Pattern:       noise.HandshakeIK,
		Initiator:     initiator,
		Prologue:      []byte(Protocol),
		StaticKeypair: static,
		PeerStatic:    peer,
	})
}
