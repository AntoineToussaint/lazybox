package artifacts

import (
	"context"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

type clock struct{ t time.Time }

func (c *clock) now() time.Time { return c.t }

func open(t *testing.T, root string) (*Store, *clock) {
	t.Helper()
	s, err := Open(root)
	if err != nil {
		t.Fatal(err)
	}
	c := &clock{t: time.UnixMilli(1_000_000)}
	s.now = c.now
	return s, c
}

var person = Author{Kind: "person"}

func TestPutCreatesAppendsAndIsIdempotent(t *testing.T) {
	root := t.TempDir()
	s, c := open(t, root)
	source := Source{Kind: "message", SessionID: "exec", ConversationID: "conv", MessageID: "42", TurnID: "t1"}
	doc, err := s.Put("alpha", Put{Title: "  Rollout plan ", Kind: "plan", Content: "# Plan\nstep 1", Author: person, Source: source})
	if err != nil {
		t.Fatal(err)
	}
	if doc.Title != "Rollout plan" || doc.Kind != "plan" || doc.Format != "markdown" || doc.Latest() != 1 || doc.Content != "# Plan\nstep 1" || doc.ContentVersion != 1 {
		t.Fatalf("%+v", doc)
	}
	if doc.OriginSessionID != "exec" || doc.OriginConversationID != "conv" || doc.Versions[0].Source != source || doc.Versions[0].Digest != Digest("# Plan\nstep 1") {
		t.Fatalf("%+v", doc)
	}
	// A retry of the same content writes nothing.
	c.t = c.t.Add(time.Minute)
	same, err := s.Put("alpha", Put{ID: doc.ID, Content: "# Plan\nstep 1", Author: person})
	if err != nil || same.Latest() != 1 || same.UpdatedAt != doc.UpdatedAt {
		t.Fatal(same, err)
	}
	// A rename alone changes metadata, not versions.
	renamed, err := s.Put("alpha", Put{ID: doc.ID, Title: "Plan B", Content: "# Plan\nstep 1", Author: person})
	if err != nil || renamed.Latest() != 1 || renamed.Title != "Plan B" || renamed.Kind != "plan" {
		t.Fatal(renamed, err)
	}
	agent := Author{Kind: "agent", SessionID: "exec"}
	next, err := s.Put("alpha", Put{ID: doc.ID, Content: "# Plan\nstep 2", Author: agent, Source: Source{Kind: "agent", SessionID: "exec"}, ExpectedVersion: 1})
	if err != nil || next.Latest() != 2 || next.Content != "# Plan\nstep 2" || next.Versions[1].Author != agent {
		t.Fatal(next, err)
	}
	if _, err = s.Put("alpha", Put{ID: doc.ID, Content: "stale", Author: person, ExpectedVersion: 1}); !errors.Is(err, ErrConflict) || !strings.Contains(err.Error(), "v2") {
		t.Fatal("stale write accepted", err)
	}
	first, err := s.Get("alpha", doc.ID, 1)
	if err != nil || first.Content != "# Plan\nstep 1" || first.ContentVersion != 1 {
		t.Fatal(first, err)
	}
	if _, err = s.Get("alpha", doc.ID, 3); !errors.Is(err, fs.ErrNotExist) {
		t.Fatal(err)
	}
	if _, err = s.Get("beta", doc.ID, 0); !errors.Is(err, fs.ErrNotExist) {
		t.Fatal("another workspace reads it", err)
	}
	if _, err = s.Put("beta", Put{ID: doc.ID, Content: "x", Author: person}); !errors.Is(err, fs.ErrNotExist) {
		t.Fatal("another workspace writes it", err)
	}
	// Everything survives a reopen.
	reopened, _ := open(t, root)
	list, err := reopened.List("alpha")
	if err != nil || len(list) != 1 || list[0].Title != "Plan B" || list[0].Latest() != 2 {
		t.Fatal(list, err)
	}
	latest, err := reopened.Get("alpha", doc.ID, 0)
	if err != nil || latest.Content != "# Plan\nstep 2" {
		t.Fatal(latest, err)
	}
}

func TestPutValidates(t *testing.T) {
	s, _ := open(t, t.TempDir())
	cases := []struct {
		ws string
		p  Put
	}{
		{"alpha", Put{Title: "", Content: "x"}},
		{"alpha", Put{Title: strings.Repeat("a", MaxTitle+1), Content: "x"}},
		{"alpha", Put{Title: "two\nlines", Content: "x"}},
		{"alpha", Put{Title: "T", Kind: "memo", Content: "x"}},
		{"alpha", Put{Title: "T", Content: "  \n"}},
		{"alpha", Put{Title: "T", Content: strings.Repeat("a", MaxContent+1)}},
		{"alpha", Put{Title: "T", Content: "bad \xff"}},
		{"../x", Put{Title: "T", Content: "x"}},
		{"", Put{Title: "T", Content: "x"}},
	}
	for _, c := range cases {
		if _, err := s.Put(c.ws, c.p); !errors.Is(err, ErrInvalid) {
			t.Fatalf("%q %+v accepted: %v", c.ws, c.p.Title, err)
		}
	}
	if _, err := s.Put("alpha", Put{Title: "T", Content: "x", Links: []Link{{Kind: "mission", ID: "m", Role: "brief"}}}); err == nil {
		t.Fatal("accepted a mission link")
	}
	doc, err := s.Put("alpha", Put{Title: "T", Content: "x"})
	if err != nil || doc.Kind != "other" {
		t.Fatal(doc, err)
	}
	if _, err = s.Put("alpha", Put{ID: "../../etc", Content: "y"}); !errors.Is(err, fs.ErrNotExist) {
		t.Fatal(err)
	}
}

func TestVersionLimit(t *testing.T) {
	s, _ := open(t, t.TempDir())
	doc, err := s.Put("alpha", Put{Title: "T", Content: "0"})
	if err != nil {
		t.Fatal(err)
	}
	a := s.items["alpha"][doc.ID]
	for len(a.Versions) < MaxVersions {
		a.Versions = append(a.Versions, Version{Number: a.Latest() + 1, Digest: "x"})
	}
	if _, err = s.Put("alpha", Put{ID: doc.ID, Content: "next"}); err == nil || !strings.Contains(err.Error(), "1000 versions") {
		t.Fatal(err)
	}
}

func TestTrackUpdateDelete(t *testing.T) {
	root := t.TempDir()
	s, _ := open(t, root)
	doc, err := s.Track("alpha", "api/docs/PLAN.md", "", "plan", "intro\n```\n# not this\n```\n# Data source plan\n", person)
	if err != nil || doc.Title != "Data source plan" || doc.TrackedPath != "api/docs/PLAN.md" || doc.Versions[0].Source.Kind != "file" || doc.Versions[0].Source.Path != "api/docs/PLAN.md" {
		t.Fatal(doc, err)
	}
	again, err := s.Track("alpha", "api/docs/PLAN.md", "Other", "spec", "different", person)
	if err != nil || again.ID != doc.ID || again.Title != "Data source plan" || again.Latest() != 1 {
		t.Fatal("tracking twice made another artifact", again, err)
	}
	named, err := s.Track("alpha", "api/notes/meeting-notes.md", "", "", "no heading", person)
	if err != nil || named.Title != "meeting-notes" || named.Kind != "other" {
		t.Fatal(named, err)
	}
	if _, err = s.SetTracked("alpha", named.ID, "api/docs/PLAN.md"); err == nil {
		t.Fatal("two artifacts follow one file")
	}
	updated, err := s.Update("alpha", doc.ID, "Renamed", "spec", true)
	if err != nil || updated.Title != "Renamed" || updated.Kind != "spec" || updated.TrackedPath != "" {
		t.Fatal(updated, err)
	}
	if _, err = s.Update("alpha", doc.ID, "", "memo", false); !errors.Is(err, ErrInvalid) {
		t.Fatal(err)
	}
	if err = s.Delete("alpha", doc.ID); err != nil {
		t.Fatal(err)
	}
	if _, err = os.Stat(filepath.Join(root, "alpha", doc.ID)); !errors.Is(err, fs.ErrNotExist) {
		t.Fatal("versions kept on disk", err)
	}
	if err = s.Delete("alpha", doc.ID); !errors.Is(err, fs.ErrNotExist) {
		t.Fatal(err)
	}
	if err = s.DeleteWorkspace("alpha"); err != nil {
		t.Fatal(err)
	}
	reopened, _ := open(t, root)
	if list, _ := reopened.List("alpha"); len(list) != 0 {
		t.Fatal(list)
	}
}

func TestWatchRecordsStableChangesOnly(t *testing.T) {
	root := t.TempDir()
	s, c := open(t, root)
	doc, err := s.Track("alpha", "api/PLAN.md", "", "plan", "# Plan\nv1", person)
	if err != nil {
		t.Fatal(err)
	}
	file, problem := "# Plan\nv1", ""
	reads := 0
	read := func(_ context.Context, ws, path string) (string, string, error) {
		reads++
		if ws != "alpha" || path != "api/PLAN.md" {
			t.Fatalf("read %s %s", ws, path)
		}
		return file, problem, nil
	}
	ctx := context.Background()
	const debounce = 2 * time.Second
	latest := func() Artifact { d, _ := s.Get("alpha", doc.ID, 0); return d.Artifact }
	s.Poll(ctx, read, debounce)
	if latest().Latest() != 1 {
		t.Fatal("unchanged file recorded")
	}
	// Still being written: each poll sees different content.
	file = "# Plan\nv2 partial"
	s.Poll(ctx, read, debounce)
	c.t = c.t.Add(3 * time.Second)
	file = "# Plan\nv2 complete"
	s.Poll(ctx, read, debounce)
	if latest().Latest() != 1 {
		t.Fatal("recorded content that was still changing")
	}
	c.t = c.t.Add(time.Second)
	s.Poll(ctx, read, debounce)
	if latest().Latest() != 1 {
		t.Fatal("recorded before the debounce")
	}
	c.t = c.t.Add(time.Second)
	s.Poll(ctx, read, debounce)
	got := latest()
	if got.Latest() != 2 || got.Versions[1].Author.Kind != "file" || got.Versions[1].Source.Path != "api/PLAN.md" {
		t.Fatalf("%+v", got.Versions)
	}
	if d, _ := s.Get("alpha", doc.ID, 2); d.Content != "# Plan\nv2 complete" {
		t.Fatal(d.Content)
	}
	// A removed file is a problem, not a version; it clears when the file returns.
	problem = "the file was removed; the last version is kept"
	s.Poll(ctx, read, debounce)
	if p := latest().TrackingProblem; p != problem {
		t.Fatal(p)
	}
	reopened, _ := open(t, root)
	if d, _ := reopened.Get("alpha", doc.ID, 0); d.TrackingProblem != problem {
		t.Fatal("problem not persisted")
	}
	problem = ""
	s.Poll(ctx, read, debounce)
	if got = latest(); got.TrackingProblem != "" || got.Latest() != 2 {
		t.Fatalf("%+v", got)
	}
	// An error reading is a problem too; untracked files are not read.
	if _, err = s.Update("alpha", doc.ID, "", "", true); err != nil {
		t.Fatal(err)
	}
	before := reads
	s.Poll(ctx, read, debounce)
	if reads != before {
		t.Fatal("read an untracked file")
	}
}

func TestWatchStopsWithContext(t *testing.T) {
	s, _ := open(t, t.TempDir())
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		s.Watch(ctx, func(context.Context, string, string) (string, string, error) { return "", "", nil }, time.Millisecond, time.Millisecond)
		close(done)
	}()
	cancel()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("watcher kept running")
	}
}

func TestRetireKeepsEveryVersionAndRestores(t *testing.T) {
	root := t.TempDir()
	s, c := open(t, root)
	agent := Author{Kind: "agent", SessionID: "exec"}
	old, err := s.Put("alpha", Put{Title: "What's left", Kind: "plan", Content: "# Left\n- a", Author: agent, Source: Source{Kind: "agent", SessionID: "exec"}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = s.Put("alpha", Put{ID: old.ID, Content: "# Left\n- a\n- b", Author: agent, Source: Source{Kind: "agent", SessionID: "exec"}}); err != nil {
		t.Fatal(err)
	}
	status, err := s.Put("alpha", Put{Title: "Status", Kind: "plan", Content: "# Status", Author: agent, Source: Source{Kind: "agent", SessionID: "exec"}})
	if err != nil {
		t.Fatal(err)
	}
	if !old.AgentPublished() || !status.AgentPublished() {
		t.Fatal("agent-written artifacts read as not agent-published")
	}

	c.t = c.t.Add(time.Minute)
	retired, err := s.Retire("alpha", old.ID, agent, status.ID, " merged into the status page ")
	if err != nil {
		t.Fatal(err)
	}
	want := Retirement{At: c.t.UnixMilli(), By: agent, SupersededBy: status.ID, Reason: "merged into the status page"}
	if retired.Retired == nil || *retired.Retired != want || retired.Latest() != 2 {
		t.Fatalf("%+v", retired.Retired)
	}
	// The store still lists it (the transport filters); every version is readable.
	if list, _ := s.List("alpha"); len(list) != 2 {
		t.Fatal(list)
	}
	if v1, err := s.Get("alpha", old.ID, 1); err != nil || v1.Content != "# Left\n- a" || v1.Retired == nil {
		t.Fatal(v1, err)
	}
	// It survives a reopen.
	reopened, _ := open(t, root)
	if doc, err := reopened.Get("alpha", old.ID, 0); err != nil || doc.Retired == nil || *doc.Retired != want {
		t.Fatal(doc, err)
	}

	for name, check := range map[string]func() error{
		"supersedes itself":  func() error { _, err := s.Retire("alpha", old.ID, agent, old.ID, ""); return err },
		"superseded by none": func() error { _, err := s.Retire("alpha", old.ID, agent, "missing", ""); return err },
		"two-line reason":    func() error { _, err := s.Retire("alpha", old.ID, agent, "", "a\nb"); return err },
		"over-long reason": func() error {
			_, err := s.Retire("alpha", old.ID, agent, "", strings.Repeat("x", MaxReason+1))
			return err
		},
	} {
		if err := check(); !errors.Is(err, ErrInvalid) {
			t.Errorf("%s: %v", name, err)
		}
	}
	if _, err = s.Retire("beta", old.ID, agent, "", ""); !errors.Is(err, fs.ErrNotExist) {
		t.Fatal("retired across workspaces", err)
	}

	restored, err := s.Restore("alpha", old.ID)
	if err != nil || restored.Retired != nil || restored.Latest() != 2 {
		t.Fatal(restored, err)
	}
	if same, err := s.Restore("alpha", old.ID); err != nil || same.Retired != nil {
		t.Fatal("restoring a live artifact", same, err)
	}
	reopened, _ = open(t, root)
	if got, err := reopened.Get("alpha", old.ID, 0); err != nil || got.Retired != nil {
		t.Fatal("restore did not persist", got, err)
	}
}

func TestAgentPublishedExcludesThePersonsAndTrackedFiles(t *testing.T) {
	s, _ := open(t, t.TempDir())
	agent := Author{Kind: "agent", SessionID: "exec"}
	mine, err := s.Put("alpha", Put{Title: "Mine", Content: "x", Author: person, Source: Source{Kind: "edit"}})
	if err != nil {
		t.Fatal(err)
	}
	edited, err := s.Put("alpha", Put{Title: "Edited", Content: "x", Author: agent, Source: Source{Kind: "agent"}})
	if err != nil {
		t.Fatal(err)
	}
	if edited, err = s.Put("alpha", Put{ID: edited.ID, Content: "y", Author: person, Source: Source{Kind: "edit"}}); err != nil {
		t.Fatal(err)
	}
	tracked, err := s.Track("alpha", "api/PLAN.md", "", "plan", "# Plan", agent)
	if err != nil {
		t.Fatal(err)
	}
	for _, a := range []Artifact{mine.Artifact, edited.Artifact, tracked.Artifact} {
		if a.AgentPublished() {
			t.Errorf("%q reads as agent-published", a.Title)
		}
	}
}
