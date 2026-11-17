package power

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"slices"
	"sync"
	"time"
)

// UI request kinds: what the CLI asks of an open window.
const (
	UIOpen   = "open"
	UIShot   = "shot"
	UIAction = "action"
	// UIDo runs an action of the app's registry (`lazybox do`).
	UIDo = "do"
)

// UI request states.
const (
	UIPending = "pending"
	UIDone    = "done"
	UIFailed  = "failed"
)

// UIRequest is one thing the CLI asked a window to do.
type UIRequest struct {
	ID, Kind                      string
	CollectionID, SessionID, File string
	Line                          int
	Panel, ActionID               string
	// Argument is a do request's argument.
	Argument string
	// Poster is the hook run that posted it ("" for the owner): only it may wait on it.
	Poster               string
	State, Error, Detail string
	PNG                  []byte
	Created              time.Time
	watcher              int
	done                 chan struct{}
}

// ErrNoWindow refuses a request no window can take.
var ErrNoWindow = errors.New("no Lazybox window is open to do this; open the app and try again")

// MaxShotBytes bounds a screenshot a window may return.
const MaxShotBytes = 32 << 20

// requestTTL is how long a request's outcome is kept.
const requestTTL = 5 * time.Minute

// Broker routes the CLI's requests to one open window and returns its answer.
// A desktop window is preferred over a browser tab, and the most recently
// connected window of a kind over older ones: that is the window in front.
type Broker struct {
	mu       sync.Mutex
	next     int
	watchers map[int]*uiWatcher
	requests map[string]*UIRequest
}

type uiWatcher struct {
	surface string
	ch      chan UIRequest
}

func NewBroker() *Broker {
	return &Broker{watchers: map[int]*uiWatcher{}, requests: map[string]*UIRequest{}}
}

// Watch registers a window. Requests addressed to it arrive on the channel;
// stop unregisters it and fails what it had not answered.
func (b *Broker) Watch(surface string) (<-chan UIRequest, func()) {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.next++
	id := b.next
	w := &uiWatcher{surface: surface, ch: make(chan UIRequest, 8)}
	b.watchers[id] = w
	return w.ch, func() {
		b.mu.Lock()
		defer b.mu.Unlock()
		delete(b.watchers, id)
		for _, r := range b.requests {
			if r.watcher == id && r.State == UIPending {
				b.finishLocked(r, "the window closed before it answered", "", nil)
			}
		}
	}
}

// Windows counts the open windows.
func (b *Broker) Windows() int {
	b.mu.Lock()
	defer b.mu.Unlock()
	return len(b.watchers)
}

// Post sends a request to the window in front.
func (b *Broker) Post(r UIRequest) (UIRequest, error) {
	if !slices.Contains([]string{UIOpen, UIShot, UIAction, UIDo}, r.Kind) {
		return UIRequest{}, errors.New("unknown request: open, shot, action or do")
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	b.expireLocked()
	// Desktop first, then the newest.
	rank := func(id int) int {
		if b.watchers[id].surface == "desktop" {
			return 1<<30 + id
		}
		return id
	}
	target := 0
	for id := range b.watchers {
		if target == 0 || rank(id) > rank(target) {
			target = id
		}
	}
	if target == 0 {
		return UIRequest{}, ErrNoWindow
	}
	if r.Kind == UIShot && b.watchers[target].surface != "desktop" {
		return UIRequest{}, errors.New("screenshots need the Lazybox desktop app; only a browser tab is open")
	}
	r.ID, r.State, r.Created, r.watcher, r.done = newID(), UIPending, time.Now(), target, make(chan struct{})
	r.Error, r.Detail, r.PNG = "", "", nil
	select {
	case b.watchers[target].ch <- r:
	default:
		return UIRequest{}, errors.New("the window is busy with earlier requests; try again")
	}
	stored := r
	b.requests[r.ID] = &stored
	return public(stored), nil
}

// Complete records a window's answer. Only the window a request went to has
// it, so the id is the authority; a second answer is refused.
func (b *Broker) Complete(id, failure, detail string, png []byte) (UIRequest, error) {
	if len(png) > MaxShotBytes {
		failure, png = "the screenshot is larger than 32 MiB", nil
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	r, ok := b.requests[id]
	if !ok {
		return UIRequest{}, errors.New("no such request")
	}
	if r.State != UIPending {
		return UIRequest{}, errors.New("this request was already answered")
	}
	b.finishLocked(r, failure, detail, png)
	return public(*r), nil
}

func (b *Broker) finishLocked(r *UIRequest, failure, detail string, png []byte) {
	r.State, r.Error, r.Detail, r.PNG = UIDone, failure, detail, png
	if failure != "" {
		r.State = UIFailed
	}
	close(r.done)
}

// Wait returns the request's outcome, or its pending state at the timeout.
// poster is the hook run asking ("" for the owner): a hook waits only on what it posted.
func (b *Broker) Wait(ctx context.Context, id, poster string, timeout time.Duration) (UIRequest, error) {
	b.mu.Lock()
	r, ok := b.requests[id]
	b.mu.Unlock()
	if !ok || (poster != "" && r.Poster != poster) {
		return UIRequest{}, errors.New("no such request")
	}
	timer := time.NewTimer(timeout)
	defer timer.Stop()
	select {
	case <-r.done:
	case <-timer.C:
	case <-ctx.Done():
		return UIRequest{}, ctx.Err()
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	return public(*r), nil
}

func (b *Broker) expireLocked() {
	for id, r := range b.requests {
		if time.Since(r.Created) > requestTTL {
			if r.State == UIPending {
				b.finishLocked(r, "the window did not answer in time", "", nil)
			}
			delete(b.requests, id)
		}
	}
}

func public(r UIRequest) UIRequest {
	r.watcher, r.done = 0, nil
	return r
}

func newID() string {
	b := make([]byte, 12)
	_, _ = rand.Read(b)
	return hex.EncodeToString(b)
}
