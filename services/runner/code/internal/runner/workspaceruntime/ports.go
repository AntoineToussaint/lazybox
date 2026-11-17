package workspaceruntime

import (
	"fmt"
	"net"
	"strconv"
	"sync"
	"time"
)

// Default port range. Below every common ephemeral range (Linux 32768+,
// macOS 49152+), so a block is not taken by an outgoing connection, and above
// the ports dev servers pick by default (3000, 5173, 8080…).
const (
	DefaultPortStart = 20000
	DefaultPortEnd   = 29999
)

// ValidRange checks a port range: unprivileged, and room for one block.
func ValidRange(start, end int) error {
	if start < 1024 || end > 65535 || end-start+1 < PortCount {
		return fmt.Errorf("the port range must be within 1024–65535 and hold at least %d ports", PortCount)
	}
	return nil
}

// Ports allocates every workspace a stable block of PortCount ports. A block
// is the workspace's until it is released (the workspace is deleted), across
// Runner restarts; blocks never overlap, and a new block skips ports another
// program is using when it is allocated.
type Ports struct {
	mu     sync.Mutex
	path   string
	blocks map[string]int
	// InUse reports a port some program listens on; a test seam.
	InUse func(port int) bool
}

// OpenPorts loads the allocations kept at path.
func OpenPorts(path string) (*Ports, error) {
	p := &Ports{path: path, blocks: map[string]int{}, InUse: PortInUse}
	if err := readJSON(path, &p.blocks); err != nil {
		return nil, err
	}
	return p, nil
}

// Lookup returns a workspace's block, if it has one.
func (p *Ports) Lookup(workspace string) (int, bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	base, ok := p.blocks[workspace]
	return base, ok
}

// Block returns the workspace's block, allocating the first free one in
// [start, end] if it has none. An existing block is kept even when the range
// changed since: ports a running server and its bookmarks use do not move.
func (p *Ports) Block(workspace string, start, end int) (int, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if base, ok := p.blocks[workspace]; ok {
		return base, nil
	}
	if err := ValidRange(start, end); err != nil {
		return 0, err
	}
	taken := func(base int) bool {
		for other, b := range p.blocks {
			if other != workspace && base < b+PortCount && b < base+PortCount {
				return true
			}
		}
		return false
	}
	busy := 0
	for base := start; base+PortCount-1 <= end; base += PortCount {
		if taken(base) {
			continue
		}
		free := true
		for port := base; port < base+PortCount; port++ {
			if p.InUse(port) {
				free = false
				break
			}
		}
		if !free {
			busy++
			continue
		}
		next := map[string]int{}
		for k, v := range p.blocks {
			next[k] = v
		}
		next[workspace] = base
		if err := writeJSON(p.path, next); err != nil {
			return 0, err
		}
		p.blocks = next
		return base, nil
	}
	if busy > 0 {
		return 0, fmt.Errorf("no free block of %d ports in %d–%d: %d blocks are used by other programs; widen the range in Settings → Workspaces", PortCount, start, end, busy)
	}
	return 0, fmt.Errorf("every block of %d ports in %d–%d belongs to a workspace; widen the range in Settings → Workspaces", PortCount, start, end)
}

// Release frees a workspace's block.
func (p *Ports) Release(workspace string) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if _, ok := p.blocks[workspace]; !ok {
		return nil
	}
	next := map[string]int{}
	for k, v := range p.blocks {
		if k != workspace {
			next[k] = v
		}
	}
	if err := writeJSON(p.path, next); err != nil {
		return err
	}
	p.blocks = next
	return nil
}

// Variables are the block's environment: LAZYBOX_PORT_0 … _9, and
// LAZYBOX_PORT for the port an action picked (0 unless it chose another).
func Variables(base, pick int) map[string]string {
	vars := map[string]string{}
	if base <= 0 {
		return vars
	}
	for i := range PortCount {
		vars["LAZYBOX_PORT_"+strconv.Itoa(i)] = strconv.Itoa(base + i)
	}
	vars["LAZYBOX_PORT"] = strconv.Itoa(base + pick)
	return vars
}

// PortInUse reports whether a program listens on the port on this machine:
// something accepts a connection on IPv4 or IPv6 loopback, or the port cannot
// be bound on IPv4 loopback.
func PortInUse(port int) bool {
	for _, host := range []string{"127.0.0.1", "::1"} {
		c, err := net.DialTimeout("tcp", net.JoinHostPort(host, strconv.Itoa(port)), 150*time.Millisecond)
		if err == nil {
			c.Close()
			return true
		}
	}
	l, err := net.Listen("tcp", net.JoinHostPort("127.0.0.1", strconv.Itoa(port)))
	if err != nil {
		return true
	}
	l.Close()
	return false
}
