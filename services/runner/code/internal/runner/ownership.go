package runner

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"slices"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/procid"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/engine"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

// Unconfirmed ownership (RESOURCE_OWNERSHIP.md, "Unconfirmed ownership"): a
// previous Runner recorded an agent or command that this Runner did not take
// over, and nothing it holds says those processes stopped. Until that is
// settled the workspace takes no new conversation and is not deleted, and the
// conversation is not deleted or resumed: a process nobody supervises could
// still be writing in the workspace.
//
// This file says what was recorded (each process, its PID and command line)
// and whether a process with that identity still runs, checked here: a host
// that answers with its secret, or a PID with the start time recorded for it.
// It settles ownership three ways: by itself when it proves every process is
// gone (the usual case after the computer restarts), by stopping exactly the
// recorded processes, or on the person's word that they stopped.

// The state of one recorded process, as the Runner checked it.
const (
	// OwnedRunning: a process with the recorded identity runs.
	OwnedRunning = "running"
	// OwnedGone: proved gone: its host is gone, its PID is free or now
	// another program's, or it started before the computer last started.
	OwnedGone = "gone"
	// OwnedUnknown: nothing recorded lets the Runner tell.
	OwnedUnknown = "unknown"
)

// OwnedProcess is one process a previous Runner recorded for a session whose
// ownership is unconfirmed.
type OwnedProcess struct {
	// Kind is "agent" or "command".
	Kind string
	// ExecutionID is the session; CommandID the command, for a command.
	ExecutionID, CommandID string
	// Label names it for people: the conversation's title, the command's.
	Label string
	// CommandLine is what it runs, where recorded or observed.
	CommandLine string
	// PID is the program's (or its host's) process ID, 0 when none is recorded.
	PID int
	// State is OwnedRunning, OwnedGone or OwnedUnknown; Detail says why.
	State, Detail string

	// The session host it runs in, if any, and whether this Runner holds it.
	root, host string
	held       bool
	answers    bool
	// identity is the host process's, for one that does not answer.
	identity procid.Identity
}

// Ownership is what is unconfirmed for a workspace or a conversation.
type Ownership struct {
	// Owners are the sessions still unconfirmed; Processes what each recorded.
	Owners    []string
	Processes []OwnedProcess
	// Settled: nothing is unconfirmed (any more).
	Settled bool
	// Note says what the Runner settled by itself, if anything.
	Note string
}

// Resolutions recorded on a settled session (collections LastOwnershipResolution,
// a command's outcome).
const (
	resolvedChecked   = "Lazybox checked that none of its processes are running"
	resolvedStopped   = "Lazybox stopped its processes"
	resolvedConfirmed = "You confirmed its processes had stopped"
)

// WorkspaceOwnership says what is unconfirmed in a workspace, settling by
// itself every session whose processes it proves gone.
func (b *Bench) WorkspaceOwnership(ctx context.Context, id string) (Ownership, error) {
	owners, err := b.workspaceOwners(id)
	if err != nil {
		return Ownership{}, err
	}
	return b.ownership(ctx, owners)
}

// ConversationOwnership is WorkspaceOwnership for one conversation: its
// attempts whose processes are unconfirmed.
func (b *Bench) ConversationOwnership(ctx context.Context, id string) (Ownership, error) {
	owners, err := b.conversationOwners(id)
	if err != nil {
		return Ownership{}, err
	}
	return b.ownership(ctx, owners)
}

// ResolveWorkspaceOwnership settles a workspace's unconfirmed sessions: with
// stop, by stopping exactly the recorded processes that still run (checked by
// identity first); without, on the person's word that they stopped, which is
// refused for a process the Runner can see running.
func (b *Bench) ResolveWorkspaceOwnership(ctx context.Context, id string, stop bool, reason string) (Ownership, error) {
	owners, err := b.workspaceOwners(id)
	if err != nil {
		return Ownership{}, err
	}
	return b.resolveOwnership(ctx, owners, stop, reason)
}

// ResolveConversationOwnership is ResolveWorkspaceOwnership for a conversation.
func (b *Bench) ResolveConversationOwnership(ctx context.Context, id string, stop bool, reason string) (Ownership, error) {
	owners, err := b.conversationOwners(id)
	if err != nil {
		return Ownership{}, err
	}
	return b.resolveOwnership(ctx, owners, stop, reason)
}

// SettleProvenOwnership runs once at startup, after re-attaching: every
// unconfirmed session whose recorded processes are all proved gone is settled,
// so the person is never asked about processes that cannot be running. After
// the computer restarts that is all of them.
func (b *Bench) SettleProvenOwnership(ctx context.Context) error {
	var errs []error
	candidates := map[string]bool{}
	if b.Collections != nil {
		for _, c := range b.Collections.List() {
			if !c.OwnershipUncertain {
				continue
			}
			owners, err := b.Collections.UncertainOwners(c.ID)
			if err != nil {
				errs = append(errs, err)
				continue
			}
			for _, owner := range owners {
				candidates[owner] = true
			}
		}
	}
	for offset := 0; ; offset += 100 {
		records, err := b.History.List(ctx, 100, offset)
		if err != nil {
			errs = append(errs, err)
			break
		}
		for _, r := range records {
			if !r.Snapshot.Closed || r.Snapshot.Fault == "native_exit_unconfirmed" {
				candidates[r.ID] = true
			}
		}
		if len(records) < 100 {
			break
		}
	}
	for _, c := range b.Commands.List("") {
		if c.Spec.SessionID != "" && c.Done && c.OwnershipUnconfirmed() {
			candidates[c.Spec.SessionID] = true
		}
	}
	owners := make([]string, 0, len(candidates))
	for owner := range candidates {
		b.mu.Lock()
		live := b.items[owner] != nil
		b.mu.Unlock()
		if !live {
			owners = append(owners, owner)
		}
	}
	slices.Sort(owners)
	if _, err := b.ownership(ctx, owners); err != nil {
		errs = append(errs, err)
	}
	return errors.Join(errs...)
}

func (b *Bench) workspaceOwners(id string) ([]string, error) {
	if b.Collections == nil {
		return nil, errors.New("workspaces unavailable")
	}
	return b.Collections.UncertainOwners(id)
}

// conversationOwners are the attempts of a conversation that need recovery,
// latest first, as deleting it walks them.
func (b *Bench) conversationOwners(id string) ([]string, error) {
	var owners []string
	seen := map[string]bool{}
	for id != "" && !seen[id] {
		seen[id] = true
		e, err := b.Get(id)
		if err != nil {
			return nil, err
		}
		if e.RecoveryRequired {
			owners = append(owners, e.ID)
		}
		id = e.Previous
	}
	return owners, nil
}

// ownership lists what each owner recorded and settles the owners proved gone.
func (b *Bench) ownership(ctx context.Context, owners []string) (Ownership, error) {
	boot, _ := procid.Booted()
	result := Ownership{}
	settled := 0
	var errs []error
	for _, owner := range owners {
		processes := b.ownedProcesses(ctx, owner, boot)
		if allGone(processes) {
			if err := b.settleOwner(ctx, owner, resolvedChecked); err != nil {
				errs = append(errs, err)
			} else {
				settled++
				continue
			}
		}
		result.Owners = append(result.Owners, owner)
		result.Processes = append(result.Processes, processes...)
	}
	result.Settled = len(result.Owners) == 0
	if settled > 0 {
		result.Note = "Lazybox checked that none of the recorded processes are running and cleared the mark."
	}
	return result, errors.Join(errs...)
}

func (b *Bench) resolveOwnership(ctx context.Context, owners []string, stop bool, reason string) (Ownership, error) {
	reason = strings.TrimSpace(reason)
	if len(reason) > 512 {
		return Ownership{}, errors.New("keep the reason under 512 characters")
	}
	boot, _ := procid.Booted()
	if !stop {
		// The person's word, as Resume's processesStopped: never against what
		// the Runner can see running.
		for _, owner := range owners {
			for _, p := range b.ownedProcesses(ctx, owner, boot) {
				if p.State == OwnedRunning {
					return Ownership{}, fmt.Errorf("%s is still running%s: stop it first, or choose Stop them and continue", describeOwned(p), pidSuffix(p.PID))
				}
			}
		}
		resolution := resolvedConfirmed
		if reason != "" {
			resolution += ": " + reason
		}
		var errs []error
		for _, owner := range owners {
			errs = append(errs, b.settleOwner(ctx, owner, resolution))
		}
		if err := errors.Join(errs...); err != nil {
			return Ownership{}, err
		}
		return Ownership{Settled: true}, nil
	}
	var errs []error
	for _, owner := range owners {
		for _, p := range b.ownedProcesses(ctx, owner, boot) {
			if p.State == OwnedRunning {
				if err := b.stopOwned(ctx, p); err != nil {
					errs = append(errs, fmt.Errorf("%s: %w", describeOwned(p), err))
				}
			}
		}
	}
	// A cancelled command and a closed agent take a moment to be confirmed.
	deadline := time.Now().Add(15 * time.Second)
	result := Ownership{}
	for {
		result = Ownership{}
		running := false
		for _, owner := range owners {
			processes := b.ownedProcesses(ctx, owner, boot)
			if allGone(processes) {
				continue
			}
			result.Owners = append(result.Owners, owner)
			result.Processes = append(result.Processes, processes...)
			for _, p := range processes {
				running = running || p.State == OwnedRunning
			}
		}
		if !running || time.Now().After(deadline) || ctx.Err() != nil {
			break
		}
		time.Sleep(250 * time.Millisecond)
	}
	for _, owner := range owners {
		if !slices.Contains(result.Owners, owner) {
			errs = append(errs, b.settleOwner(ctx, owner, resolvedStopped))
		}
	}
	result.Settled = len(result.Owners) == 0
	if !result.Settled && len(errs) == 0 {
		for _, p := range result.Processes {
			switch p.State {
			case OwnedRunning:
				errs = append(errs, fmt.Errorf("%s is still running%s", describeOwned(p), pidSuffix(p.PID)))
			case OwnedUnknown:
				errs = append(errs, fmt.Errorf("Lazybox cannot check %s: %s If you know it stopped, choose They're already stopped", describeOwned(p), p.Detail))
			}
		}
	}
	return result, errors.Join(errs...)
}

func allGone(processes []OwnedProcess) bool {
	for _, p := range processes {
		if p.State != OwnedGone {
			return false
		}
	}
	return true
}

func describeOwned(p OwnedProcess) string {
	if p.Kind == "command" {
		return "the command " + strconv.Quote(p.Label)
	}
	return "the agent of " + strconv.Quote(p.Label)
}

func pidSuffix(pid int) string {
	if pid <= 0 {
		return ""
	}
	return " (PID " + strconv.Itoa(pid) + ")"
}

// ownedProcesses lists what a previous Runner recorded for execution id: its
// agent, and every command of it whose stop is unconfirmed, each checked.
func (b *Bench) ownedProcesses(ctx context.Context, id string, boot time.Time) []OwnedProcess {
	title := b.conversationTitle(ctx, id)
	if title == "" {
		title = "Untitled conversation"
	}
	var out []OwnedProcess
	root := b.Config.HostRoot
	hosts, err := b.hosting.Hosts(root)
	found := false
	if err != nil {
		found = true
		out = append(out, OwnedProcess{Kind: "agent", ExecutionID: id, Label: title, State: OwnedUnknown, Detail: "Lazybox could not read its process records."})
	}
	for _, host := range hosts {
		d, err := b.hosting.Describe(root, host)
		if err != nil || d.Owner != id {
			continue
		}
		found = true
		p := b.hostEvidence(ctx, root, host, d)
		p.Kind, p.ExecutionID, p.Label = "agent", id, title
		out = append(out, p)
	}
	if !found {
		// Every agent runs in a session host, which removes its record only
		// once it has exited; a host's exit takes its agent with it.
		out = append(out, OwnedProcess{Kind: "agent", ExecutionID: id, Label: title, State: OwnedGone, Detail: "No session host is left for its agent."})
	}
	for _, c := range b.Commands.List(id) {
		if c.Done && !c.OwnershipUnconfirmed() {
			continue
		}
		p := OwnedProcess{Kind: "command", ExecutionID: id, CommandID: c.ID, Label: commands.Label(c.Spec), CommandLine: strings.Join(c.Spec.Argv, " "), PID: c.PID}
		root, host := b.Commands.HostOf(c.ID)
		switch {
		case host != "":
			d, err := b.hosting.Describe(root, host)
			if err != nil {
				if errors.Is(err, os.ErrNotExist) {
					p.State, p.Detail = OwnedGone, "Its session host is gone, and the command with it."
				} else {
					p.State, p.Detail = OwnedUnknown, "Lazybox could not read its process record."
				}
				break
			}
			evidence := b.hostEvidence(ctx, root, host, d)
			evidence.Kind, evidence.ExecutionID, evidence.CommandID, evidence.Label = p.Kind, p.ExecutionID, p.CommandID, p.Label
			if p.CommandLine != "" {
				evidence.CommandLine = p.CommandLine
			}
			p = evidence
		case !c.Done:
			// A pipe: its program is the person's, which Lazybox does not run.
			p.State, p.Detail = OwnedUnknown, "Lazybox only receives this command's output; it does not run it."
			if p.PID > 0 {
				if state, _ := (procid.Identity{PID: p.PID}).Check(); state == procid.Gone {
					p.State, p.Detail = OwnedGone, "No process has its PID any more."
				}
			}
		case c.StartedAt > 0 && !boot.IsZero() && time.UnixMilli(c.StartedAt).Before(boot):
			p.State, p.Detail = OwnedGone, "It started before the computer last started."
		case p.PID > 0:
			if state, _ := (procid.Identity{PID: p.PID}).Check(); state == procid.Gone {
				p.State, p.Detail = OwnedGone, "No process has its PID any more."
			} else {
				p.State, p.Detail = OwnedUnknown, "A process has its PID, but Lazybox did not record when it started, so it cannot tell whether it is the same one."
			}
		default:
			p.State, p.Detail = OwnedUnknown, "Lazybox recorded no process for it, so it cannot check."
		}
		out = append(out, p)
	}
	return out
}

// hostEvidence checks one session host: a host that answers with its secret
// is the recorded one; one that does not is checked by its PID and start time.
func (b *Bench) hostEvidence(ctx context.Context, root, host string, d sessionhost.Description) OwnedProcess {
	p := OwnedProcess{PID: d.PID, root: root, host: host, identity: procid.Identity{PID: d.PID, Start: d.Started}}
	probe, cancel := context.WithTimeout(ctx, 3*time.Second)
	status, err := b.hosting.Probe(probe, root, host)
	cancel()
	switch {
	case err == nil:
		p.answers = true
		p.held = b.hosting.Held(root, host)
		if status.AgentPID > 0 && !status.Exited {
			p.PID = status.AgentPID
		}
		p.State = OwnedRunning
		switch {
		case status.Exited:
			p.Detail = "Its program has exited; its session host still holds its output."
		case p.held:
			p.Detail = "This Runner supervises it."
		default:
			p.Detail = "It runs, and no Runner supervises it."
		}
	case sessionhost.Gone(err):
		p.State, p.Detail = OwnedGone, "Its session host is gone, and its program with it."
	default:
		state, _ := p.identity.Check()
		switch state {
		case procid.Alive:
			p.State, p.Detail = OwnedRunning, "Its session host runs but does not answer."
		case procid.Gone:
			p.State, p.Detail = OwnedGone, "Its session host is gone, and its program with it."
		case procid.Recycled:
			p.State, p.Detail = OwnedGone, "Its PID now belongs to another program: the recorded process is gone."
		default:
			p.State, p.Detail = OwnedUnknown, "Its session host does not answer, and Lazybox cannot tell whether the process with its PID is still the same one."
		}
	}
	if p.State == OwnedRunning && p.CommandLine == "" {
		p.CommandLine = commandLine(ctx, p.PID)
	}
	return p
}

// commandLine is what process pid runs, as the process table shows it.
func commandLine(ctx context.Context, pid int) string {
	if pid <= 0 {
		return ""
	}
	ctx, cancel := context.WithTimeout(ctx, 2*time.Second)
	defer cancel()
	out, err := exec.CommandContext(ctx, "ps", "-o", "command=", "-p", strconv.Itoa(pid)).Output()
	if err != nil {
		return ""
	}
	line := strings.TrimSpace(string(out))
	if len(line) > 512 {
		line = line[:512] + "…"
	}
	return line
}

// stopOwned stops one recorded process that runs, the way its owner would
// where it can: a host that answers is stopped through its own control
// connection (its secret is its identity), one this Runner holds through its
// session or command. A host that does not answer is killed only if its PID
// still has the start time recorded for it: never a PID given to another
// program since. Its program dies with it (processguard).
func (b *Bench) stopOwned(ctx context.Context, p OwnedProcess) error {
	switch {
	case p.host == "":
		return errors.New("Lazybox does not run it, so it cannot stop it")
	case p.held:
		return b.stopHeld(ctx, p.Kind, p.host)
	case p.answers:
		return b.hosting.Stop(ctx, p.root, p.host)
	}
	if p.identity.Start == 0 {
		return errors.New("its session host does not answer and its start time was not recorded, so Lazybox cannot be sure the process with its PID is the same one")
	}
	err := p.identity.Signal(syscall.SIGKILL, false)
	switch {
	case errors.Is(err, procid.ErrGone), errors.Is(err, procid.ErrNotSame):
		// Gone already: the PID is free, or another program's now.
		return nil
	case err != nil:
		return err
	}
	return nil
}

// settleOwner records that execution id's processes are known stopped: its
// unconfirmed commands are settled, a conversation left open by a previous
// Runner is ended as interrupted (resumable, with a notice saying why), and
// every workspace that recorded it as an owner is resolved.
func (b *Bench) settleOwner(ctx context.Context, id, resolution string) error {
	var errs []error
	for _, c := range b.Commands.List(id) {
		if c.Done && c.OwnershipUnconfirmed() {
			errs = append(errs, b.Commands.Settle(c.ID, resolution+"."))
		}
	}
	b.mu.Lock()
	live := b.items[id] != nil
	b.mu.Unlock()
	if !live {
		record, err := b.History.Get(ctx, id)
		switch {
		case errors.Is(err, os.ErrNotExist):
		case err != nil:
			errs = append(errs, err)
		case !record.Snapshot.Closed || record.Snapshot.Fault == "native_exit_unconfirmed":
			// Its queued messages, if a re-attachment record kept them, are
			// carried to the resumed agent as after a reboot.
			attachment, err := b.loadAttachment(id)
			if err != nil {
				attachment = engine.Attachment{}
				if queue, carried := b.loadCarried(id); carried == nil {
					attachment.Queue = queue
				}
			}
			errs = append(errs, b.endInterrupted(ctx, record, attachment, ownershipNotice(resolution)))
		}
		b.mu.Lock()
		delete(b.archives, id)
		b.mu.Unlock()
	}
	if b.Collections != nil {
		for _, c := range b.Collections.List() {
			if !c.OwnershipUncertain {
				continue
			}
			owners, err := b.Collections.UncertainOwners(c.ID)
			if err != nil || !slices.Contains(owners, id) {
				continue
			}
			if _, err = b.Collections.ResolveOwner(c.ID, id, resolution); err != nil {
				errs = append(errs, err)
			}
		}
	}
	return errors.Join(errs...)
}

// ownershipNotice leads the agent_lost notice of a conversation a previous
// Runner left open, once its processes are known stopped.
func ownershipNotice(resolution string) string {
	return "A previous Lazybox left this agent's processes unaccounted for. " + resolution + ", so this conversation ended."
}
