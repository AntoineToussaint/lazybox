// Package jsonl owns a supervised local JSON-lines connection. It knows
// nothing about agent lifecycle or any provider's method vocabulary.
package jsonl

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/processguard"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/redact"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

type Message struct {
	// JSONRPC is "2.0" on the wire of a peer that requires it (Options.Version).
	JSONRPC string          `json:"jsonrpc,omitempty"`
	ID      json.RawMessage `json:"id,omitempty"`
	Method  string          `json:"method,omitempty"`
	Params  json.RawMessage `json:"params,omitempty"`
	Result  json.RawMessage `json:"result,omitempty"`
	Error   json.RawMessage `json:"error,omitempty"`
	// Seq is the session host's transport cursor (zero for a local pipe).
	// Redelivered marks a request the agent still waits on after a Runner restart.
	Seq         uint64 `json:"-"`
	Redelivered bool   `json:"-"`
}

// Options adapt the connection to a peer's dialect of JSON lines. The zero
// value is the Lazybox workers' and Codex app-server's: no version member,
// and a reply to no call of this Process is dropped.
type Options struct {
	// Version, when set, is written as every message's "jsonrpc" member.
	// JSON-RPC 2.0 peers (ACP agents) reject a message without it.
	Version string
	// Unmatched hands a reply that answers no call of this Process to Receive
	// instead of dropping it: the answer to a request written with Send, or to
	// a call a previous Runner made on the same host before it went away.
	Unmatched bool
}

type Process struct {
	opts    Options
	mu      sync.Mutex
	writeMu sync.Mutex
	stdin   io.WriteCloser
	cmd     *exec.Cmd
	guard   *processguard.Guard
	pending map[string]chan Message
	next    uint64
	events  chan Message
	done    chan struct{}
	exited  chan struct{}
	err     error
	exitErr error
	once    sync.Once
	stderr  *redact.Tail
	// host is set when a detached session host owns the native process.
	host        *sessionhost.Client
	hostBackend sessionhost.Backend
	hostRoot    string
	detached    bool
	closing     bool
	// Close may race the context watcher; terminate exactly once.
	closeHostOnce sync.Once
	closeHostErr  error
	commitMu      sync.Mutex
	commit        uint64
	// delivered is the transport cursor of the last line handed to Receive.
	// After the agent exits, the host is released only once commit reaches
	// it: until then the host still holds output this Runner has not recorded.
	delivered   uint64
	exitSeen    bool
	releaseOnce sync.Once
}

func Start(ctx context.Context, binary string, args, env []string, cwd string) (*Process, error) {
	return StartWith(ctx, binary, args, env, cwd, Options{})
}

// StartWith is Start speaking the dialect opts describes.
func StartWith(ctx context.Context, binary string, args, env []string, cwd string, opts Options) (*Process, error) {
	cmd := exec.Command(binary, args...)
	cmd.Env = env
	cmd.Dir = cwd
	in, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	// Own the read end independently of exec.Cmd.Wait so reaping the helper
	// cannot truncate queued protocol messages, or wait forever for escaped FDs.
	out, output, err := os.Pipe()
	if err != nil {
		in.Close()
		return nil, err
	}
	cmd.Stdout = output
	// Bounded startup diagnostics, with credential values from the resolved
	// child environment removed. The child's stderr never shares the JSON pipe.
	diagnostic := &redact.Tail{}
	diagnostic.Redact(env)
	cmd.Stderr = diagnostic
	guard, err := processguard.Start(cmd)
	output.Close()
	if err != nil {
		in.Close()
		out.Close()
		return nil, err
	}
	p := &Process{opts: opts, stdin: in, cmd: cmd, guard: guard, pending: map[string]chan Message{}, events: make(chan Message, 256), done: make(chan struct{}), exited: make(chan struct{}), stderr: diagnostic}
	readDone := make(chan struct{})
	go func() {
		defer func() { out.Close(); close(readDone) }()
		s := bufio.NewScanner(out)
		s.Buffer(make([]byte, 4096), 16<<20)
		for s.Scan() {
			var m Message
			if err := json.Unmarshal(s.Bytes(), &m); err != nil {
				p.stop(fmt.Errorf("invalid adapter JSON: %w", err))
				return
			}
			if m.Method == "" && len(m.ID) > 0 {
				p.mu.Lock()
				ch := p.pending[string(m.ID)]
				p.mu.Unlock()
				if ch != nil {
					select {
					case ch <- m:
					default:
					}
					continue
				}
				if !p.opts.Unmatched {
					continue
				}
			}
			select {
			case p.events <- m:
			case <-p.done:
				return
			default:
				p.stop(errors.New("adapter event buffer exhausted"))
				return
			}
		}
		err := s.Err()
		_ = guard.Stop()
		_, _ = guard.Wait() // drain stderr before reporting bounded diagnostics
		if err == nil {
			err = io.EOF
		}
		// The process is gone (reaped above): its stream ended with it.
		p.stop(fmt.Errorf("%w: %w", agent.ErrProcessExited, err))
	}()
	go func() {
		_, exitErr := guard.Wait()
		if exitErr != nil {
			p.stop(fmt.Errorf("%w: %w", agent.ErrProcessExited, exitErr))
		}
		timer := time.NewTimer(time.Second)
		defer timer.Stop()
		select {
		case <-readDone:
		case <-timer.C:
			out.Close()
			exitErr = processguard.ErrCleanupUnconfirmed
			p.stop(exitErr)
		}
		p.exitErr = exitErr
		close(p.exited)
	}()
	go func() {
		select {
		case <-ctx.Done():
			p.stop(ctx.Err())
		case <-p.done:
		}
	}()
	return p, nil
}

func (p *Process) stop(err error) {
	p.once.Do(func() {
		p.mu.Lock()
		p.err = err
		if text := p.stderr.Text(); text != "" {
			p.err = fmt.Errorf("%w; adapter stderr: %s", err, text)
		}
		p.mu.Unlock()
		close(p.done)
		if p.host != nil {
			// The host keeps the agent; each hosted path ends its own connection.
			return
		}
		_ = p.stdin.Close()
		_ = p.guard.Stop()
	})
}
func (p *Process) write(m Message) error {
	m.JSONRPC = p.opts.Version
	p.writeMu.Lock()
	defer p.writeMu.Unlock()
	if p.host != nil {
		line, err := json.Marshal(m)
		if err != nil {
			return err
		}
		return p.host.Send(line)
	}
	return json.NewEncoder(p.stdin).Encode(m)
}
func (p *Process) Notify(method string, params any) error {
	b, err := json.Marshal(params)
	if err != nil {
		return err
	}
	return p.write(Message{Method: method, Params: b})
}
func (p *Process) Reply(id json.RawMessage, result any) error {
	b, err := json.Marshal(result)
	if err != nil {
		return err
	}
	return p.write(Message{ID: id, Result: b})
}
func (p *Process) Reject(id json.RawMessage, reason string) error {
	return p.RejectWith(id, -32601, reason)
}

// RejectWith answers a peer's request with a JSON-RPC error of this code.
func (p *Process) RejectWith(id json.RawMessage, code int, reason string) error {
	b, _ := json.Marshal(map[string]any{"code": code, "message": reason})
	return p.write(Message{ID: id, Error: b})
}

// NewID allocates a request id no call of this Process uses, for Request.
func (p *Process) NewID() json.RawMessage {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.next++
	id, _ := json.Marshal(fmt.Sprintf("lazybox-%d", p.next))
	return id
}

// Request writes request id without waiting for its reply. The reply reaches
// Receive (Options.Unmatched) in order with the peer's other lines, so it is
// recorded like them: a request whose answer may outlive this Runner, such as
// a turn that ends long after it starts. The caller records id first, so the
// reply can never arrive before the caller knows what it answers.
func (p *Process) Request(id json.RawMessage, method string, params any) error {
	b, err := json.Marshal(params)
	if err != nil {
		return err
	}
	return p.write(Message{ID: id, Method: method, Params: b})
}

func (p *Process) Call(ctx context.Context, method string, params any) (json.RawMessage, error) {
	p.mu.Lock()
	p.next++
	id, _ := json.Marshal(fmt.Sprintf("lazybox-%d", p.next))
	ch := make(chan Message, 1)
	p.pending[string(id)] = ch
	p.mu.Unlock()
	defer func() { p.mu.Lock(); delete(p.pending, string(id)); p.mu.Unlock() }()
	b, err := json.Marshal(params)
	if err != nil {
		return nil, err
	}
	if err = p.write(Message{ID: id, Method: method, Params: b}); err != nil {
		return nil, err
	}
	select {
	case m := <-ch:
		if len(m.Error) > 0 && string(m.Error) != "null" {
			return nil, NewCallError(method, m.Error)
		}
		return m.Result, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-p.done:
		p.mu.Lock()
		defer p.mu.Unlock()
		return nil, p.err
	}
}

func (p *Process) Receive(ctx context.Context) (Message, error) {
	select {
	case m := <-p.events:
		return m, nil
	default:
	}
	select {
	case m := <-p.events:
		return m, nil
	case <-ctx.Done():
		return Message{}, ctx.Err()
	case <-p.done:
		p.mu.Lock()
		defer p.mu.Unlock()
		return Message{}, p.err
	}
}

// Close waits for the owned native process to be reaped. Closing a pipe or
// sending a kill signal alone is not proof that a replacement writer may start.
// A hosted process is terminated through its host; a detached one is no longer
// this Process's to stop and Close returns immediately.
func (p *Process) Close() error {
	if p.host != nil {
		return p.closeHosted()
	}
	_ = p.stdin.Close()
	timer := time.NewTimer(2 * time.Second)
	defer timer.Stop()
	select {
	case <-p.exited:
		return p.exitErr
	case <-timer.C:
		p.stop(io.EOF)
	}
	timer.Reset(2 * time.Second)
	select {
	case <-p.exited:
		return p.exitErr
	case <-timer.C:
		return errors.New("native process exit remains unconfirmed")
	}
}

// CallError is the error an adapter answered a call with (a JSON-RPC error
// object). It reads as the agent's own message ("API error: 429 This request
// would exceed your account's rate limit…"), never as the raw object: that
// text reaches the person. Method, Code and Data stay for code that decides
// what to do with it.
type CallError struct {
	Method  string
	Code    int
	Message string
	Data    json.RawMessage
	// Raw is the error as the adapter sent it, for an answer that is not an object.
	Raw json.RawMessage
}

// NewCallError reads an adapter's error answer to method.
func NewCallError(method string, raw json.RawMessage) *CallError {
	e := &CallError{Method: method, Raw: append(json.RawMessage(nil), raw...)}
	var object struct {
		Code    int             `json:"code"`
		Message string          `json:"message"`
		Data    json.RawMessage `json:"data"`
	}
	if json.Unmarshal(raw, &object) == nil {
		e.Code, e.Message, e.Data = object.Code, object.Message, object.Data
	} else {
		var text string
		if json.Unmarshal(raw, &text) == nil {
			e.Message = text
		}
	}
	// A JavaScript error's own "Error: " prefix says nothing to the person.
	e.Message = strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(e.Message), "Error: "))
	return e
}

func (e *CallError) Error() string {
	if e.Message != "" {
		return e.Message
	}
	return fmt.Sprintf("the agent refused %s: %s", e.Method, e.Raw)
}
