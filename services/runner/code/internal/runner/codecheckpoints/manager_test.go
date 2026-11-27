package codecheckpoints

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"testing"
	"time"
)

func run(t *testing.T, dir string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", append([]string{"-C", dir}, args...)...)
	cmd.Env = append(gitEnv(""), "GIT_CONFIG_GLOBAL=/dev/null", "GIT_CONFIG_NOSYSTEM=1")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v\n%s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

func write(t *testing.T, path, text string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(text), 0644); err != nil {
		t.Fatal(err)
	}
}

func read(t *testing.T, path string) string {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(data)
}

func exists(path string) bool { _, err := os.Lstat(path); return err == nil }

// newRepo is a repository with one commit: tracked files, a binary file and
// a .gitignore.
func newRepo(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	run(t, dir, "init", "-q", "-b", "main")
	run(t, dir, "config", "user.name", "Person")
	run(t, dir, "config", "user.email", "person@example.invalid")
	write(t, filepath.Join(dir, "src/main.go"), "package main\n\nfunc main() {}\n")
	write(t, filepath.Join(dir, "README.md"), "# Project\n")
	write(t, filepath.Join(dir, "old-name.txt"), "a file that will be renamed\nwith enough lines\nto be detected\nas a rename\n")
	write(t, filepath.Join(dir, ".gitignore"), "build/\n*.log\n")
	if err := os.WriteFile(filepath.Join(dir, "logo.bin"), []byte{0, 1, 2, 3, 0xff, 0}, 0644); err != nil {
		t.Fatal(err)
	}
	run(t, dir, "add", "-A")
	run(t, dir, "commit", "-q", "-m", "initial")
	return dir
}

// gitState is everything a checkpoint must leave alone.
type gitState struct {
	Head, Branch, Status, Staged, Stash, IndexEntries string
}

func state(t *testing.T, dir string) gitState {
	t.Helper()
	return gitState{
		Head:         run(t, dir, "rev-parse", "HEAD"),
		Branch:       run(t, dir, "symbolic-ref", "HEAD"),
		Status:       run(t, dir, "status", "--porcelain=v1", "--untracked-files=all", "--ignored"),
		Staged:       run(t, dir, "diff", "--cached", "--name-status"),
		Stash:        run(t, dir, "stash", "list"),
		IndexEntries: run(t, dir, "ls-files", "-s"),
	}
}

func manager(t *testing.T, limits Limits) *Manager {
	t.Helper()
	m, err := Open(filepath.Join(t.TempDir(), "checkpoints"), limits)
	if err != nil {
		t.Fatal(err)
	}
	return m
}

func ctx(t *testing.T) context.Context {
	c, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	t.Cleanup(cancel)
	return c
}

func treeFiles(t *testing.T, dir, commit string) []string {
	t.Helper()
	files := strings.Split(run(t, dir, "ls-tree", "-r", "--name-only", commit), "\n")
	sort.Strings(files)
	return files
}

func TestCheckpointCapturesUncommittedAndUntrackedWithoutTouchingIndexHeadOrStash(t *testing.T) {
	dir := newRepo(t)
	write(t, filepath.Join(dir, "README.md"), "# Project\n\nUnstaged edit.\n")
	write(t, filepath.Join(dir, "src/staged.go"), "package main\n")
	run(t, dir, "add", "src/staged.go")
	write(t, filepath.Join(dir, "notes/todo.txt"), "untracked\n")
	write(t, filepath.Join(dir, "build/out.o"), "ignored\n")
	write(t, filepath.Join(dir, "debug.log"), "ignored\n")
	before := state(t, dir)
	// Read after git status, which may itself refresh the index.
	indexPath := filepath.Join(dir, ".git", "index")
	indexBefore, err := os.ReadFile(indexPath)
	if err != nil {
		t.Fatal(err)
	}
	infoBefore, _ := os.Stat(indexPath)

	m := manager(t, Limits{})
	turn, err := m.BeginTurn(ctx(t), "conv1", []Repository{{Member: "app", Path: dir}}, "op-1", "Add the parser\nwith details")
	if err != nil {
		t.Fatal(err)
	}
	if turn.Seq != 1 || turn.Prompt != "Add the parser" || len(turn.Before.Repos) != 1 || !turn.Before.Repos[0].ok() {
		t.Fatalf("turn = %+v", turn)
	}
	indexAfter, _ := os.ReadFile(indexPath)
	infoAfter, _ := os.Stat(indexPath)
	if !bytes.Equal(indexBefore, indexAfter) || !infoBefore.ModTime().Equal(infoAfter.ModTime()) {
		t.Fatal("checkpoint wrote the repository's index")
	}
	point := turn.Before.Repos[0]
	if got := run(t, dir, "rev-parse", RefPrefix+"conv1/turn-1-before"); got != point.Commit {
		t.Fatalf("private ref = %s, want %s", got, point.Commit)
	}
	files := treeFiles(t, dir, point.Commit)
	want := []string{".gitignore", "README.md", "logo.bin", "notes/todo.txt", "old-name.txt", "src/main.go", "src/staged.go"}
	if !reflect.DeepEqual(files, want) {
		t.Fatalf("checkpoint files = %v, want %v (ignored files excluded)", files, want)
	}
	if got := run(t, dir, "show", point.Commit+":README.md"); !strings.Contains(got, "Unstaged edit.") {
		t.Fatalf("checkpoint holds the committed README, not the working copy: %q", got)
	}
	if parent := run(t, dir, "rev-parse", point.Commit+"^"); parent != before.Head {
		t.Fatalf("checkpoint parent = %s, want HEAD %s", parent, before.Head)
	}
	if after := state(t, dir); after != before {
		t.Fatalf("checkpoint changed the repository:\nbefore %+v\nafter  %+v", before, after)
	}
	if branches := run(t, dir, "for-each-ref", "--format=%(refname)", "refs/heads", "refs/stash"); branches != "refs/heads/main" {
		t.Fatalf("branches or stash changed: %s", branches)
	}
}

func TestUnchangedFilesReuseTheCheckpointAndWriteNoObjects(t *testing.T) {
	dir := newRepo(t)
	write(t, filepath.Join(dir, "draft.txt"), "untracked\n")
	m := manager(t, Limits{})
	repos := []Repository{{Member: "app", Path: dir}}
	first, err := m.BeginTurn(ctx(t), "conv", repos, "op-1", "one")
	if err != nil {
		t.Fatal(err)
	}
	objects := run(t, dir, "count-objects", "-v")
	if err = m.EndTurn(ctx(t), "conv", repos, "", time.Now()); err != nil {
		t.Fatal(err)
	}
	second, err := m.BeginTurn(ctx(t), "conv", repos, "op-2", "two")
	if err != nil {
		t.Fatal(err)
	}
	if second.Before.Repos[0].Commit != first.Before.Repos[0].Commit {
		t.Fatal("an unchanged work tree got a new checkpoint commit")
	}
	if after := run(t, dir, "count-objects", "-v"); after != objects {
		t.Fatalf("unchanged checkpoints wrote objects:\n%s\n%s", objects, after)
	}
	rec, _ := m.List("conv")
	if rec.Turns[0].Files != 0 || rec.Turns[0].After == nil {
		t.Fatalf("turn 1 = %+v", rec.Turns[0])
	}
}

// agentTurn is what an agent does in one turn: every kind of change.
func agentTurn(t *testing.T, dir string) {
	write(t, filepath.Join(dir, "src/main.go"), "package main\n\nfunc main() { run() }\n")
	if err := os.Remove(filepath.Join(dir, "README.md")); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(filepath.Join(dir, "old-name.txt"), filepath.Join(dir, "new-name.txt")); err != nil {
		t.Fatal(err)
	}
	write(t, filepath.Join(dir, "src/parser/parser.go"), "package parser\n")
	if err := os.WriteFile(filepath.Join(dir, "logo.bin"), []byte{9, 9, 0, 9}, 0644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "run.sh"), []byte("#!/bin/sh\n"), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("src/main.go", filepath.Join(dir, "main-link")); err != nil {
		t.Fatal(err)
	}
	write(t, filepath.Join(dir, "build/generated.o"), "ignored output\n")
}

func TestRevertRestoresEveryKindOfChangeAndUndoBringsThemBack(t *testing.T) {
	dir := newRepo(t)
	write(t, filepath.Join(dir, "scratch.txt"), "the person's untracked notes\n")
	write(t, filepath.Join(dir, "src/staged.go"), "package main // staged\n")
	run(t, dir, "add", "src/staged.go")
	m := manager(t, Limits{})
	repos := []Repository{{Member: "app", Path: dir}}
	if _, err := m.BeginTurn(ctx(t), "conv", repos, "op-1", "refactor"); err != nil {
		t.Fatal(err)
	}
	original := state(t, dir)
	agentTurn(t, dir)
	if err := m.EndTurn(ctx(t), "conv", repos, "", time.Now()); err != nil {
		t.Fatal(err)
	}
	afterTurn := state(t, dir)

	changes, inProgress, err := m.Changes(ctx(t), "conv", 1)
	if err != nil || inProgress || len(changes) != 1 || changes[0].Error != "" {
		t.Fatalf("changes = %+v %v %v", changes, inProgress, err)
	}
	status := map[string]string{}
	for _, f := range changes[0].Files {
		status[f.Path] = f.Status
		if f.Path == "logo.bin" && !f.Binary {
			t.Error("binary change not marked binary")
		}
		if f.Status == "R" && f.OriginalPath != "old-name.txt" {
			t.Errorf("rename source = %q", f.OriginalPath)
		}
	}
	want := map[string]string{"src/main.go": "M", "README.md": "D", "new-name.txt": "R", "src/parser/parser.go": "A", "logo.bin": "M", "run.sh": "A", "main-link": "A"}
	if !reflect.DeepEqual(status, want) {
		t.Fatalf("turn changes = %v, want %v", status, want)
	}
	rec, _ := m.List("conv")
	if rec.Turns[0].Files != len(want) || rec.Turns[0].Additions == 0 {
		t.Fatalf("turn summary = %+v", rec.Turns[0])
	}
	diff, err := m.Diff(ctx(t), "conv", 1, "app", "src/main.go", "")
	if err != nil || !strings.Contains(diff.Text, "+func main() { run() }") {
		t.Fatalf("diff = %+v %v", diff, err)
	}

	preview, err := m.Preview(ctx(t), "conv", 1, repos)
	if err != nil || len(preview) != 1 || len(preview[0].Files) != len(want) {
		t.Fatalf("preview = %+v %v", preview, err)
	}
	revert, err := m.Revert(ctx(t), "conv", 1, func() error { return nil })
	if err != nil {
		t.Fatal(err)
	}
	if len(revert.Problems) != 0 || revert.Files == 0 {
		t.Fatalf("revert = %+v", revert)
	}
	if got := read(t, filepath.Join(dir, "README.md")); got != "# Project\n" {
		t.Fatalf("deleted file not restored: %q", got)
	}
	for _, gone := range []string{"src/parser/parser.go", "src/parser", "new-name.txt", "run.sh", "main-link"} {
		if exists(filepath.Join(dir, gone)) {
			t.Errorf("%s created in the turn survived the revert", gone)
		}
	}
	if data, _ := os.ReadFile(filepath.Join(dir, "logo.bin")); !bytes.Equal(data, []byte{0, 1, 2, 3, 0xff, 0}) {
		t.Errorf("binary not restored: %v", data)
	}
	if read(t, filepath.Join(dir, "scratch.txt")) != "the person's untracked notes\n" {
		t.Error("untracked file from before the turn lost")
	}
	if !exists(filepath.Join(dir, "build/generated.o")) {
		t.Error("an ignored file was deleted")
	}
	// The index (the staged file), HEAD and branch are as they were; the
	// ignored output the turn produced is the only difference git reports.
	reverted := state(t, dir)
	original.Status = strings.TrimSpace(original.Status + "\n!! build/generated.o")
	if reverted != original {
		t.Fatalf("revert left the repository different:\nwant %+v\ngot  %+v", original, reverted)
	}
	if note := m.Note("conv"); !strings.Contains(note, "before turn 1 (“refactor”)") {
		t.Fatalf("agent note = %q", note)
	}

	undo, err := m.Undo(ctx(t), "conv", revert.ID, nil)
	if err != nil || undo.Kind != "undo" || undo.UndoOf != revert.ID {
		t.Fatalf("undo = %+v %v", undo, err)
	}
	if got := state(t, dir); got != afterTurn {
		t.Fatalf("undo did not bring the turn's files back:\nwant %+v\ngot  %+v", afterTurn, got)
	}
	if target, _ := os.Readlink(filepath.Join(dir, "main-link")); target != "src/main.go" {
		t.Errorf("symlink not restored: %q", target)
	}
	if info, err := os.Stat(filepath.Join(dir, "run.sh")); err != nil || info.Mode()&0100 == 0 {
		t.Errorf("executable bit not restored: %v", err)
	}
	if _, err = m.Undo(ctx(t), "conv", revert.ID, nil); err == nil {
		t.Fatal("undid an older revert")
	}
	// The undo is the latest revert, so it can be undone too.
	redo, err := m.Undo(ctx(t), "conv", undo.ID, nil)
	if err != nil || exists(filepath.Join(dir, "run.sh")) {
		t.Fatalf("redo = %+v %v", redo, err)
	}
	if _, err = m.Revert(ctx(t), "conv", 1, nil); !errors.Is(err, ErrNothingToRevert) {
		t.Fatalf("revert to matching files = %v", err)
	}
}

func TestRevertKeepsCommitsTheTurnMade(t *testing.T) {
	dir := newRepo(t)
	m := manager(t, Limits{})
	repos := []Repository{{Member: "app", Path: dir}}
	if _, err := m.BeginTurn(ctx(t), "conv", repos, "op-1", "commit something"); err != nil {
		t.Fatal(err)
	}
	start := run(t, dir, "rev-parse", "HEAD")
	write(t, filepath.Join(dir, "feature.go"), "package main\n")
	run(t, dir, "add", "feature.go")
	run(t, dir, "commit", "-q", "-m", "feature")
	head := run(t, dir, "rev-parse", "HEAD")
	if err := m.EndTurn(ctx(t), "conv", repos, "", time.Now()); err != nil {
		t.Fatal(err)
	}
	preview, err := m.Preview(ctx(t), "conv", 1, repos)
	if err != nil || preview[0].CommitsSince != 1 || len(preview[0].Files) != 1 || preview[0].Files[0].Status != "D" {
		t.Fatalf("preview = %+v %v", preview, err)
	}
	if _, err = m.Revert(ctx(t), "conv", 1, nil); err != nil {
		t.Fatal(err)
	}
	if got := run(t, dir, "rev-parse", "HEAD"); got != head || got == start {
		t.Fatal("revert moved the branch")
	}
	if exists(filepath.Join(dir, "feature.go")) {
		t.Fatal("file not reverted")
	}
	// The committed file stays in the index: the revert is an uncommitted deletion.
	if got := run(t, dir, "status", "--porcelain"); got != "D feature.go" && got != " D feature.go" {
		t.Fatalf("status = %q", got)
	}
}

func TestRevertIsRefusedWhileAnAgentWorks(t *testing.T) {
	dir := newRepo(t)
	m := manager(t, Limits{})
	repos := []Repository{{Member: "app", Path: dir}}
	if _, err := m.BeginTurn(ctx(t), "conv", repos, "op-1", "one"); err != nil {
		t.Fatal(err)
	}
	write(t, filepath.Join(dir, "README.md"), "changed\n")
	busy := errors.New("the agent is working")
	if _, err := m.Revert(ctx(t), "conv", 1, func() error { return busy }); !errors.Is(err, busy) {
		t.Fatalf("revert while busy = %v", err)
	}
	if read(t, filepath.Join(dir, "README.md")) != "changed\n" {
		t.Fatal("refused revert changed files")
	}
	if rec, _ := m.List("conv"); len(rec.Reverts) != 0 || rec.Note != "" {
		t.Fatalf("refused revert recorded: %+v", rec)
	}
	if _, err := m.Revert(ctx(t), "conv", 7, nil); !errors.Is(err, ErrNoCheckpoint) {
		t.Fatalf("revert to an unknown turn = %v", err)
	}
	// The turn is still running: its changes so far are what the files are now.
	changes, inProgress, err := m.Changes(ctx(t), "conv", 1)
	if err != nil || !inProgress || len(changes[0].Files) != 1 || changes[0].Files[0].Path != "README.md" {
		t.Fatalf("changes so far = %+v %v %v", changes, inProgress, err)
	}
}

func TestMultiRepositoryCheckpointIsASetAcrossRepositoriesAndWorktrees(t *testing.T) {
	api := newRepo(t)
	source := newRepo(t)
	web := filepath.Join(t.TempDir(), "web")
	run(t, source, "worktree", "add", "-q", "-b", "lazybox/web", web)
	m := manager(t, Limits{})
	repos := []Repository{{Member: "api", Path: api}, {Member: "web", Path: web}}
	turn, err := m.BeginTurn(ctx(t), "conv", repos, "op-1", "both")
	if err != nil || len(turn.Before.Repos) != 2 || !turn.Before.Repos[1].ok() {
		t.Fatalf("turn = %+v %v", turn, err)
	}
	// A linked worktree's checkpoint ref is in the repository it shares.
	if got := run(t, source, "rev-parse", RefPrefix+"conv/turn-1-before"); got != turn.Before.Repos[1].Commit {
		t.Fatal("worktree checkpoint ref not in the shared repository")
	}
	write(t, filepath.Join(api, "api.go"), "package api\n")
	write(t, filepath.Join(web, "README.md"), "web changed\n")
	if err = m.EndTurn(ctx(t), "conv", repos, "", time.Now()); err != nil {
		t.Fatal(err)
	}
	extra := newRepo(t)
	preview, err := m.Preview(ctx(t), "conv", 1, append(repos, Repository{Member: "docs", Path: extra}))
	if err != nil || len(preview) != 3 || len(preview[0].Files) != 1 || len(preview[1].Files) != 1 || preview[2].Error == "" {
		t.Fatalf("preview = %+v %v", preview, err)
	}
	if _, err = m.Revert(ctx(t), "conv", 1, nil); err != nil {
		t.Fatal(err)
	}
	if exists(filepath.Join(api, "api.go")) || read(t, filepath.Join(web, "README.md")) != "# Project\n" {
		t.Fatal("not every repository was reverted")
	}
}

func TestOldTurnsAndRevertsArePrunedWithTheirRefs(t *testing.T) {
	dir := newRepo(t)
	m := manager(t, Limits{KeepTurns: 3, KeepReverts: 1})
	repos := []Repository{{Member: "app", Path: dir}}
	for i := 1; i <= 5; i++ {
		if _, err := m.BeginTurn(ctx(t), "conv", repos, "op", "turn"); err != nil {
			t.Fatal(err)
		}
		write(t, filepath.Join(dir, "file.txt"), strings.Repeat("x", i))
		if err := m.EndTurn(ctx(t), "conv", repos, "", time.Now()); err != nil {
			t.Fatal(err)
		}
	}
	rec, _ := m.List("conv")
	if len(rec.Turns) != 3 || rec.Turns[0].Seq != 3 {
		t.Fatalf("kept turns = %+v", rec.Turns)
	}
	refs := run(t, dir, "for-each-ref", "--format=%(refname:lstrip=4)", RefPrefix+"conv/")
	if strings.Contains(refs, "turn-1-") || strings.Contains(refs, "turn-2-") || !strings.Contains(refs, "turn-3-before") || !strings.Contains(refs, "turn-5-after") {
		t.Fatalf("refs after pruning:\n%s", refs)
	}
	if _, err := m.Revert(ctx(t), "conv", 3, nil); err != nil {
		t.Fatal(err)
	}
	if _, err := m.Revert(ctx(t), "conv", 5, nil); err != nil {
		t.Fatal(err)
	}
	rec, _ = m.List("conv")
	if len(rec.Reverts) != 1 || rec.Reverts[0].ID != "r2" {
		t.Fatalf("kept reverts = %+v", rec.Reverts)
	}
	refs = run(t, dir, "for-each-ref", "--format=%(refname:lstrip=4)", RefPrefix+"conv/")
	if strings.Contains(refs, "revert-1-") || !strings.Contains(refs, "revert-2-safety") {
		t.Fatalf("revert refs after pruning:\n%s", refs)
	}
	if err := m.Delete(ctx(t), "conv"); err != nil {
		t.Fatal(err)
	}
	if refs = run(t, dir, "for-each-ref", RefPrefix); refs != "" {
		t.Fatalf("refs left after delete: %s", refs)
	}
	if rec, _ = m.List("conv"); len(rec.Turns) != 0 {
		t.Fatal("record left after delete")
	}
}

func TestLargeUntrackedFilesAreSkippedAndKeptOnRevert(t *testing.T) {
	dir := newRepo(t)
	m := manager(t, Limits{MaxFileBytes: 1024})
	repos := []Repository{{Member: "app", Path: dir}}
	write(t, filepath.Join(dir, "dataset.csv"), strings.Repeat("row\n", 1000))
	turn, err := m.BeginTurn(ctx(t), "conv", repos, "op", "one")
	if err != nil {
		t.Fatal(err)
	}
	if p := turn.Before.Repos[0]; p.SkippedCount != 1 || !strings.Contains(p.Skipped[0], "dataset.csv") {
		t.Fatalf("skipped = %+v", p)
	}
	write(t, filepath.Join(dir, "README.md"), "changed\n")
	if _, err = m.Revert(ctx(t), "conv", 1, nil); err != nil {
		t.Fatal(err)
	}
	if !exists(filepath.Join(dir, "dataset.csv")) {
		t.Fatal("a file too large to checkpoint was deleted by the revert")
	}
}

func TestRepositoryWithoutCommitsIsCheckpointed(t *testing.T) {
	dir := t.TempDir()
	run(t, dir, "init", "-q", "-b", "main")
	write(t, filepath.Join(dir, "first.txt"), "first\n")
	m := manager(t, Limits{})
	repos := []Repository{{Member: "new", Path: dir}}
	if _, err := m.BeginTurn(ctx(t), "conv", repos, "op", "start"); err != nil {
		t.Fatal(err)
	}
	write(t, filepath.Join(dir, "second.txt"), "second\n")
	if _, err := m.Revert(ctx(t), "conv", 1, nil); err != nil {
		t.Fatal(err)
	}
	if exists(filepath.Join(dir, "second.txt")) || !exists(filepath.Join(dir, "first.txt")) {
		t.Fatal("unborn repository not reverted")
	}
}

// Untracked files past the limit are counted, never held: the checkpoint has
// exactly MaxUntracked of them and says how many it left out.
func TestUntrackedFilesPastTheLimitAreCountedExactly(t *testing.T) {
	dir := newRepo(t)
	for i := range 50 {
		write(t, filepath.Join(dir, "gen", fmt.Sprintf("f%02d.txt", i)), "x\n")
	}
	w, err := snapshot(ctx(t), dir, t.TempDir(), Limits{MaxUntracked: 5}.withDefaults())
	if err != nil {
		t.Fatal(err)
	}
	if w.SkippedCount != 45 || len(w.Skipped) != skippedListed {
		t.Fatalf("skipped %d, listed %d", w.SkippedCount, len(w.Skipped))
	}
	untracked := 0
	for _, f := range strings.Split(run(t, dir, "ls-tree", "-r", "--name-only", w.Tree), "\n") {
		if strings.HasPrefix(f, "gen/") {
			untracked++
		}
	}
	if untracked != 5 {
		t.Fatalf("checkpoint holds %d untracked files, want 5", untracked)
	}
}

// A stream stopped by its reader ends Git without an error and reads nothing
// more.
func TestStreamStopsWhenTheReaderHasEnough(t *testing.T) {
	dir := newRepo(t)
	calls := 0
	err := gitCall{dir: dir}.stream(ctx(t), func(string) error {
		calls++
		return errStop
	}, "ls-files", "-z")
	if err != nil || calls != 1 {
		t.Fatalf("calls %d, err %v", calls, err)
	}
	if err = (gitCall{dir: dir}).stream(ctx(t), func(string) error { return nil }, "rev-parse", "--verify", "no-such-ref"); err == nil {
		t.Fatal("a failing command streamed without an error")
	}
}

// The change list stops one file past changeLimit and still counts lines,
// renames included, for the files it shows.
func TestChangesStopPastTheLimitAndCountRenames(t *testing.T) {
	dir := newRepo(t)
	before := run(t, dir, "write-tree")
	run(t, dir, "mv", "old-name.txt", "a-renamed.txt")
	write(t, filepath.Join(dir, "a-renamed.txt"), read(t, filepath.Join(dir, "a-renamed.txt"))+"one more line\n")
	for i := range changeLimit + 5 {
		write(t, filepath.Join(dir, "many", fmt.Sprintf("f%05d.txt", i)), "x\n")
	}
	run(t, dir, "add", "-A")
	after := run(t, dir, "write-tree")
	files, truncated, err := changes(ctx(t), dir, before, after)
	if err != nil {
		t.Fatal(err)
	}
	if !truncated || len(files) != changeLimit {
		t.Fatalf("truncated %v, %d files", truncated, len(files))
	}
	rename := files[0]
	if rename.Status != "R" || rename.OriginalPath != "old-name.txt" || rename.Path != "a-renamed.txt" || rename.Additions != 1 || rename.Deletions != 0 {
		t.Fatalf("rename = %+v", rename)
	}
	if last := files[len(files)-1]; last.Additions != 1 {
		t.Fatalf("last listed file = %+v", last)
	}
}

// The safety checkpoint skips some files (too large here; also ignored, past
// the untracked limit, or nested repositories). A revert that would write a
// path holding one of them must leave it alone: undo could not bring it back.
func TestRevertLeavesFilesTheSafetyCheckpointSkipped(t *testing.T) {
	dir := newRepo(t)
	m := manager(t, Limits{MaxFileBytes: 1024})
	repos := []Repository{{Member: "app", Path: dir}}
	notes := filepath.Join(dir, "notes.txt")
	write(t, notes, "small notes\n")
	if _, err := m.BeginTurn(ctx(t), "conv", repos, "op", "one"); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(notes); err != nil {
		t.Fatal(err)
	}
	if err := m.EndTurn(ctx(t), "conv", repos, "op", time.Now()); err != nil {
		t.Fatal(err)
	}
	large := strings.Repeat("row\n", 1000)
	write(t, notes, large)
	write(t, filepath.Join(dir, "README.md"), "changed\n")
	r, err := m.Revert(ctx(t), "conv", 1, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got := read(t, notes); got != large {
		t.Fatalf("revert overwrote a file the safety checkpoint skipped: %q", got)
	}
	if !strings.Contains(strings.Join(r.Problems, "\n"), "notes.txt: left as it is") {
		t.Fatalf("problems = %q", r.Problems)
	}
	if got := read(t, filepath.Join(dir, "README.md")); got != "# Project\n" {
		t.Fatalf("other files not reverted: %q", got)
	}
}

// A directory the target replaces with a file can still hold ignored files
// after its tracked files are removed; writing the file would delete them.
func TestRevertKeepsIgnoredFilesInADirectoryTheTargetReplaces(t *testing.T) {
	dir := newRepo(t)
	m := manager(t, Limits{})
	repos := []Repository{{Member: "app", Path: dir}}
	out := filepath.Join(dir, "out")
	write(t, out, "a file\n")
	if _, err := m.BeginTurn(ctx(t), "conv", repos, "op", "one"); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(out); err != nil {
		t.Fatal(err)
	}
	write(t, filepath.Join(out, "kept.txt"), "tracked by the checkpoint\n")
	write(t, filepath.Join(out, "run.log"), "ignored\n")
	if err := m.EndTurn(ctx(t), "conv", repos, "op", time.Now()); err != nil {
		t.Fatal(err)
	}
	r, err := m.Revert(ctx(t), "conv", 1, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got := read(t, filepath.Join(out, "run.log")); got != "ignored\n" {
		t.Fatalf("ignored file lost: %q", got)
	}
	if !strings.Contains(strings.Join(r.Problems, "\n"), "out: left as it is") {
		t.Fatalf("problems = %q", r.Problems)
	}
	if exists(filepath.Join(out, "kept.txt")) {
		t.Fatal("tracked file in the replaced directory was not removed")
	}
	if _, err = m.Undo(ctx(t), "conv", r.ID, nil); err != nil {
		t.Fatal(err)
	}
	if got := read(t, filepath.Join(out, "kept.txt")); got != "tracked by the checkpoint\n" {
		t.Fatalf("undo did not restore the directory: %q", got)
	}
}

func TestMessageReadsCommittedNoteDuringCheckpoint(t *testing.T) {
	m := manager(t, Limits{})
	if err := m.save(&Record{Conversation: "chat", Note: "Re-read the reverted files."}); err != nil {
		t.Fatal(err)
	}
	unlock, err := m.lockConversation("chat")
	if err != nil {
		t.Fatal(err)
	}
	defer unlock()
	got := make(chan string, 1)
	go func() { got <- m.Note("chat") }()
	select {
	case note := <-got:
		if note != "Re-read the reverted files." {
			t.Fatalf("note: %q", note)
		}
	case <-time.After(time.Second):
		t.Fatal("reading a committed note waited for checkpoint capture")
	}
}
