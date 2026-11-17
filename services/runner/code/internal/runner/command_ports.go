package runner

import (
	"context"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/listenports"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
)

// portsFresh is how long a managed command's ports are reused: clients poll
// the command list every 1.5 s, and one look costs a process-table read and
// an lsof.
const portsFresh = 3 * time.Second

type portCache struct {
	mu      sync.Mutex
	entries map[string]portEntry
}

type portEntry struct {
	at    time.Time
	ports []uint32
}

// CommandPorts returns the TCP ports a running command's processes listen
// on: what its `lazybox run` last reported for a pipe, or a look at a managed
// command's process tree, reused for a few seconds. Nothing once it is done.
func (b *Bench) CommandPorts(ctx context.Context, state commands.Status) []uint32 {
	if state.Done {
		b.ports.forget(state.ID)
		return nil
	}
	if state.Mode != "managed" {
		return state.Ports
	}
	b.ports.mu.Lock()
	if e, ok := b.ports.entries[state.ID]; ok && time.Since(e.at) < portsFresh {
		b.ports.mu.Unlock()
		return e.ports
	}
	b.ports.mu.Unlock()
	c, err := b.Commands.Get(state.ID)
	if err != nil {
		return nil
	}
	pid := c.PID()
	if pid <= 0 {
		return nil
	}
	ports, err := listenports.Of(ctx, pid)
	if err != nil {
		return nil
	}
	b.ports.mu.Lock()
	defer b.ports.mu.Unlock()
	if b.ports.entries == nil {
		b.ports.entries = map[string]portEntry{}
	}
	b.ports.entries[state.ID] = portEntry{at: time.Now(), ports: ports}
	return ports
}

func (p *portCache) forget(id string) {
	p.mu.Lock()
	delete(p.entries, id)
	p.mu.Unlock()
}
