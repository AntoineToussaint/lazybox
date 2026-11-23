package runner

import (
	"sync"
	"testing"
	"time"
)

// A lookup that arrives while a fingerprint is under way never takes it: that
// fingerprint may have read the files before the caller's commit. The late
// lookups share the next one, which reads the files after all of them arrived.
func TestLateLookupsNeverShareAFingerprintThatStartedBeforeThem(t *testing.T) {
	b := &Bench{}
	var mu sync.Mutex
	files, calls := "before the commit", 0
	reading, release := make(chan struct{}, 8), make(chan struct{})
	b.fingerprints.fingerprint = func(string) (string, error) {
		mu.Lock()
		calls++
		read := files
		mu.Unlock()
		reading <- struct{}{}
		<-release
		return read, nil
	}
	first := make(chan string, 1)
	go func() { first <- b.currentCandidate("ws") }()
	<-reading // the first fingerprint has read the files
	mu.Lock()
	files = "after the commit"
	mu.Unlock()
	late := make(chan string, 3)
	for range 3 {
		go func() { late <- b.currentCandidate("ws") }()
	}
	for deadline := time.Now().Add(5 * time.Second); ; time.Sleep(time.Millisecond) {
		b.fingerprints.mu.Lock()
		queued := b.fingerprints.workspaces["ws"] != nil && b.fingerprints.workspaces["ws"].next != nil
		b.fingerprints.mu.Unlock()
		if queued || time.Now().After(deadline) {
			break
		}
	}
	time.Sleep(50 * time.Millisecond) // let the other late lookups queue behind the first
	close(release)
	if got := <-first; got != "before the commit" {
		t.Fatalf("first lookup: %q", got)
	}
	for range 3 {
		if got := <-late; got != "after the commit" {
			t.Fatalf("a late lookup took a fingerprint that started before it: %q", got)
		}
	}
	mu.Lock()
	defer mu.Unlock()
	if calls > 3 {
		t.Fatalf("%d fingerprints for 4 lookups: late lookups did not share", calls)
	}
}

// A fingerprint that panics releases its waiters and the workspace: the next
// lookup fingerprints again instead of waiting forever.
func TestFingerprintPanicNeverHangsLaterLookups(t *testing.T) {
	b := &Bench{}
	panics := true
	b.fingerprints.fingerprint = func(string) (string, error) {
		if panics {
			panics = false
			panic("fingerprint failed")
		}
		return "candidate:after", nil
	}
	func() {
		defer func() { _ = recover() }()
		b.currentCandidate("ws")
	}()
	done := make(chan string, 1)
	go func() { done <- b.currentCandidate("ws") }()
	select {
	case got := <-done:
		if got != "candidate:after" {
			t.Fatal(got)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("a lookup after a panic hung")
	}
}
