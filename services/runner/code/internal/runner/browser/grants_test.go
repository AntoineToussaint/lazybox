package browser

import (
	"context"
	"testing"
	"time"
)

// openAllowedOnce opens docs.example for alice, the person allowing it once.
func openAllowedOnce(t *testing.T, m *Manager) {
	t.Helper()
	done := make(chan error, 1)
	go func() {
		_, err := call(t, m, alice, "browser_open", map[string]string{"url": "https://docs.example/"})
		done <- err
	}()
	r := waitRequest(t, m, "conv-a")
	_, _ = m.Decide(r.ID, "once", "")
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

// pageMovesWhileThePersonHoldsIt: the person takes the browser, and the page
// goes to moved.example on its own (a script, a meta refresh) before they give
// it back.
func pageMovesWhileThePersonHoldsIt(t *testing.T, m *Manager) {
	t.Helper()
	ctx := context.Background()
	if _, err := m.Control(ctx, alice, "take_over", ""); err != nil {
		t.Fatal(err)
	}
	if _, err := m.Input(ctx, "conv-a", map[string]any{"type": "text", "text": "page-navigates-itself"}); err != nil {
		t.Fatal(err)
	}
	if st, _ := m.State("conv-a"); st.URL != "https://moved.example/" {
		t.Fatalf("the page did not move: %+v", st)
	}
	if _, err := m.Control(ctx, alice, "give_back", ""); err != nil {
		t.Fatal(err)
	}
}

// expectAsk runs an agent call that must ask the person about origin before
// it goes on; the person declines.
func expectAsk(t *testing.T, m *Manager, origin, tool string, args any) {
	t.Helper()
	done := make(chan error, 1)
	go func() {
		_, err := call(t, m, alice, tool, args)
		done <- err
	}()
	deadline := time.Now().Add(5 * time.Second)
	for {
		if p := m.Pending("conv-a"); len(p) > 0 {
			if p[0].Origin != origin {
				t.Fatalf("asked about %s, want %s", p[0].Origin, origin)
			}
			_, _ = m.Decide(p[0].ID, "deny", "")
			if err := <-done; err == nil {
				t.Fatalf("%s went on after the person declined", tool)
			}
			return
		}
		select {
		case err := <-done:
			t.Fatalf("%s did not ask about %s: %v", tool, origin, err)
		case <-time.After(10 * time.Millisecond):
		}
		if time.Now().After(deadline) {
			t.Fatalf("%s neither asked nor returned", tool)
		}
	}
}

// Regression: a page that moved itself while the person held the browser
// became a site the agent could open without asking.
func TestAPageMovingItselfDuringTakeOverGrantsNothing(t *testing.T) {
	m := newManager(t)
	openAllowedOnce(t, m)
	pageMovesWhileThePersonHoldsIt(t, m)
	expectAsk(t, m, "https://moved.example", "browser_navigate", map[string]string{"url": "https://moved.example/more"})
}

// Regression: a navigation the page made before the person's first input,
// which the service asks about, was allowed and granted because the person
// held the browser.
func TestTheServicesAskDuringTakeOverAsksThePerson(t *testing.T) {
	m := newManager(t)
	openAllowedOnce(t, m)
	ctx := context.Background()
	if _, err := m.Control(ctx, alice, "take_over", ""); err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() {
		_, err := m.Input(ctx, "conv-a", map[string]any{"type": "text", "text": "page-asks-site"})
		done <- err
	}()
	r := waitRequest(t, m, "conv-a")
	if r.Origin != "https://asked.example" {
		t.Fatalf("%+v", r)
	}
	_, _ = m.Decide(r.ID, "deny", "")
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	if st, _ := m.State("conv-a"); st.URL == "https://asked.example/x" {
		t.Fatalf("the page went where the person declined: %+v", st)
	}
}

// Regression: browser_open with no URL, after the browser closed, granted
// the site it was last on without asking.
func TestReopeningWhereTheBrowserWasAsksForASiteNotAllowed(t *testing.T) {
	m := newManager(t)
	openAllowedOnce(t, m)
	pageMovesWhileThePersonHoldsIt(t, m)
	if _, err := call(t, m, alice, "browser_close", nil); err != nil {
		t.Fatal(err)
	}
	expectAsk(t, m, "https://moved.example", "browser_open", nil)

	// A site still allowed reopens without asking.
	m2 := newManager(t)
	openAllowedOnce(t, m2)
	if _, err := call(t, m2, alice, "browser_close", nil); err != nil {
		t.Fatal(err)
	}
	if _, err := call(t, m2, alice, "browser_open", nil); err != nil || len(m2.Pending("conv-a")) != 0 {
		t.Fatalf("reopening an allowed site: %v", err)
	}
	if st, _ := m2.State("conv-a"); st.URL != "https://docs.example/" {
		t.Fatalf("not back where it was: %+v", st)
	}
}
