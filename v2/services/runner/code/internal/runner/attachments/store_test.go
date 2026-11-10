package attachments

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
)

func TestGenericFilesArePrivateRetainedAndIndependentOfClientPaths(t *testing.T) {
	if _, err := New("relative"); err == nil {
		t.Fatal("relative storage root accepted")
	}
	root := t.TempDir()
	store, err := New(root)
	if err != nil {
		t.Fatal(err)
	}
	inputs := []Input{{Name: "notes.txt", Data: []byte("context")}, {Name: "report.pdf", MediaType: "application/pdf", Data: []byte("%PDF-1.7\nfixture")}, {Name: "binary.dat", Data: []byte{0, 255, 1}}, {Name: "empty", Data: []byte{}}}
	files, err := store.Put("execution", inputs)
	if err != nil {
		t.Fatal(err)
	}
	for i, file := range files {
		data, err := os.ReadFile(file.Path)
		if err != nil {
			t.Fatal(err)
		}
		if !bytes.Equal(data, inputs[i].Data) || file.Name != inputs[i].Name || file.Size != int64(len(data)) || !strings.HasPrefix(file.Path, filepath.Join(root, "execution")+string(os.PathSeparator)) {
			t.Fatalf("invalid retained attachment: %+v", file)
		}
		info, err := os.Stat(file.Path)
		if err != nil || info.Mode().Perm() != 0600 {
			t.Fatalf("private mode missing: %v", err)
		}
	}
	if !strings.HasPrefix(files[0].MediaType, "text/plain") || files[1].MediaType != "application/pdf" {
		t.Fatal("media metadata lost")
	}
	retry, err := store.Put("execution", inputs)
	if err != nil || retry[0].Path != files[0].Path {
		t.Fatalf("retry did not reuse retained content: %v", err)
	}
	other, err := store.Put("sibling", inputs)
	if err != nil || other[0].Path == files[0].Path {
		t.Fatalf("executions share attachment paths: %v", err)
	}
	changed, err := store.Put("execution", []Input{{Name: "notes.txt", Data: []byte("changed")}})
	if err != nil || changed[0].Path == files[0].Path {
		t.Fatalf("changed bytes overwrote old attachment: %v", err)
	}
	sameNames, err := store.Put("execution", []Input{{Name: "same.txt", Data: []byte("one")}, {Name: "same.txt", Data: []byte("two")}})
	if err != nil || len(sameNames) != 2 || sameNames[0].Path == sameNames[1].Path {
		t.Fatalf("same-named files lost distinct bytes: %v", err)
	}
	var wg sync.WaitGroup
	for range 8 {
		wg.Go(func() {
			if _, err := store.Put("execution", inputs); err != nil {
				t.Errorf("concurrent retry: %v", err)
			}
		})
	}
	wg.Wait()
}

func TestBatchValidationAndRetentionBudgets(t *testing.T) {
	store, err := New(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"", ".", "..", "../escape", "a/b", "a\\b", "bad\nname", strings.Repeat("x", 256)} {
		if _, err := store.Put("execution", []Input{{Name: "valid", Data: []byte("first")}, {Name: name}}); err == nil {
			t.Fatalf("unsafe name accepted: %q", name)
		}
	}
	if _, err := store.Put("../escape", nil); err == nil {
		t.Fatal("unsafe execution identity")
	}
	if _, err := store.Put("execution", make([]Input, MaxFiles+1)); err == nil {
		t.Fatal("unbounded file count")
	}
	if _, err := store.Put("execution", []Input{{Name: "huge", Data: make([]byte, MaxFileBytes+1)}}); err == nil {
		t.Fatal("unbounded file size")
	}
	data := make([]byte, MaxMessageBytes/2+1)
	if _, err := store.Put("execution", []Input{{Name: "a", Data: data}, {Name: "b", Data: data}}); err == nil {
		t.Fatal("unbounded message size")
	}
	entries, err := os.ReadDir(store.root)
	if err != nil || len(entries) != 0 {
		t.Fatal("invalid batch left partial files")
	}
	// Model the retained inventory without writing 64 MiB for this admission test.
	store.sessions["full"] = map[string]int64{"prior": SessionBudget}
	if _, err := store.Put("full", []Input{{Name: "one-more", Data: []byte{1}}}); err == nil {
		t.Fatal("retention budget ignored")
	}
	store.sessions["many"] = map[string]int64{}
	for i := range SessionFiles {
		store.sessions["many"][string(rune(i))] = 0
	}
	if _, err := store.Put("many", []Input{{Name: "empty"}}); err == nil {
		t.Fatal("zero-byte files bypassed count budget")
	}
}

func TestWriteFailureRollsBackTheBatchAndDoesNotSpendQuota(t *testing.T) {
	root := t.TempDir()
	store, err := New(root)
	if err != nil {
		t.Fatal(err)
	}
	first := []byte("first")
	second := []byte("second")
	pathFor := func(data []byte, name string) string {
		digest := sha256.Sum256(data)
		return filepath.Join(root, "execution", hex.EncodeToString(digest[:]), name)
	}
	blocked := filepath.Dir(pathFor(second, "b"))
	if err := os.MkdirAll(filepath.Dir(blocked), 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(blocked, []byte("not a directory"), 0600); err != nil {
		t.Fatal(err)
	}
	inputs := []Input{{Name: "a", Data: first}, {Name: "b", Data: second}}
	if _, err := store.Put("execution", inputs); err == nil {
		t.Fatal("write failure hidden")
	}
	if _, err := os.Stat(pathFor(first, "a")); !os.IsNotExist(err) {
		t.Fatal("partial batch retained")
	}
	if len(store.sessions["execution"]) != 0 {
		t.Fatal("failed batch spent quota")
	}
	if err := os.Remove(blocked); err != nil {
		t.Fatal(err)
	}
	if _, err := store.Put("execution", inputs); err != nil {
		t.Fatalf("clean retry failed: %v", err)
	}
}
