// Package procid identifies a process by its PID and its start time, so a PID
// the system has since given to another program is never taken for the process
// that was recorded. A PID alone is not an identity: once its process exits,
// the number is reused.
package procid

import (
	"errors"
	"syscall"
	"time"
)

// Identity is a process as recorded: its PID and when it started. Start is an
// opaque value from Of: equal only for the same process; zero when unknown.
type Identity struct {
	PID   int   `json:"pid"`
	Start int64 `json:"start,omitempty"`
}

// State is what Check found for an identity.
type State int

const (
	// Unknown: the PID exists, but no start time was recorded to tell
	// whether it is still the recorded process.
	Unknown State = iota
	// Alive: a process with this PID and this start time runs.
	Alive
	// Gone: no process has this PID.
	Gone
	// Recycled: a process has this PID, but it started at another time: it is
	// another program, and the recorded one is gone.
	Recycled
)

func (s State) String() string {
	return [...]string{"unknown", "alive", "gone", "recycled"}[s]
}

// ErrGone is Of for a PID no process has.
var ErrGone = errors.New("no such process")

// ErrNotSame is Signal refusing a process that is not the recorded one.
var ErrNotSame = errors.New("the process with that PID is not the one recorded")

// Of reads the identity of the running process pid.
func Of(pid int) (Identity, error) {
	if pid <= 0 {
		return Identity{}, errors.New("invalid process id")
	}
	if !exists(pid) {
		return Identity{}, ErrGone
	}
	start, err := startOf(pid)
	if err != nil {
		if !exists(pid) {
			return Identity{}, ErrGone
		}
		return Identity{}, err
	}
	return Identity{PID: pid, Start: start}, nil
}

// Check says whether the recorded process still runs. Only Alive means it
// does; Gone and Recycled both prove it does not.
func (id Identity) Check() (State, error) {
	if id.PID <= 0 {
		return Unknown, errors.New("no process id recorded")
	}
	now, err := Of(id.PID)
	if errors.Is(err, ErrGone) {
		return Gone, nil
	}
	if err != nil {
		return Unknown, err
	}
	switch {
	case id.Start == 0:
		return Unknown, nil
	case now.Start == id.Start:
		return Alive, nil
	default:
		return Recycled, nil
	}
}

// Signal sends sig to the recorded process, only after checking that the
// process with that PID is still the one recorded (same start time). With
// group, it signals the process group the recorded process leads. A process
// whose start was not recorded is never signalled.
//
// The check and the signal are two system calls: a process that exits and
// whose PID is reused between them is the one window left, microseconds wide.
// A process group's ID is not handed out again while the group has members.
func (id Identity) Signal(sig syscall.Signal, group bool) error {
	state, err := id.Check()
	if err != nil {
		return err
	}
	switch state {
	case Gone:
		return ErrGone
	case Alive:
	default:
		return ErrNotSame
	}
	target := id.PID
	if group {
		target = -id.PID
	}
	if err = syscall.Kill(target, sig); errors.Is(err, syscall.ESRCH) {
		return ErrGone
	}
	return err
}

// Booted is when this machine last started. Nothing recorded before it runs.
func Booted() (time.Time, error) { return bootTime() }

// exists reports whether any process has pid. A process of another user
// answers EPERM: it exists.
func exists(pid int) bool {
	err := syscall.Kill(pid, 0)
	return err == nil || errors.Is(err, syscall.EPERM)
}
