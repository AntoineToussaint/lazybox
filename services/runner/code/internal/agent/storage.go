package agent

import (
	"errors"
	"strings"
	"syscall"
)

// StorageErrorEvent is the conversation's notice that the Runner could not
// save something of it (the journal, its saved state, metering, account
// state). It is the Runner's own problem, never the agent's: the agent keeps
// running, its output waits in its session host, and the write is retried.
const StorageErrorEvent = "storage_error"

// DiskFull reports whether err is the disk (or the database on it) being full:
// ENOSPC from a file, or SQLite's SQLITE_FULL ("database or disk is full").
func DiskFull(err error) bool {
	if err == nil {
		return false
	}
	if errors.Is(err, syscall.ENOSPC) {
		return true
	}
	text := strings.ToLower(err.Error())
	return strings.Contains(text, "database or disk is full") || strings.Contains(text, "no space left on device") || strings.Contains(text, "disk quota exceeded")
}

// StorageError is the notice for a write of the Runner's that failed with err:
// what happened and that the agent goes on. what names what was not saved
// ("this conversation", "its usage").
func StorageError(what string, err error) Event {
	if DiskFull(err) {
		return Event{Type: StorageErrorEvent, Status: "disk_full", Text: "Lazybox couldn't save " + what + " — the disk is full. The agent keeps running and Lazybox keeps trying; free some space."}
	}
	return Event{Type: StorageErrorEvent, Status: "write_failed", Text: "Lazybox couldn't save " + what + " (" + err.Error() + "). The agent keeps running and Lazybox keeps trying."}
}

// StorageRecovered is the notice that the Runner could save again after it
// could not record this conversation at all for a while (StorageError said so
// only in its log, as the journal itself was failing).
func StorageRecovered(err error) Event {
	if DiskFull(err) {
		return Event{Type: StorageErrorEvent, Status: "recovered", Text: "Lazybox couldn't save this conversation for a while: the disk was full. The agent kept running and nothing was lost."}
	}
	return Event{Type: StorageErrorEvent, Status: "recovered", Text: "Lazybox couldn't save this conversation for a while (" + err.Error() + "). The agent kept running and nothing was lost."}
}

// TransportFault reports whether a fault says only that the Runner lost its
// stream from the agent (the connection to its session host, the supervisor
// reading it, or the Runner's own record keeping), not that the provider or
// the agent failed. Re-attaching to the live native process settles it, and
// so does any event the agent sends after it: the agent is evidently alive. A
// native process that really exited reports its exit again on re-attachment,
// so it stays failed; one whose host is gone cannot be re-attached at all.
// Provider faults (limits, sign-in, billing, a turn's error) never qualify.
func TransportFault(fault string) bool {
	return strings.HasPrefix(fault, "provider stream ended") || strings.HasPrefix(fault, "event capture failed")
}
