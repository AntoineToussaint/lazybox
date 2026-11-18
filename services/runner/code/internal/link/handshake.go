package link

import (
	"context"
	"crypto/ecdsa"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"time"

	"github.com/flynn/noise"
)

// The handshake (REMOTE_RUNNERS §2.4):
//
//	client → msg 1: e, es, s, ss   payload clientHello
//	Runner          looks the client's channel key up in owners.json; unknown: close, no reply
//	Runner → msg 2: e, ee, se      payload runnerHello
//	client → transport: proof      ECDSA-P256(identity, SHA-256(Protocol‖0‖Runner key‖h))
//	Runner → transport: ready      or a refusal, then close
//
// A client that pinned another Runner's key cannot even produce a message 1
// this Runner can read; a Runner without the pinned key cannot answer it.

type clientHello struct {
	Version int    `json:"v"`
	Route   string `json:"route"`
	Name    string `json:"name,omitempty"`
}

// RunnerHello is what the Runner says about itself in message 2.
type RunnerHello struct {
	Version  int    `json:"v"`
	RunnerID string `json:"runnerId,omitempty"`
	Machine  string `json:"machine,omitempty"`
	Release  string `json:"release,omitempty"`
}

type proof struct {
	Signature string `json:"sig"`
}

// Ready is the Runner's answer to the proof: the principal this session is.
type Ready struct {
	Principal string `json:"principal,omitempty"`
	KeyID     string `json:"keyId,omitempty"`
	Name      string `json:"name,omitempty"`
	Error     string `json:"error,omitempty"`
}

// Refusal is why a handshake did not complete, in words a person can act on.
type Refusal struct {
	Reason string
	// Code: "unknown-key" (not or no longer an owner), "wrong-runner" (the
	// pinned key is not this Runner's), "proof" (the identity signature).
	Code string
}

func (r *Refusal) Error() string { return r.Reason }
func (r *Refusal) Unwrap() error { return ErrHandshake }

// handshakeTimeout bounds the whole exchange on either side.
const handshakeTimeout = 20 * time.Second

// ClientHandshake runs the initiator side over stream: it pins runnerKey,
// proves keys, and returns the encrypted channel and what the Runner said.
func ClientHandshake(ctx context.Context, stream io.ReadWriteCloser, keys ClientKeys, runnerKey []byte, route, name string) (*Conn, RunnerHello, Ready, error) {
	if len(runnerKey) != 32 {
		return nil, RunnerHello{}, Ready{}, errors.New("no pinned Runner key")
	}
	stop := closeOnDone(ctx, stream, handshakeTimeout)
	defer stop()
	hs, err := newHandshake(true, keys.Channel, runnerKey)
	if err != nil {
		return nil, RunnerHello{}, Ready{}, err
	}
	hello, _ := json.Marshal(clientHello{Version: 1, Route: route, Name: name})
	msg1, _, _, err := hs.WriteMessage(nil, hello)
	if err != nil {
		return nil, RunnerHello{}, Ready{}, err
	}
	if err = writeFrame(stream, msg1); err != nil {
		return nil, RunnerHello{}, Ready{}, err
	}
	msg2, err := readFrame(stream)
	if err != nil {
		// The Runner closes without a word when it cannot read message 1 (a
		// wrong pin) or does not know this key: the route says which.
		return nil, RunnerHello{}, Ready{}, fmt.Errorf("%w: the Runner closed the link (%v)", ErrHandshake, err)
	}
	payload, send, recv, err := hs.ReadMessage(nil, msg2)
	if err != nil || send == nil || recv == nil {
		return nil, RunnerHello{}, Ready{}, &Refusal{Code: "wrong-runner", Reason: "the computer answered with another Runner's key than the one pinned at enrolment"}
	}
	var runner RunnerHello
	_ = json.Unmarshal(payload, &runner)
	conn := newConn(stream, send, recv, route)
	signature, err := ecdsa.SignASN1(rand.Reader, keys.Identity, signedMessage(runnerKey, hs.ChannelBinding()))
	if err != nil {
		return nil, runner, Ready{}, err
	}
	signed, _ := json.Marshal(proof{Signature: Encode(signature)})
	if err = conn.writeMessage(signed); err != nil {
		return nil, runner, Ready{}, err
	}
	answer, err := conn.readMessage()
	if err != nil {
		return nil, runner, Ready{}, fmt.Errorf("%w: the Runner closed the link after the proof (%v)", ErrHandshake, err)
	}
	var ready Ready
	if err = json.Unmarshal(answer, &ready); err != nil {
		return nil, runner, Ready{}, fmt.Errorf("%w: an unreadable answer", ErrHandshake)
	}
	if ready.Error != "" || ready.Principal == "" {
		return nil, runner, ready, &Refusal{Code: "proof", Reason: firstNonEmpty(ready.Error, "the Runner refused this client")}
	}
	return conn, runner, ready, nil
}

// serverHandshake runs the responder side. owners decides who may connect;
// it is consulted after message 1 and again after the proof.
func serverHandshake(ctx context.Context, stream io.ReadWriteCloser, static noise.DHKey, owners Owners, hello RunnerHello) (*Conn, Owner, clientHello, error) {
	stop := closeOnDone(ctx, stream, handshakeTimeout)
	defer stop()
	hs, err := newHandshake(false, static, nil)
	if err != nil {
		return nil, Owner{}, clientHello{}, err
	}
	msg1, err := readFrame(stream)
	if err != nil {
		return nil, Owner{}, clientHello{}, fmt.Errorf("%w: no handshake (%v)", ErrHandshake, err)
	}
	payload, _, _, err := hs.ReadMessage(nil, msg1)
	if err != nil {
		return nil, Owner{}, clientHello{}, &Refusal{Code: "wrong-runner", Reason: "the client's first message did not decrypt: it pinned another Runner's key"}
	}
	var client clientHello
	_ = json.Unmarshal(payload, &client)
	owner, found, err := owners.Lookup(hs.PeerStatic())
	if err != nil {
		return nil, Owner{}, client, err
	}
	if !found {
		return nil, Owner{}, client, &Refusal{Code: "unknown-key", Reason: fmt.Sprintf("key %s is not an owner of this Runner (never enrolled, or revoked)", KeyID(hs.PeerStatic()))}
	}
	reply, _ := json.Marshal(hello)
	msg2, recv, send, err := hs.WriteMessage(nil, reply)
	if err != nil {
		return nil, Owner{}, client, err
	}
	if err = writeFrame(stream, msg2); err != nil {
		return nil, Owner{}, client, err
	}
	conn := newConn(stream, send, recv, client.Route)
	refuse := func(code, reason string) (*Conn, Owner, clientHello, error) {
		answer, _ := json.Marshal(Ready{Error: reason})
		_ = conn.writeMessage(answer)
		return nil, Owner{}, client, &Refusal{Code: code, Reason: reason}
	}
	message, err := conn.readMessage()
	if err != nil {
		return nil, Owner{}, client, fmt.Errorf("%w: no proof (%v)", ErrHandshake, err)
	}
	var p proof
	signature, decodeErr := []byte(nil), json.Unmarshal(message, &p)
	if decodeErr == nil {
		signature, decodeErr = Decode(p.Signature)
	}
	identityDER, _ := Decode(owner.IdentityKey)
	identity, err := parseIdentity(identityDER)
	if err != nil {
		return refuse("proof", "this owner's identity key on record is unusable; enrol the client again")
	}
	if decodeErr != nil || !ecdsa.VerifyASN1(identity, signedMessage(static.Public, hs.ChannelBinding()), signature) {
		return refuse("proof", "the identity signature does not verify for this key")
	}
	// Revoked between message 1 and the proof: still refused.
	if _, still, err := owners.Lookup(hs.PeerStatic()); err != nil || !still {
		return refuse("unknown-key", "this key was revoked")
	}
	answer, _ := json.Marshal(Ready{Principal: "owner", KeyID: owner.KeyID, Name: owner.Name})
	if err = conn.writeMessage(answer); err != nil {
		return nil, Owner{}, client, err
	}
	return conn, owner, client, nil
}

// closeOnDone closes stream if ctx ends or the handshake takes longer than
// limit, which unblocks a read in progress; stop ends the watch.
func closeOnDone(ctx context.Context, stream io.Closer, limit time.Duration) (stop func()) {
	done := make(chan struct{})
	timer := time.NewTimer(limit)
	go func() {
		defer timer.Stop()
		select {
		case <-ctx.Done():
			_ = stream.Close()
		case <-timer.C:
			_ = stream.Close()
		case <-done:
		}
	}()
	return func() { close(done) }
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}
