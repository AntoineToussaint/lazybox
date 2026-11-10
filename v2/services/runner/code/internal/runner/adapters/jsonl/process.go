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
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/processguard"
	"github.com/AntoineToussaint/lazybox/v2/internal/redact"
	"github.com/AntoineToussaint/lazybox/v2/internal/sessionhost"
)

type Message struct {
	ID     json.RawMessage `json:"id,omitempty"`
	Method string          `json:"method,omitempty"`
	Params json.RawMessage `json:"params,omitempty"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  json.RawMessage `json:"error,omitempty"`
	// Seq is the session host's transport cursor (zero for a local pipe).
	// Redelivered marks a request the agent still waits on after a Runner restart.
	Seq         uint64 `json:"-"`
	Redelivered bool   `json:"-"`
}

type Process struct {
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
	host     *sessionhost.Client
	hostRoot string
	detached bool
	closing  bool
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
	p := &Process{stdin: in, cmd: cmd, guard: guard, pending: map[string]chan Message{}, events: make(chan Message, 256), done: make(chan struct{}), exited: make(chan struct{}), stderr: diagnostic}
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
				}
				continue
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
		p.stop(err)
	}()
	go func() {
		_, exitErr := guard.Wait()
		if exitErr != nil {
			p.stop(exitErr)
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
	b, _ := json.Marshal(map[string]any{"code": -32601, "message": reason})
	return p.write(Message{ID: id, Error: b})
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
			return nil, fmt.Errorf("adapter %s: %s", method, m.Error)
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
