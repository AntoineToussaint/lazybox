package engine

import (
	"errors"
	"io"
	"io/fs"
	"log"
	"os"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

// The Runner's own writes (the journal, the saved snapshot and attachment)
// can fail while the agent is fine: a full disk. That is never a reason to
// stop supervising the agent or to fail its conversation. The engine keeps
// reading the agent, says once in the conversation (and the log) that it
// cannot save, and keeps trying:
//
//   - a journal write that fails is retried with the same event, and
//     nothing more is read from the agent or acknowledged to its session host
//     until it is recorded: the host keeps the agent's output (and the agent
//     blocks once that buffer is full), so nothing is lost;
//   - a snapshot that cannot be saved is saved with the next event, or by a
//     retry if none comes: the journal already holds what happened.

// writeLocked appends value to the journal.
func (e *Engine) writeLocked(value any) error {
	if e.refuseWrites != nil {
		if err := e.refuseWrites(); err != nil {
			return err
		}
	}
	_, err := e.journal.Append(value)
	return err
}

// retryableWrite reports whether a journal append failed in a way a later one
// may not: the file system refused the write. A journal whose budget is spent
// or that closed takes nothing more.
func retryableWrite(err error) bool {
	if errors.Is(err, journal.ErrFull) || errors.Is(err, os.ErrClosed) {
		return false
	}
	var path *fs.PathError
	return errors.As(err, &path) || errors.Is(err, io.ErrShortWrite)
}

// storageFailedLocked notes that a write failed with err. The first failure
// of a run of them is logged and said in the conversation, once the journal
// can say it: at once when only the snapshot failed, when it takes writes
// again when the journal did.
func (e *Engine) storageFailedLocked(err error, journalFailed bool) {
	if journalFailed {
		e.storageJournal = true
	}
	if e.storageErr != nil {
		return
	}
	e.storageErr = err
	log.Printf("conversation %s: Lazybox couldn't save it, the agent keeps running: %v", e.state.ID, err)
	if !journalFailed {
		// Straight to the journal: saving the snapshot is what failed.
		_ = e.writeLocked(agent.StorageError("this conversation", err))
	}
}

// storageSavedLocked notes that a write succeeded, ending a run of failures.
func (e *Engine) storageSavedLocked() {
	if e.storageErr == nil {
		return
	}
	err, journalFailed := e.storageErr, e.storageJournal
	e.storageErr, e.storageJournal = nil, false
	log.Printf("conversation %s: Lazybox can save it again", e.state.ID)
	if journalFailed {
		_ = e.writeLocked(agent.StorageRecovered(err))
	}
}

// storageCause is why the last write failed.
func (e *Engine) storageCause() error {
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.storageErr == nil {
		return errors.New("write failed")
	}
	return e.storageErr
}

// recordAgain retries recording ev, received from the agent, until the
// journal takes it, the engine ends or it detaches.
func (e *Engine) recordAgain(ev agent.Event) bool {
	for delay := 250 * time.Millisecond; ; delay = min(2*delay, 10*time.Second) {
		select {
		case <-e.ctx.Done():
			return false
		case <-time.After(delay):
		}
		e.mu.Lock()
		if e.state.Closed || e.detaching {
			e.mu.Unlock()
			return false
		}
		ok, retry := e.recordLocked(ev)
		e.mu.Unlock()
		if ok {
			return true
		}
		if !retry {
			return false
		}
	}
}

// retryPersistLocked saves the snapshot again until it is saved, when no
// event comes to save it.
func (e *Engine) retryPersistLocked() {
	if e.persistRetrying {
		return
	}
	e.persistRetrying = true
	go func() {
		for delay := time.Second; ; delay = min(2*delay, 30*time.Second) {
			select {
			case <-e.ctx.Done():
				e.mu.Lock()
				e.persistRetrying = false
				e.mu.Unlock()
				return
			case <-time.After(delay):
			}
			e.mu.Lock()
			if e.detaching || e.state.Closed || e.storageErr == nil {
				e.persistRetrying = false
				e.mu.Unlock()
				return
			}
			if err := e.persistLocked(); err == nil {
				if !e.storageJournal {
					e.storageSavedLocked()
				}
				e.persistRetrying = false
				e.mu.Unlock()
				return
			}
			e.mu.Unlock()
		}
	}()
}

// settleTransportFaultLocked clears a fault that says only that the Runner
// lost its stream from the agent (agent.TransportFault), reporting whether
// there was one. The main agent the lost stream left unknown is running its
// turn, or ready; what it reports next says more.
func (e *Engine) settleTransportFaultLocked() bool {
	if !agent.TransportFault(e.state.Fault) {
		return false
	}
	e.state.Fault = ""
	if root, ok := e.state.Agents["root"]; ok && root.Status == agent.Unknown {
		root.Status = agent.Ready
		if e.state.Busy {
			root.Status = agent.Running
		}
		root.UpdatedAt = time.Now().UTC()
		e.state.Agents["root"] = root
	}
	return true
}

// Streaming reports whether the engine still reads its agent: its stream did
// not end, and it was neither closed nor detached.
func (e *Engine) Streaming() bool {
	select {
	case <-e.done:
		return false
	default:
		return true
	}
}

// SettleTransportFault clears a fault that says only that the stream from the
// agent was lost (agent.TransportFault) while this engine reads it again,
// reporting whether it did. It is what Resume does for a conversation whose
// agent is live and adopted: no second agent is started.
func (e *Engine) SettleTransportFault() bool {
	if !e.Streaming() {
		return false
	}
	e.mu.Lock()
	defer e.mu.Unlock()
	// A journal that failed is not mended by a live stream: that takes a
	// re-attachment, which opens the journal again.
	if e.state.Closed || e.detaching || strings.HasPrefix(e.state.Fault, "event capture failed") || !e.settleTransportFaultLocked() {
		return false
	}
	if err := e.persistLocked(); err != nil {
		e.storageFailedLocked(err, false)
		e.retryPersistLocked()
	}
	return true
}
