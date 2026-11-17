package runner

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/terminals"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

// HostedProcess is one live process the Runner hosts so it outlives the Runner:
// an agent, a managed command or a terminal, in its session host.
type HostedProcess struct {
	// ID is the session host's.
	ID string
	// Kind is "agent", "command" or "terminal".
	Kind string
	// Backend is the one that started the host.
	Backend string
	// State is "running", "starting" (no program yet), "exited" (output not
	// yet collected) or "unreachable" (the host did not answer a probe).
	State string
	// Attached: this Runner holds the host. An orphan is a host no Runner
	// holds: the safe sweep stops it.
	Attached bool
	// HostPID is the session host; ProgramPID the agent, command or shell.
	HostPID, ProgramPID int
	// Uptime is the host's, from the process table.
	Uptime time.Duration
	// Label names the process; the IDs below say where to open it.
	Label                                                           string
	WorkspaceID, WorkspaceName                                      string
	ExecutionID, ConversationTitle, Provider, CommandID, TerminalID string
	// AttachCommand attaches a terminal to a tmux-hosted process's window.
	AttachCommand string
	// Usage sums the host and every process under it.
	Usage *ProcessUsage
	// Ports are the TCP ports a command's processes listen on.
	Ports []uint32
}

// runProcessPrefix marks a `lazybox run` that went on in the background: it
// has no session host, so its id is this prefix and its command's id.
const runProcessPrefix = "run:"

// ProcessUsage is a process tree's share of the machine, from one ps sample.
type ProcessUsage struct {
	CPU       float64
	Memory    uint64
	Processes int
}

// ProcessInventory is every hosted process, and how new ones are hosted.
type ProcessInventory struct {
	Processes []HostedProcess
	Hosting   HostingState
}

type hostRoot struct{ kind, root string }

func (b *Bench) hostRoots() []hostRoot {
	return []hostRoot{{"agent", b.Config.HostRoot}, {"command", b.Config.CommandHostRoot}, {"terminal", b.Config.TerminalHostRoot}}
}

// Processes lists every live hosted process, whether this Runner holds it or
// not, with what it belongs to. Probing never supersedes a host's client.
func (b *Bench) Processes(ctx context.Context) (ProcessInventory, error) {
	agents, executions := map[string]*Execution{}, map[string]*Execution{}
	for _, e := range b.List() {
		executions[e.ID] = e
		if hosted, ok := e.Engine.(agent.Hosted); ok {
			if id := hosted.HostID(); id != "" {
				agents[id] = e
			}
		}
	}
	commandHosts, terminalHosts := b.Commands.Hosted(), b.Terminals.Hosted()

	type found struct {
		root hostRoot
		id   string
	}
	var all []found
	var errs []error
	for _, root := range b.hostRoots() {
		ids, err := b.hosting.Hosts(root.root)
		if err != nil {
			errs = append(errs, err)
			continue
		}
		for _, id := range ids {
			all = append(all, found{root, id})
		}
	}
	processes := make([]*HostedProcess, len(all))
	probe, cancel := context.WithTimeout(ctx, 3*time.Second)
	defer cancel()
	var wg sync.WaitGroup
	for i, f := range all {
		wg.Add(1)
		go func() {
			defer wg.Done()
			processes[i] = b.describeHost(probe, f.root, f.id)
		}()
	}
	wg.Wait()

	inventory := ProcessInventory{Hosting: b.hosting.state(ctx)}
	var pids []int
	for _, p := range processes {
		if p == nil {
			continue
		}
		switch p.Kind {
		case "agent":
			if e := agents[p.ID]; e != nil {
				b.describeAgent(ctx, p, e)
			}
		case "command":
			if status, ok := commandHosts[p.ID]; ok {
				p.CommandID, p.Label, p.ExecutionID = status.ID, commands.Label(status.Spec), status.Spec.SessionID
				p.Ports = b.CommandPorts(ctx, status)
				if e := executions[status.Spec.SessionID]; e != nil {
					p.WorkspaceID, p.Provider = e.CollectionID, e.Profile.Provider
					p.ConversationTitle = b.conversationTitle(ctx, e.ID)
				} else if status.Spec.Kind != "" {
					// A workspace action, setup or archive belongs to no conversation.
					p.WorkspaceID = status.Spec.CollectionID
				}
			}
		case "terminal":
			if status, ok := terminalHosts[p.ID]; ok {
				p.TerminalID, p.Label, p.WorkspaceID = status.ID, terminals.Label(status.Spec), status.Spec.CollectionID
			}
		}
		if p.WorkspaceID != "" && b.Collections != nil {
			if w, err := b.Collections.Get(p.WorkspaceID); err == nil {
				p.WorkspaceName = w.Name
			}
		}
		if p.HostPID > 0 {
			pids = append(pids, p.HostPID)
		}
		inventory.Processes = append(inventory.Processes, *p)
	}
	// A `lazybox run` in the background runs under no session host; its CLI
	// follows it and stops it when asked.
	for _, status := range b.Commands.List("") {
		if status.Mode != "pipe" || !status.Following || !status.Background || status.Done {
			continue
		}
		p := HostedProcess{ID: runProcessPrefix + status.ID, Kind: "command", Backend: "run", State: "running", Attached: true, HostPID: status.PID, ProgramPID: status.PID, Label: commands.Label(status.Spec), CommandID: status.ID, ExecutionID: status.Spec.SessionID, WorkspaceID: status.Spec.CollectionID, Ports: status.Ports}
		if status.StartedAt > 0 {
			p.Uptime = time.Since(time.UnixMilli(status.StartedAt)).Truncate(time.Second)
		}
		if e := executions[status.Spec.SessionID]; e != nil {
			p.WorkspaceID, p.Provider = e.CollectionID, e.Profile.Provider
			p.ConversationTitle = b.conversationTitle(ctx, e.ID)
		}
		if p.WorkspaceID != "" && b.Collections != nil {
			if w, err := b.Collections.Get(p.WorkspaceID); err == nil {
				p.WorkspaceName = w.Name
			}
		}
		if p.HostPID > 0 {
			pids = append(pids, p.HostPID)
		}
		inventory.Processes = append(inventory.Processes, p)
	}
	if usage, err := sampleProcesses(ctx, pids); err == nil {
		for i := range inventory.Processes {
			p := &inventory.Processes[i]
			if u, ok := usage[p.HostPID]; ok {
				p.Uptime, p.Usage = u.uptime, &ProcessUsage{CPU: u.cpu, Memory: u.memory, Processes: u.count}
			}
		}
	}
	order := map[string]int{"agent": 0, "command": 1, "terminal": 2}
	sort.SliceStable(inventory.Processes, func(i, j int) bool {
		a, c := inventory.Processes[i], inventory.Processes[j]
		if a.Kind != c.Kind {
			return order[a.Kind] < order[c.Kind]
		}
		return a.Uptime < c.Uptime
	})
	return inventory, errors.Join(errs...)
}

// describeHost reads a host's record and probes it; nil for a host that is gone.
func (b *Bench) describeHost(ctx context.Context, root hostRoot, id string) *HostedProcess {
	d, err := b.hosting.Describe(root.root, id)
	if err != nil {
		return nil
	}
	p := &HostedProcess{ID: id, Kind: root.kind, Backend: d.Backend, HostPID: d.PID, Label: d.Label, AttachCommand: d.AttachCommand()}
	if p.Label == "" {
		p.Label = map[string]string{"agent": "Agent", "command": "Command", "terminal": "Terminal"}[root.kind]
	}
	p.Attached = b.hosting.Held(root.root, id)
	status, err := b.hosting.Probe(ctx, root.root, id)
	switch {
	case sessionhost.Gone(err):
		return nil
	case err != nil:
		p.State = "unreachable"
	case status.Exited:
		p.State, p.ProgramPID = "exited", status.AgentPID
	case !status.Spawned:
		p.State = "starting"
	default:
		p.State, p.ProgramPID = "running", status.AgentPID
	}
	return p
}

func (b *Bench) describeAgent(ctx context.Context, p *HostedProcess, e *Execution) {
	p.ExecutionID, p.WorkspaceID, p.Provider = e.ID, e.CollectionID, e.Profile.Provider
	p.ConversationTitle = b.conversationTitle(ctx, e.ID)
	if p.ConversationTitle != "" {
		p.Label = p.ConversationTitle
	}
}

func (b *Bench) conversationTitle(ctx context.Context, id string) string {
	record, err := b.History.Get(ctx, id)
	if err != nil {
		return ""
	}
	for _, title := range []string{record.DisplayTitle, record.UserTitle, record.Title} {
		if title != "" {
			return title
		}
	}
	return ""
}

// ErrProcessNotFound means no live hosted process has that ID.
var ErrProcessNotFound = fmt.Errorf("hosted process not found: %w", os.ErrNotExist)

// StopProcess stops one hosted process the way its owner would: an agent's
// session is closed (the conversation stays and can continue), a command is
// cancelled, a terminal is closed. A host no Runner holds is stopped directly.
func (b *Bench) StopProcess(ctx context.Context, id string) error {
	if command, ok := strings.CutPrefix(id, runProcessPrefix); ok {
		c, err := b.Commands.Get(command)
		if err != nil {
			return ErrProcessNotFound
		}
		return c.Cancel()
	}
	for _, root := range b.hostRoots() {
		ids, err := b.hosting.Hosts(root.root)
		if err != nil {
			return err
		}
		for _, candidate := range ids {
			if candidate != id {
				continue
			}
			if !b.hosting.Held(root.root, id) {
				return b.hosting.Stop(ctx, root.root, id)
			}
			return b.stopHeld(ctx, root.kind, id)
		}
	}
	return ErrProcessNotFound
}

func (b *Bench) stopHeld(ctx context.Context, kind, id string) error {
	switch kind {
	case "agent":
		for _, e := range b.List() {
			if hosted, ok := e.Engine.(agent.Hosted); ok && hosted.HostID() == id {
				return e.Engine.Close()
			}
		}
	case "command":
		if status, ok := b.Commands.Hosted()[id]; ok {
			c, err := b.Commands.Get(status.ID)
			if err != nil {
				return err
			}
			return c.Cancel()
		}
	case "terminal":
		if status, ok := b.Terminals.Hosted()[id]; ok {
			return b.CloseTerminal(ctx, status.ID)
		}
	}
	return errors.New("this process is still starting; try again in a moment")
}

// CleanUpOrphanProcesses runs the safe sweep (stopOrphanHosts): it stops every
// host this Runner does not hold and forgets records of hosts that are gone. It
// reports how many live orphans it stopped.
func (b *Bench) CleanUpOrphanProcesses(ctx context.Context) (int, error) {
	return b.sweepOrphanHosts(ctx)
}

type processSample struct {
	cpu    float64
	memory uint64
	count  int
	uptime time.Duration
}

// sampleProcesses reads the process table once (ps) and sums each root's tree.
func sampleProcesses(ctx context.Context, roots []int) (map[int]processSample, error) {
	if len(roots) == 0 {
		return nil, nil
	}
	ctx, cancel := context.WithTimeout(ctx, 3*time.Second)
	defer cancel()
	out, err := exec.CommandContext(ctx, "ps", "-A", "-o", "pid=,ppid=,pcpu=,rss=,etime=").Output()
	if err != nil {
		return nil, err
	}
	type row struct {
		ppid   int
		cpu    float64
		rss    uint64
		uptime time.Duration
	}
	rows, children := map[int]row{}, map[int][]int{}
	scanner := bufio.NewScanner(bytes.NewReader(out))
	for scanner.Scan() {
		f := strings.Fields(scanner.Text())
		if len(f) < 5 {
			continue
		}
		pid, err1 := strconv.Atoi(f[0])
		ppid, err2 := strconv.Atoi(f[1])
		if err1 != nil || err2 != nil {
			continue
		}
		cpu, _ := strconv.ParseFloat(f[2], 64)
		rss, _ := strconv.ParseUint(f[3], 10, 64)
		rows[pid] = row{ppid: ppid, cpu: cpu, rss: rss * 1024, uptime: parseElapsed(f[4])}
		children[ppid] = append(children[ppid], pid)
	}
	samples := map[int]processSample{}
	for _, root := range roots {
		r, ok := rows[root]
		if !ok {
			continue
		}
		s := processSample{uptime: r.uptime}
		for queue, seen := []int{root}, map[int]bool{}; len(queue) > 0; queue = queue[1:] {
			pid := queue[0]
			if seen[pid] {
				continue
			}
			seen[pid] = true
			r := rows[pid]
			s.cpu += r.cpu
			s.memory += r.rss
			s.count++
			queue = append(queue, children[pid]...)
		}
		samples[root] = s
	}
	return samples, nil
}

// parseElapsed reads ps's etime: [[dd-]hh:]mm:ss.
func parseElapsed(s string) time.Duration {
	days := 0
	if d, rest, ok := strings.Cut(s, "-"); ok {
		days, _ = strconv.Atoi(d)
		s = rest
	}
	parts := strings.Split(s, ":")
	total := 0
	for _, p := range parts {
		n, err := strconv.Atoi(p)
		if err != nil {
			return 0
		}
		total = total*60 + n
	}
	return time.Duration(days*86400+total) * time.Second
}
