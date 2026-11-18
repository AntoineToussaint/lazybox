package commands

import (
	"errors"
	"os"
	"path/filepath"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/processguard"
)

// ErrOwnershipUnconfirmed is the outcome of a command a previous Runner left
// running without a session host: nothing this Runner holds says it stopped.
var ErrOwnershipUnconfirmed = errors.New("command ownership unconfirmed after Runner restart")

// OwnershipUnconfirmed reports a command whose processes nobody has confirmed
// stopped: a previous Runner's command it could not take over, or one whose
// stop went unconfirmed (CleanupUnconfirmed).
func (state Status) OwnershipUnconfirmed() bool {
	return state.Error == ErrOwnershipUnconfirmed.Error() || state.Error == processguard.ErrCleanupUnconfirmed.Error()
}

// HostOf is the session host a command runs or ran in, "" for one without.
func (m *Manager) HostOf(id string) (root, host string) {
	c, err := m.Get(id)
	if err != nil {
		return "", ""
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.hostID != "" {
		return m.hostRoot, c.hostID
	}
	if c.recordedHost != "" {
		return m.hostRoot, c.recordedHost
	}
	return "", ""
}

// Settle records that a finished command whose stop was unconfirmed is known
// to be stopped: the Runner checked, or the person said so. note says which,
// and replaces the command's error. It is kept in <id>.settled beside the
// command's journal, so the next Runner reads the command as settled too.
func (m *Manager) Settle(id, note string) error {
	note = strings.TrimSpace(note)
	if note == "" || len(note) > 1024 {
		return errors.New("say how the command is known to be stopped")
	}
	c, err := m.Get(id)
	if err != nil {
		return err
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if !c.status.Done {
		return errors.New("the command is still running")
	}
	if !c.status.OwnershipUnconfirmed() {
		return nil
	}
	f, err := os.CreateTemp(m.dir, ".settled-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.WriteString(note); err == nil {
		err = f.Sync()
	}
	if err = errors.Join(err, f.Close()); err != nil {
		return err
	}
	if err = os.Rename(f.Name(), filepath.Join(m.dir, id+".settled")); err != nil {
		return err
	}
	c.status.Error = note
	return nil
}

func (m *Manager) settledNote(id string) (string, bool) {
	data, err := os.ReadFile(filepath.Join(m.dir, id+".settled"))
	if err != nil || len(data) == 0 || len(data) > 1024 {
		return "", false
	}
	return string(data), true
}
