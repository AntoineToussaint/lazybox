package jsonl

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"math/rand/v2"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/processguard"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/redact"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

// ErrDetached is the stream error after this Runner detached from its host.
var ErrDetached = sessionhost.ErrDetached

// Host is a launched session host whose agent has not started yet. Callers that
// route provider traffic through a Runner-local gateway point the agent at
// ProxyURL, which stays valid for the life of the session.
type Host struct {
	root    string
	backend sessionhost.Backend
	client  *sessionhost.Client
}

// Launch starts a detached session host under root.
func Launch(ctx context.Context, root string) (*Host, error) {
	return LaunchOn(ctx, nil, root, sessionhost.Options{})
}

// LaunchOn starts a session host under root with backend (the default when
// nil), recording opts for the Runner's process inventory.
func LaunchOn(ctx context.Context, backend sessionhost.Backend, root string, opts sessionhost.Options) (*Host, error) {
	backend = sessionhost.Use(backend)
	c, err := backend.Launch(ctx, root, opts)
	if err != nil {
		return nil, err
	}
	return &Host{root: root, backend: backend, client: c}, nil
}

func (h *Host) ProxyURL() string { return h.client.ProxyURL() }

// Abandon stops a host whose agent will not be started.
func (h *Host) Abandon() {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	_ = h.backend.Stop(ctx, h.root, h.client.ID())
	_ = h.client.Detach()
}

// Start spawns the agent inside the host. As for a local pipe, cancelling ctx
// stops the agent: only an explicit Detach (or the Runner dying outright)
// leaves it running for a later Runner.
func (h *Host) Start(ctx context.Context, binary string, args, env []string, cwd string) (*Process, error) {
	return h.StartWith(ctx, binary, args, env, cwd, Options{})
}

// StartWith is Start speaking the dialect opts describes.
func (h *Host) StartWith(ctx context.Context, binary string, args, env []string, cwd string, opts Options) (*Process, error) {
	if err := h.client.Spawn(ctx, sessionhost.Spec{Path: binary, Args: args, Env: env, Cwd: cwd}); err != nil {
		h.Abandon()
		return nil, err
	}
	return hosted(ctx, h.backend, h.root, h.client, 0, opts), nil
}

// Attach re-connects to a host started by an earlier Runner. ack is the highest
// transport cursor this Runner has durably recorded.
func Attach(ctx context.Context, root, id string, ack uint64) (*Process, error) {
	return AttachOn(ctx, nil, root, id, ack)
}

// AttachOn is Attach through backend (the default when nil). Any backend
// re-attaches a host any other one started.
func AttachOn(ctx context.Context, backend sessionhost.Backend, root, id string, ack uint64) (*Process, error) {
	return AttachWith(ctx, backend, root, id, ack, Options{})
}

// AttachWith is AttachOn speaking the dialect opts describes; a host is
// re-attached with the options it was started with.
func AttachWith(ctx context.Context, backend sessionhost.Backend, root, id string, ack uint64, opts Options) (*Process, error) {
	backend = sessionhost.Use(backend)
	c, err := backend.Attach(ctx, root, id, ack)
	if err != nil {
		return nil, err
	}
	if !c.Spawned() {
		_ = c.Detach()
		return nil, errors.New("session host never started its agent")
	}
	p := hosted(ctx, backend, root, c, ack, opts)
	// The previous Runner's calls may still be answered on this host. Start
	// this Runner's request IDs elsewhere, so a late reply to one of them is
	// never taken for the answer to a new call.
	p.mu.Lock()
	p.next = rand.Uint64() >> 1
	p.mu.Unlock()
	return p, nil
}

// hosted wraps a host connection. ack is the cursor already recorded, set
// before the reader starts.
func hosted(ctx context.Context, backend sessionhost.Backend, root string, c *sessionhost.Client, ack uint64, opts Options) *Process {
	p := &Process{opts: opts, host: c, hostBackend: backend, hostRoot: root, commit: ack, pending: map[string]chan Message{}, events: make(chan Message, 256), done: make(chan struct{}), exited: make(chan struct{}), stderr: &redact.Tail{}}
	go p.readHosted()
	go func() {
		select {
		case <-ctx.Done():
			_ = p.Close() // A no-op once detached.
		case <-p.done:
		}
	}()
	return p
}

func (p *Process) readHosted() {
	var exitErr error
	defer func() { p.exitErr = exitErr; close(p.exited) }()
	for {
		m, exit, err := p.host.Next(context.Background())
		if err != nil {
			p.mu.Lock()
			detached, closing := p.detached, p.closing
			p.mu.Unlock()
			switch {
			case detached:
				p.stop(sessionhost.ErrDetached)
			case closing:
				p.stop(errors.New("native process closed"))
			case errors.Is(err, sessionhost.ErrSuperseded):
				// Another Runner adopted the agent: never stop it from here.
				p.mu.Lock()
				p.detached = true
				p.mu.Unlock()
				p.stop(sessionhost.ErrDetached)
			default:
				// The host vanished: nothing confirms the agent and its group stopped.
				exitErr = processguard.ErrCleanupUnconfirmed
				p.stop(fmt.Errorf("session host connection lost: %w", err))
			}
			return
		}
		if exit != nil {
			if exit.Outcome.Error != "" {
				exitErr = errors.New(exit.Outcome.Error)
			}
			reason := fmt.Errorf("%w (code %d%s)", agent.ErrProcessExited, exit.Outcome.ExitCode, signalSuffix(exit.Outcome.Signal))
			if exit.Stderr != "" {
				reason = fmt.Errorf("%w; adapter stderr: %s", reason, exit.Stderr)
			}
			p.stop(reason)
			p.commitMu.Lock()
			p.exitSeen = true
			recorded := p.commit >= p.delivered
			p.commitMu.Unlock()
			if recorded {
				p.release()
			}
			return
		}
		msg, err := decodeMessage(m.Line)
		if err != nil {
			// JSON that is not a message (`42`, a string, an array): nothing
			// to deliver. Stopping here would leave the line at the head of
			// the host's log, failing every later attach on it. It is kept
			// as a diagnostic and acknowledged in order (skip), and the
			// agent goes on being supervised.
			_, _ = fmt.Fprintf(p.stderr, "\nignored agent output that is not a message (%v): %s", err, clip(m.Line))
			p.skip(m.Seq)
			continue
		}
		msg.Seq, msg.Redelivered = m.Seq, m.Redelivered
		if msg.Method == "" && len(msg.ID) > 0 {
			p.mu.Lock()
			ch := p.pending[string(msg.ID)]
			p.mu.Unlock()
			if ch != nil {
				select {
				case ch <- msg:
				default:
				}
				// A reply has no event; it is recorded once the next event commits.
				continue
			}
			if !p.opts.Unmatched {
				continue
			}
		}
		p.commitMu.Lock()
		p.delivered = m.Seq
		p.commitMu.Unlock()
		// Unlike a local pipe, a hosted stream applies backpressure instead of
		// failing: the host keeps the output until this Runner catches up.
		select {
		case p.events <- msg:
		case <-p.done:
			return
		}
	}
}

// decodeMessage reads one line as a message: a JSON object of the shape
// Message describes. `null` decodes into an empty struct without error, so
// the object is required explicitly.
func decodeMessage(line json.RawMessage) (Message, error) {
	var msg Message
	trimmed := bytes.TrimLeft(line, " \t\r\n")
	if len(trimmed) == 0 || trimmed[0] != '{' {
		return msg, errors.New("not a JSON object")
	}
	err := json.Unmarshal(line, &msg)
	return msg, err
}

// clip bounds a line quoted in a diagnostic.
func clip(line []byte) string {
	const limit = 256
	if len(line) > limit {
		return string(line[:limit]) + "…"
	}
	return string(line)
}

// skip acknowledges a line that yields nothing to record. Acknowledgements are
// cumulative, so it is acknowledged at once only when every line delivered
// before it is recorded; otherwise Commit acknowledges it with them.
func (p *Process) skip(seq uint64) {
	p.commitMu.Lock()
	p.skipped = max(p.skipped, seq)
	ack := p.commit >= p.delivered && seq > p.commit
	if ack {
		p.commit = seq
		_ = p.host.Ack(seq)
	}
	p.commitMu.Unlock()
}

func signalSuffix(signal string) string {
	if signal == "" {
		return ""
	}
	return ", signal " + signal
}

// HostID identifies the session host for re-attachment ("" for a local pipe).
func (p *Process) HostID() string {
	if p.host == nil {
		return ""
	}
	return p.host.ID()
}

// SetUpstream points the host's provider proxy at this Runner's gateway.
func (p *Process) SetUpstream(url string) error {
	if p.host == nil {
		return errors.New("not hosted")
	}
	return p.host.SetUpstream(url)
}

// Commit acknowledges every line up to seq as durably recorded by the Runner,
// letting the host release it. Cumulative and monotonic; zero is ignored.
// Once the agent has exited and its last delivered line is recorded, the host
// has nothing left to keep and is released.
func (p *Process) Commit(seq uint64) {
	if p.host == nil || seq == 0 {
		return
	}
	p.commitMu.Lock()
	if seq <= p.commit {
		p.commitMu.Unlock()
		return
	}
	if seq >= p.delivered && p.skipped > seq {
		// Everything delivered is recorded: lines skipped since go too.
		seq = p.skipped
	}
	p.commit = seq
	_ = p.host.Ack(seq)
	done := p.exitSeen && seq >= p.delivered
	p.commitMu.Unlock()
	if done {
		p.release()
	}
}

// Resolve tells the host the peer no longer waits on its request id although
// no reply was written (the peer resolved it itself, or its turn ended), so a
// later Runner is not handed it again. A local pipe redelivers nothing.
func (p *Process) Resolve(id json.RawMessage) error {
	if p.host == nil || len(id) == 0 {
		return nil
	}
	return p.host.Resolve(id)
}

// release lets the host of an exited agent go away and ends this connection.
func (p *Process) release() {
	p.releaseOnce.Do(func() {
		_ = p.host.Release()
		_ = p.host.Detach()
	})
}

// Detach leaves a hosted agent running for a later Runner. It is an error for
// a local pipe, whose process cannot outlive this Runner.
func (p *Process) Detach() error {
	if p.host == nil {
		return errors.New("a local process cannot be detached")
	}
	p.mu.Lock()
	p.detached = true
	p.mu.Unlock()
	p.stop(sessionhost.ErrDetached)
	return p.host.Detach()
}

func (p *Process) closeHosted() error {
	p.closeHostOnce.Do(func() { p.closeHostErr = p.terminateHosted() })
	return p.closeHostErr
}

func (p *Process) terminateHosted() error {
	p.mu.Lock()
	detached := p.detached
	p.closing = true
	p.mu.Unlock()
	if detached {
		return nil
	}
	select {
	case <-p.exited:
		// Closing is the end of this Runner's interest, recorded or not.
		p.commitMu.Lock()
		exitSeen := p.exitSeen
		p.commitMu.Unlock()
		if exitSeen {
			p.release()
			return p.exitErr
		}
		if errors.Is(p.exitErr, processguard.ErrCleanupUnconfirmed) {
			// The connection to the host was lost, not the agent's exit seen:
			// ask the host to stop it. A host that is gone took its agent's
			// group with it, which confirms the exit; one that answers is
			// stopped now. Either way a close can succeed.
			ctx, cancel := context.WithTimeout(context.Background(), 12*time.Second)
			defer cancel()
			if err := p.hostBackend.Stop(ctx, p.hostRoot, p.host.ID()); err != nil && !sessionhost.Gone(err) {
				return fmt.Errorf("native exit remains unconfirmed: %w", err)
			}
			return nil
		}
		return p.exitErr
	default:
	}
	ctx, cancel := context.WithTimeout(context.Background(), 12*time.Second)
	defer cancel()
	// A fresh control connection supersedes this one, terminates the agent,
	// waits for its exit and releases the host.
	err := p.hostBackend.Stop(ctx, p.hostRoot, p.host.ID())
	p.stop(errors.New("native process closed"))
	_ = p.host.Detach()
	select {
	case <-p.exited:
	case <-time.After(2 * time.Second):
	}
	if err != nil {
		// The exit may have arrived while Stop ran: the host then went away
		// on its own, and the exit this Process observed is the confirmation.
		p.commitMu.Lock()
		exitSeen := p.exitSeen
		p.commitMu.Unlock()
		if exitSeen {
			p.release()
			return p.exitErr
		}
		return fmt.Errorf("native exit remains unconfirmed: %w", err)
	}
	return nil
}
