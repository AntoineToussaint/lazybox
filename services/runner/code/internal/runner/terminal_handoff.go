package runner

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"errors"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/terminals"
)

type terminalHandoff struct {
	mu           sync.Mutex
	decided      chan struct{}
	ctx          context.Context
	collectionID string
	cwd          string
	argv         []string
	env          []string
	terminal     *terminals.Terminal
	rejected     bool
}

func (b *Bench) requestTerminalHandoff(ctx context.Context, e *Execution, argv []string, cwd string, env []string) (any, error) {
	if e.CollectionID == "" || e.Options.Permission == "read-only" || e.Options.Workflow == "plan" || agent.IsAssistant(e.Purpose) {
		return nil, errors.New("interactive commands require a writable workspace conversation")
	}
	data, err := json.Marshal(argv)
	if err != nil {
		return nil, err
	}
	if len(argv) == 0 || argv[0] == "" || strings.HasPrefix(argv[0], "-") || len(data) > 8192 || strings.ContainsRune(strings.Join(argv, ""), 0) {
		return nil, errors.New("interactive commands require executable arguments of at most 8 KiB")
	}
	dir, err := commandDir(e.Cwd, cwd)
	if err != nil {
		return nil, err
	}
	if env == nil {
		env = e.Env
	}
	approval, cancel := context.WithTimeout(ctx, 10*time.Minute)
	defer cancel()
	id := rand.Text()
	h := &terminalHandoff{
		decided: make(chan struct{}), ctx: approval, collectionID: e.CollectionID,
		cwd: dir, argv: argv, env: terminals.Environment(env, e.CollectionID),
	}
	b.terminalHandoffs.Store(id, h)
	defer b.terminalHandoffs.Delete(id)
	record := func(status, text string, data json.RawMessage) error {
		return b.Record(e, agent.Event{Type: "terminal_handoff", RequestID: id, Name: e.CollectionID, Status: status, Text: text, Data: data})
	}
	if err := record("pending", dir, data); err != nil {
		return nil, err
	}
	select {
	case <-h.decided:
	case <-approval.Done():
	case <-b.ctx.Done():
	}
	// If approval won the race with the deadline, the terminal already exists.
	// Take the same lock as the owner decision before reading the outcome.
	cancel()
	h.mu.Lock()
	terminal, rejected := h.terminal, h.rejected
	h.mu.Unlock()
	if terminal == nil {
		status := "expired"
		if rejected {
			status = "declined"
		}
		_ = record(status, "", nil)
		return map[string]string{"status": status}, nil
	}
	_ = record("running", terminal.Status().ID, nil)
	wait, stop := context.WithCancel(ctx)
	defer stop()
	stopOnShutdown := context.AfterFunc(b.ctx, stop)
	defer stopOnShutdown()
	result, err := terminal.Wait(wait)
	if err != nil {
		return nil, err
	}
	_ = record("finished", result.ID, nil)
	return struct {
		ExitCode *int   `json:"exitCode,omitempty"`
		Signal   string `json:"signal,omitempty"`
		Error    string `json:"error,omitempty"`
	}{result.ExitCode, result.Signal, result.Error}, nil
}

// DecideTerminalHandoff is called by the owner after reviewing the proposed
// argv. The approval starts exactly that command under a PTY, with no agent
// input channel and without either the owner or the agent's Runner credential.
func (b *Bench) DecideTerminalHandoff(id string, accept bool) (*terminals.Terminal, error) {
	value, ok := b.terminalHandoffs.Load(id)
	if !ok {
		return nil, os.ErrNotExist
	}
	h := value.(*terminalHandoff)
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.terminal != nil && accept {
		return h.terminal, nil
	}
	if h.terminal != nil || h.rejected || h.ctx.Err() != nil || b.ctx.Err() != nil {
		return nil, os.ErrNotExist
	}
	if !accept {
		h.rejected = true
		close(h.decided)
		return nil, nil
	}
	b.terminalStart.Lock()
	defer b.terminalStart.Unlock()
	c, err := b.Collections.Get(h.collectionID)
	if err != nil {
		return nil, err
	}
	if c.State != "ready" {
		return nil, errors.New("workspace is not ready")
	}
	dir, err := commandDir(c.Root, h.cwd)
	if err != nil {
		return nil, err
	}
	// Use a fixed exec wrapper to resolve the executable under the
	// conversation's PATH and pass each argument literally.
	t, err := b.Terminals.Start(terminals.Spec{
		CollectionID: h.collectionID, Cwd: dir, Title: "Interactive command",
		Place: terminals.InteractiveCommandPlace, Shell: "/bin/sh", Env: h.env, Cols: 100, Rows: 30,
		Args: append([]string{"-c", `exec "$@"`, "lazybox"}, h.argv...),
	}, "")
	if err != nil {
		return nil, err
	}
	h.terminal = t
	close(h.decided)
	return t, nil
}
