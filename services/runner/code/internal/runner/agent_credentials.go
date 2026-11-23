package runner

import (
	"context"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

// Agents never hold the owner's bearer. Each execution gets a credential
// bound to its own id, which the transport accepts only for publishing that
// execution's console output (`lazybox run`, `lazybox log`). It is an HMAC of
// the execution id under a key kept in the Runner state, so it needs no
// storage, survives a Runner restart for agents that outlive the Runner, and
// stops working once the conversation is deleted.

// agentTokenPrefix marks a scoped agent credential; the owner token never has it.
const agentTokenPrefix = "lbx-agent."

// agentKeyBytes is the size of the Runner secret the credentials derive from.
const agentKeyBytes = 32

// agentEndpointDir holds one endpoint file per execution, private to the user.
func (b *Bench) agentEndpointDir() string { return filepath.Join(b.Config.State, "agent-endpoints") }

// AgentEndpoint is the file an execution's LAZYBOX_V2_ENDPOINT names: the
// Runner URL and that execution's scoped credential, never the owner's.
func (b *Bench) AgentEndpoint(id string) string {
	return filepath.Join(b.agentEndpointDir(), id+".json")
}

// loadAgentKey reads the Runner secret, creating it on first use. A key that
// already exists is never replaced: live agents hold credentials derived from it.
func loadAgentKey(path string) ([]byte, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		key := make([]byte, agentKeyBytes)
		if _, err = rand.Read(key); err != nil {
			return nil, err
		}
		var f *os.File
		f, err = os.CreateTemp(filepath.Dir(path), ".agent-credential-*")
		if err != nil {
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
		// Link, not rename: it refuses to replace a key that appeared meanwhile.
		if err = os.Link(f.Name(), path); err != nil && !errors.Is(err, fs.ErrExist) {
			return nil, err
		}
		data, err = os.ReadFile(path)
	}
	if err != nil {
		return nil, err
	}
	if len(data) != agentKeyBytes {
		return nil, errors.New("agent credential key is corrupt; stop every agent before removing it")
	}
	return data, nil
}

func (b *Bench) agentMAC(id string) []byte {
	mac := hmac.New(sha256.New, b.agentKey)
	mac.Write([]byte("lazybox v2 agent credential\x00"))
	mac.Write([]byte(id))
	return mac.Sum(nil)
}

// agentToken is the credential for one execution.
func (b *Bench) agentToken(id string) string {
	return agentTokenPrefix + id + "." + base64.RawURLEncoding.EncodeToString(b.agentMAC(id))
}

// AgentSession returns the execution a scoped credential was issued to. It
// fails for a forged credential and for one whose conversation was deleted.
func (b *Bench) AgentSession(ctx context.Context, token string) (string, error) {
	rest, ok := strings.CutPrefix(token, agentTokenPrefix)
	if !ok {
		return "", errors.New("not an agent credential")
	}
	dot := strings.LastIndexByte(rest, '.')
	if dot <= 0 {
		return "", errors.New("malformed agent credential")
	}
	id, encoded := rest[:dot], rest[dot+1:]
	presented, err := base64.RawURLEncoding.DecodeString(encoded)
	if err != nil || !validExecutionID(id) || !hmac.Equal(presented, b.agentMAC(id)) {
		return "", errors.New("invalid agent credential")
	}
	deleted, err := b.History.Deleted(ctx, id)
	if err != nil {
		return "", err
	}
	if len(deleted) > 0 {
		return "", errors.New("agent credential revoked")
	}
	return id, nil
}

// CommandSession returns the execution that owns a command or pipe.
func (b *Bench) CommandSession(id string) (string, error) {
	c, err := b.Commands.Get(id)
	if err != nil {
		return "", err
	}
	return c.Status().Spec.SessionID, nil
}

func validExecutionID(id string) bool {
	return id != "" && id != "." && id != ".." && filepath.Base(id) == id && !strings.ContainsAny(id, ".\x00")
}

// writeAgentEndpoint (re)writes an execution's endpoint file with this
// Runner's URL. Opening and adopting an execution both call it, so an agent
// that outlived its Runner reaches the new one at the same path.
func (b *Bench) writeAgentEndpoint(id string) error {
	if !validExecutionID(id) {
		return errors.New("invalid execution identity")
	}
	dir := b.agentEndpointDir()
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	data, err := json.Marshal(map[string]string{"url": b.Config.Address, "token": b.agentToken(id), "sessionId": id})
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(dir, ".endpoint-*")
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
	return os.Rename(f.Name(), b.AgentEndpoint(id))
}

// removeAgentEndpoint deletes an execution's endpoint file. The credential is
// revoked by the deletion record itself; this only removes the copy on disk.
func (b *Bench) removeAgentEndpoint(id string) error {
	if !validExecutionID(id) {
		return errors.New("invalid execution identity")
	}
	if err := os.Remove(b.AgentEndpoint(id)); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return err
	}
	return nil
}

// runnerWiring reports a variable that configures this Runner rather than the
// person's environment: LAZYBOX_V2_* (its state, config, or an owner endpoint
// from the shell that started it) and CODEFLY* (the service wiring Codefly
// gives the Runner: its credentials, its endpoints and its container recovery
// marker, which a `codefly` an agent runs would refuse as another process's).
// Workspace terminals drop the same (terminals.Environment).
func runnerWiring(entry string) bool {
	return strings.HasPrefix(entry, "LAZYBOX_V2_") || strings.HasPrefix(entry, "CODEFLY")
}

// childEnv is this process's environment without the Runner's wiring: the base
// of anything the Runner starts on the person's behalf (agents, account
// probes, provider CLIs).
func childEnv() []string {
	env := make([]string, 0, 64)
	for _, entry := range os.Environ() {
		if !runnerWiring(entry) {
			env = append(env, entry)
		}
	}
	return env
}

// executionEnv is the Lazybox part of every agent's and managed command's
// environment: this Runner's CLI first on PATH, the execution's own endpoint
// file, its identity and its output directory. The Runner's own wiring
// (runnerWiring) is dropped first.
func (b *Bench) executionEnv(base []string, id, collectionID string) []string {
	env := make([]string, 0, len(base)+4)
	for _, entry := range base {
		if !runnerWiring(entry) {
			env = append(env, entry)
		}
	}
	env = append(b.withCLI(env), "LAZYBOX_V2_ENDPOINT="+b.AgentEndpoint(id), "LAZYBOX_V2_SESSION_ID="+id, b.outputEnv(id))
	if collectionID != "" {
		env = append(env, "LAZYBOX_V2_COLLECTION_ID="+collectionID)
	}
	return env
}
