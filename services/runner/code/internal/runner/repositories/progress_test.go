package repositories

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestProgressSanitizesAndHandlesChunks(t *testing.T) {
	var rows []string
	w := &progressWriter{emit: func(s string) error { rows = append(rows, s); return nil }, cancel: func() {}}
	for _, s := range []string{"Receiving obj", "ects:  42% (42/100), token-secret\r", "Receiving objects:  42% (42/100)\r", "fatal: secret-token\n", strings.Repeat("x", 5000) + "Receiving objects: 99% (99/100)\n", "Resolving deltas: 100% (2/2), done.\n"} {
		if _, e := w.Write([]byte(s)); e != nil {
			t.Fatal(e)
		}
	}
	if len(rows) != 2 || rows[0] != "Receiving objects: 42% (42/100)" || rows[1] != "Resolving deltas: 100% (2/2)" {
		t.Fatal(rows)
	}
}
func TestProgressFailureCancelsClone(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	w := &progressWriter{emit: func(string) error { return errors.New("reader gone") }, cancel: cancel}
	_, err := w.Write([]byte("Receiving objects: 1% (1/100)\r"))
	if err == nil || ctx.Err() == nil {
		t.Fatal("failed reader did not cancel clone")
	}
}

func TestGitCloneReportsRealStages(t *testing.T) {
	source := t.TempDir()
	gitTest(t, source, "init", "-b", "main")
	if err := os.WriteFile(filepath.Join(source, "data.txt"), []byte(strings.Repeat("fixture data\n", 2000)), 0600); err != nil {
		t.Fatal(err)
	}
	gitTest(t, source, "add", "data.txt")
	gitTest(t, source, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "commit", "-m", "fixture")
	var stages []string
	err := (Git{}).CloneWithProgress(context.Background(), "file://"+source, filepath.Join(t.TempDir(), "clone.git"), "fixture-secret", func(stage string) error { stages = append(stages, stage); return nil })
	if err != nil || len(stages) == 0 {
		t.Fatal(stages, err)
	}
	for _, stage := range stages {
		if strings.Contains(stage, "fixture-secret") {
			t.Fatal("secret in progress")
		}
	}
}
