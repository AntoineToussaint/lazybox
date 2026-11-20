package codex

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func TestCodexProgramKeepsPackagedHostBesideExecutable(t *testing.T) {
	root := t.TempDir()
	pkg, bin := filepath.Join(root, "package"), filepath.Join(root, "bin")
	for _, dir := range []string{pkg, bin} {
		if err := os.Mkdir(dir, 0755); err != nil {
			t.Fatal(err)
		}
	}
	entry, link := filepath.Join(pkg, "codex"), filepath.Join(bin, "codex")
	helper := filepath.Join(pkg, "codex-code-mode-host")
	if err := os.WriteFile(entry, []byte("#!/bin/sh\nexec \"$(dirname \"$0\")/codex-code-mode-host\"\n"), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(entry, link); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", bin+string(os.PathListSeparator)+os.Getenv("PATH"))
	if got := codexProgram(""); got != link {
		t.Fatalf("without a packaged helper, wanted %q, got %q", link, got)
	}
	if err := os.WriteFile(helper, []byte("#!/bin/sh\necho ready\n"), 0755); err != nil {
		t.Fatal(err)
	}
	if out, err := exec.Command(link).CombinedOutput(); err == nil {
		t.Fatalf("the symlink should fail to locate its companion: %s", out)
	}
	if out, err := exec.Command(codexProgram("")).CombinedOutput(); err != nil || strings.TrimSpace(string(out)) != "ready" {
		t.Fatalf("could not launch the companion: %s (%v)", out, err)
	}
	if err := os.Symlink(helper, filepath.Join(bin, filepath.Base(helper))); err != nil {
		t.Fatal(err)
	}
	if got := codexProgram(""); got != link {
		t.Fatalf("a complete public bin directory should be kept, got %q", got)
	}
}
