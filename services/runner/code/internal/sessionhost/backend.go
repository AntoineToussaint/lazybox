package sessionhost

import (
	"context"
	"errors"
	"path/filepath"
	"strings"
)

// Backend names.
const (
	// BackendSessionHost starts each host as a detached process in its own
	// session: the default.
	BackendSessionHost = "sessionhost"
	// BackendTmux starts each host in a window of a private tmux server, where
	// a person can watch it (`tmux -S … attach`). Optional.
	BackendTmux = "tmux"
)

// Backend hosts long-lived processes that outlive the Runner: it starts a host
// (Launch), re-attaches to one an earlier Runner left running (Attach), stops
// and inspects one (Stop, Probe), lists them (Hosts), forgets one that is gone
// (Forget) and says whether this process holds one (Held). The Client a Backend
// returns streams the program's I/O, resizes a terminal and signals the
// program (Terminate) whichever backend started it.
//
// Every host, however it was started, writes the same record and speaks the
// same authenticated wire. Only Launch differs between Local and Tmux; the
// other operations are the record operations both share. A Runner configured
// for one backend therefore re-attaches, stops and sweeps hosts the other one
// started, and switching backends never strands a running process.
type Backend interface {
	// Name is BackendSessionHost or BackendTmux.
	Name() string
	// Launch starts a new host under root and attaches to it. The host has no
	// program until Client.Spawn.
	Launch(ctx context.Context, root string, opts Options) (*Client, error)
	// Attach re-connects to a running host; see the package function.
	Attach(ctx context.Context, root, id string, ack uint64) (*Client, error)
	// Stop terminates a host's program and waits for it to exit.
	Stop(ctx context.Context, root, id string) error
	// Probe inspects a host without superseding its client.
	Probe(ctx context.Context, root, id string) (Status, error)
	// Hosts lists the hosts under root.
	Hosts(root string) ([]string, error)
	// Forget removes the record of a host that is gone.
	Forget(root, id string) error
	// Held reports whether a client in this process holds the host.
	Held(root, id string) bool
	// Describe reads what a person needs to find the host from outside.
	Describe(root, id string) (Description, error)
}

// Use returns b, or the default Local backend when b is nil.
func Use(b Backend) Backend {
	if b == nil {
		return Local{}
	}
	return b
}

// records are the operations every backend shares.
type records struct{}

func (records) Attach(ctx context.Context, root, id string, ack uint64) (*Client, error) {
	return Attach(ctx, root, id, ack)
}
func (records) Stop(ctx context.Context, root, id string) error { return Stop(ctx, root, id) }
func (records) Probe(ctx context.Context, root, id string) (Status, error) {
	return Probe(ctx, root, id)
}
func (records) Hosts(root string) ([]string, error)           { return Hosts(root) }
func (records) Forget(root, id string) error                  { return Forget(root, id) }
func (records) Held(root, id string) bool                     { return Held(root, id) }
func (records) Describe(root, id string) (Description, error) { return Describe(root, id) }

// Local is the default backend: each host is the Runner binary re-executed as
// a detached process in its own session (see the package documentation).
type Local struct{ records }

func (Local) Name() string { return BackendSessionHost }

// Launch starts a detached host; it is the package function Launch.
func (Local) Launch(ctx context.Context, root string, opts Options) (*Client, error) {
	return Launch(ctx, root, opts)
}

// Description is a host as its record tells it, for an inventory.
type Description struct {
	ID, Backend string
	// PID is the host process; Started its start time (Record.Started).
	PID                int
	Started            int64
	Kind, Owner, Label string
	// Tmux is set for a host started in tmux.
	Tmux *TmuxWindow
}

// AttachCommand is the shell command that attaches a terminal to a tmux-hosted
// host's window, or "" for a host with no window.
func (d Description) AttachCommand() string {
	if d.Tmux == nil || d.Tmux.Session == "" {
		return ""
	}
	if d.Tmux.Socket != "" {
		return "tmux -S " + shellQuote(d.Tmux.Socket) + " attach -t " + shellQuote(d.Tmux.Session)
	}
	return "tmux -L " + shellQuote(d.Tmux.Server) + " attach -t " + shellQuote(d.Tmux.Session)
}

// Describe reads a host's record. It does not contact the host.
func Describe(root, id string) (Description, error) {
	if !validID.MatchString(id) {
		return Description{}, errors.New("invalid session host id")
	}
	r, err := ReadRecord(filepath.Join(root, id))
	if err != nil {
		return Description{}, err
	}
	return Description{ID: r.ID, Backend: r.Backend, PID: r.PID, Started: r.Started, Kind: r.Kind, Owner: r.Owner, Label: r.Label, Tmux: r.Tmux}, nil
}

// shellQuote quotes s for a POSIX shell when it needs it.
func shellQuote(s string) string {
	if s != "" && strings.Trim(s, "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_./:@%+=,") == "" {
		return s
	}
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}
