package reviews

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestLocateFindsTheNearestOccurrenceAndIgnoresTrailingWhitespace(t *testing.T) {
	file := []string{"a", "x", "y", "b", "x", "y  ", "c", "x", "y"}
	if got := Locate(file, []string{"x", "y"}, 5); got != 5 {
		t.Fatalf("same place: %d", got)
	}
	if got := Locate(file, []string{"x", "y"}, 7); got != 8 {
		t.Fatalf("nearest after: %d", got)
	}
	if got := Locate(file, []string{"x", "y"}, 1); got != 2 {
		t.Fatalf("nearest before: %d", got)
	}
	// Equidistant: the earlier occurrence wins.
	if got := Locate([]string{"x", "-", "x"}, []string{"x"}, 2); got != 1 {
		t.Fatalf("tie: %d", got)
	}
	if got := Locate(file, []string{"x", "z"}, 2); got != 0 {
		t.Fatalf("absent lines found at %d", got)
	}
	if Locate(file, nil, 1) != 0 || Locate([]string{"a"}, []string{"a", "b"}, 1) != 0 {
		t.Fatal("empty or oversized snippet located")
	}
	if Hash([]string{"x ", "y\r"}) != Hash([]string{"x", "y"}) || Hash([]string{"x", "y"}) == Hash([]string{"xy"}) {
		t.Fatal("hash must ignore trailing whitespace and keep line breaks")
	}
}

func TestEvaluateReportsCurrentMovedOutdatedAndRemovedLines(t *testing.T) {
	original := "package api\n\nfunc Handle() {\n\treturn\n}\n"
	c := Comment{Side: SideNew, StartLine: 3, Snippet: []string{"func Handle() {", "\treturn"}, FileHash: FileHash([]byte(original))}
	if a := Evaluate(c, []byte(original), true); a.Freshness != Current || a.StartLine != 3 || a.EndLine != 4 {
		t.Fatalf("unchanged: %+v", a)
	}
	moved := "package api\n\nimport \"fmt\"\n\nfunc Handle() {\n\treturn\n}\n"
	if a := Evaluate(c, []byte(moved), true); a.Freshness != Moved || a.StartLine != 5 || a.EndLine != 6 || a.Reason == "" {
		t.Fatalf("moved: %+v", a)
	}
	changed := "package api\n\nfunc Handle() error {\n\treturn nil\n}\n"
	if a := Evaluate(c, []byte(changed), true); a.Freshness != Outdated || a.StartLine != 0 {
		t.Fatalf("changed: %+v", a)
	}
	if a := Evaluate(c, nil, false); a.Freshness != Outdated || !strings.Contains(a.Reason, "no longer exists") {
		t.Fatalf("deleted: %+v", a)
	}
	removed := Comment{Side: SideOld, StartLine: 3, Snippet: []string{"old()"}, FileHash: FileHash([]byte(original))}
	if a := Evaluate(removed, []byte(original), true); a.Freshness != Current {
		t.Fatalf("removed lines, file unchanged: %+v", a)
	}
	if a := Evaluate(removed, []byte(moved), true); a.Freshness != Outdated {
		t.Fatalf("removed lines, file changed: %+v", a)
	}
}

func newStore(t *testing.T) *Store {
	t.Helper()
	s, err := New(filepath.Join(t.TempDir(), "reviews"))
	if err != nil {
		t.Fatal(err)
	}
	clock := time.Unix(1_800_000_000, 0)
	s.Now = func() time.Time { clock = clock.Add(time.Second); return clock }
	return s
}

func sample() Comment {
	return Comment{Member: "api", Path: "src/handler.go", Side: SideNew, StartLine: 12, Snippet: []string{"if err != nil {", "\treturn"}, Body: "Wrap this error.\n\n", Head: "abc", View: "uncommitted"}
}

func TestCommentsPersistPrivatelyAcrossReopen(t *testing.T) {
	s := newStore(t)
	added, err := s.Add("ws-1", sample())
	if err != nil {
		t.Fatal(err)
	}
	if added.ID == "" || added.EndLine != 13 || added.Body != "Wrap this error." || added.Disposition != Open || added.Hash != Hash(sample().Snippet) || added.CreatedAt == 0 {
		t.Fatalf("added: %+v", added)
	}
	second, err := s.Add("ws-1", Comment{Member: "api", Path: "README.md", Side: SideOld, StartLine: 1, Snippet: []string{"# Old"}, Body: "Keep it", View: "branch", Base: "main"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = s.Add("ws-2", sample()); err != nil {
		t.Fatal(err)
	}
	reopened, err := New(s.dir)
	if err != nil {
		t.Fatal(err)
	}
	review, err := reopened.Load("ws-1")
	if err != nil || len(review.Comments) != 2 || review.Comments[0].ID != added.ID || review.Comments[1].Base != "main" || review.AutoResolve {
		t.Fatalf("reopened: %+v %v", review, err)
	}
	stat, err := os.Stat(filepath.Join(s.dir, "ws-1.json"))
	if err != nil || stat.Mode().Perm() != 0o600 {
		t.Fatalf("review file is not private: %v %v", stat, err)
	}
	if review, err = s.Edit("ws-1", added.ID, "Wrap it with context"); err != nil || review.Comments[0].Body != "Wrap it with context" || review.Comments[0].UpdatedAt <= added.UpdatedAt {
		t.Fatalf("edit: %+v %v", review, err)
	}
	if review, err = s.Delete("ws-1", second.ID); err != nil || len(review.Comments) != 1 {
		t.Fatalf("delete: %+v %v", review, err)
	}
	if _, err = s.Delete("ws-1", second.ID); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("deleting twice: %v", err)
	}
	if other, _ := s.Load("ws-2"); len(other.Comments) != 1 {
		t.Fatal("workspaces share comments")
	}
	if err = s.Forget("ws-2"); err != nil {
		t.Fatal(err)
	}
	if other, _ := s.Load("ws-2"); len(other.Comments) != 0 {
		t.Fatal("forgotten workspace kept its comments")
	}
}

func TestInvalidCommentsAreRefusedAndChangeNothing(t *testing.T) {
	s := newStore(t)
	for name, change := range map[string]func(*Comment){
		"empty body":    func(c *Comment) { c.Body = " \n" },
		"no member":     func(c *Comment) { c.Member = "" },
		"member path":   func(c *Comment) { c.Member = "../api" },
		"absolute path": func(c *Comment) { c.Path = "/etc/passwd" },
		"escaping path": func(c *Comment) { c.Path = "../x" },
		"side":          func(c *Comment) { c.Side = "left" },
		"no lines":      func(c *Comment) { c.Snippet = nil },
		"line zero":     func(c *Comment) { c.StartLine = 0 },
		"view":          func(c *Comment) { c.View = "pr" },
		"huge body":     func(c *Comment) { c.Body = strings.Repeat("x", bodyLimit+1) },
	} {
		c := sample()
		change(&c)
		if _, err := s.Add("ws", c); err == nil {
			t.Errorf("%s accepted", name)
		}
	}
	if _, err := s.Add("../escape", sample()); err == nil {
		t.Fatal("workspace id escaping the directory accepted")
	}
	if review, _ := s.Load("ws"); len(review.Comments) != 0 {
		t.Fatal("refused comments were stored")
	}
	added, _ := s.Add("ws", sample())
	if _, err := s.Edit("ws", added.ID, "  "); err == nil {
		t.Fatal("blank edit accepted")
	}
	if _, err := s.SetDisposition("ws", []string{added.ID}, "done", Handover{}); err == nil {
		t.Fatal("unknown disposition accepted")
	}
	if _, err := s.SetDisposition("ws", []string{added.ID}, Sent, Handover{}); err == nil {
		t.Fatal("sent without a conversation")
	}
	if _, err := s.SetDisposition("ws", []string{added.ID, "missing"}, Resolved, Handover{}); err == nil {
		t.Fatal("unknown id accepted")
	}
	if review, _ := s.Load("ws"); review.Comments[0].Disposition != Open {
		t.Fatal("a failed batch changed a comment")
	}
}

func TestDispositionsRecordTheHandoverAndAutoResolveOnlySentComments(t *testing.T) {
	s := newStore(t)
	a, _ := s.Add("ws", sample())
	b, _ := s.Add("ws", sample())
	review, err := s.SetDisposition("ws", []string{a.ID, b.ID}, Sent, Handover{Session: "s1", Title: "Fix errors", Heads: map[string]string{"api": "abc"}})
	if err != nil || review.Comments[0].SentTo != "s1" || review.Comments[1].SentToTitle != "Fix errors" || review.Comments[0].SentHead != "abc" || review.Comments[0].SentAt == 0 {
		t.Fatalf("sent: %+v %v", review, err)
	}
	// Off by default: nothing resolves on its own.
	if changed, err := s.ResolveByCommit("ws", a.ID, "def4567890"); err != nil || changed {
		t.Fatalf("resolved without the preference: %v %v", changed, err)
	}
	if review, _ = s.SetAutoResolve("ws", true); !review.AutoResolve {
		t.Fatal("preference not saved")
	}
	if review, _ = s.SetDisposition("ws", []string{b.ID}, Open, Handover{}); review.Comments[1].SentTo != "" || review.Comments[1].SentAt != 0 {
		t.Fatal("taking a comment back kept its handover")
	}
	if changed, _ := s.ResolveByCommit("ws", b.ID, "def4567890"); changed {
		t.Fatal("an open comment resolved on its own")
	}
	changed, err := s.ResolveByCommit("ws", a.ID, "def4567890")
	review, _ = s.Load("ws")
	if err != nil || !changed || review.Comments[0].Disposition != Resolved || review.Comments[0].ResolvedBy != "def4567890" || !strings.Contains(review.Comments[0].Note, "def45678") {
		t.Fatalf("auto-resolve: %+v %v", review.Comments[0], err)
	}
	// Reopening clears what the Runner decided.
	if review, _ = s.SetDisposition("ws", []string{a.ID}, Open, Handover{}); review.Comments[0].ResolvedBy != "" || review.Comments[0].Note != "" {
		t.Fatalf("reopen kept the resolution: %+v", review.Comments[0])
	}
	if review, _ = s.SetDisposition("ws", []string{a.ID}, WontFix, Handover{}); review.Comments[0].Disposition != WontFix {
		t.Fatal("won't fix not recorded")
	}
}
