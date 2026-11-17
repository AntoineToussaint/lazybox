package workspaceruntime

import (
	"net"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestApprovalCoversOneExactVersionAcrossRestarts(t *testing.T) {
	path := filepath.Join(t.TempDir(), "approvals.json")
	a, err := OpenApprovals(path)
	if err != nil {
		t.Fatal(err)
	}
	v1, _ := Parse([]byte(example))
	if state, _ := a.Status("backend", v1); state != "required" {
		t.Fatalf("never approved: %s", state)
	}
	if err = a.Approve("backend", v1); err != nil {
		t.Fatal(err)
	}
	if state, _ := a.Status("backend", v1); state != "approved" {
		t.Fatalf("approved: %s", state)
	}
	if state, _ := a.Status("frontend", v1); state != "required" {
		t.Fatal("an approval covers one repository only")
	}
	v2, _ := Parse([]byte(strings.Replace(example, "npm ci", "npm ci && ./postinstall.sh", 1)))
	state, changed := a.Status("backend", v2)
	if state != "changed" || !reflect.DeepEqual(changed, []string{"setup"}) {
		t.Fatalf("a changed script needs approval again: %s %v", state, changed)
	}
	if err = a.Approve("backend", v2); err != nil {
		t.Fatal(err)
	}
	// Reopened (a Runner restart): both versions stay approved, so a
	// workspace on an older branch is not asked again.
	again, err := OpenApprovals(path)
	if err != nil {
		t.Fatal(err)
	}
	for _, c := range []Config{v1, v2} {
		if state, _ := again.Status("backend", c); state != "approved" {
			t.Fatalf("after a restart: %s", state)
		}
	}
	if info, err := os.Stat(path); err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("approvals must be private: %v %v", info.Mode(), err)
	}
}

func TestPortBlocksAreStableDisjointAndSkipBusyPorts(t *testing.T) {
	path := filepath.Join(t.TempDir(), "ports.json")
	p, err := OpenPorts(path)
	if err != nil {
		t.Fatal(err)
	}
	busy := map[int]bool{30013: true}
	p.InUse = func(port int) bool { return busy[port] }
	a, err := p.Block("alpha", 30000, 30049)
	if err != nil || a != 30000 {
		t.Fatalf("alpha: %d %v", a, err)
	}
	// 30010–30019 holds a port another program listens on: skipped.
	b, err := p.Block("beta", 30000, 30049)
	if err != nil || b != 30020 {
		t.Fatalf("beta must skip the busy block: %d %v", b, err)
	}
	if again, _ := p.Block("alpha", 30000, 30049); again != a {
		t.Fatal("a workspace's block must not move")
	}
	// A block stays put once allocated, even if its ports become busy or the range changes.
	busy[30001] = true
	if again, _ := p.Block("alpha", 40000, 40049); again != a {
		t.Fatal("a workspace's block moved when the range changed")
	}
	reopened, err := OpenPorts(path)
	if err != nil {
		t.Fatal(err)
	}
	reopened.InUse = p.InUse
	if got, ok := reopened.Lookup("beta"); !ok || got != b {
		t.Fatalf("blocks must survive a restart: %d %v", got, ok)
	}
	c, err := reopened.Block("gamma", 30000, 30049)
	if err != nil || c != 30030 {
		t.Fatalf("gamma: %d %v", c, err)
	}
	if _, err = reopened.Block("delta", 30000, 30039); err == nil || !strings.Contains(err.Error(), "used by other programs") {
		t.Fatalf("an exhausted range must say why: %v", err)
	}
	delete(busy, 30001)
	if err = reopened.Release("alpha"); err != nil {
		t.Fatal(err)
	}
	if d, err := reopened.Block("delta", 30000, 30039); err != nil || d != 30000 {
		t.Fatalf("a released block is reused: %d %v", d, err)
	}
	if _, err = reopened.Block("epsilon", 100, 105); err == nil {
		t.Fatal("a privileged or too small range was accepted")
	}
}

func TestPortInUseSeesAListener(t *testing.T) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := l.Addr().(*net.TCPAddr).Port
	if !PortInUse(port) {
		t.Fatal("a listening port was reported free")
	}
	l.Close()
	if PortInUse(port) {
		t.Fatal("a closed port was reported in use")
	}
}

func TestSettingsRefuseStaleAndInvalidSaves(t *testing.T) {
	path := filepath.Join(t.TempDir(), "settings.json")
	st, err := OpenSettings(path)
	if err != nil {
		t.Fatal(err)
	}
	s := st.Get()
	if s.PortStart != DefaultPortStart || s.PortEnd != DefaultPortEnd {
		t.Fatalf("defaults: %+v", s)
	}
	s.Repositories = map[string]Override{"backend": {Config: "actions:\n  web: make web\n", DefaultAction: "web"}, "empty": {}}
	saved, err := st.Save(s)
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := saved.Repositories["empty"]; ok || saved.Revision != s.Revision+1 {
		t.Fatalf("saved: %+v", saved)
	}
	if _, err = st.Save(s); err != ErrStaleSettings {
		t.Fatalf("a stale save must be refused: %v", err)
	}
	saved.Repositories["backend"] = Override{Config: "actoins: {}\n"}
	if _, err = st.Save(saved); err == nil || !strings.Contains(err.Error(), "unknown key") {
		t.Fatalf("an invalid override must be refused with its error: %v", err)
	}
	reopened, _ := OpenSettings(path)
	if reopened.Override("backend").DefaultAction != "web" {
		t.Fatal("settings must survive a restart")
	}
}

func TestCopyFilesStaysInsideBothTreesAndNeverOverwrites(t *testing.T) {
	source, worktree, outside := t.TempDir(), t.TempDir(), t.TempDir()
	write := func(root, name, content string) {
		t.Helper()
		if err := os.MkdirAll(filepath.Dir(filepath.Join(root, name)), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(root, name), []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	write(source, ".env", "SECRET=1")
	write(source, "config/a.local.json", "{}")
	write(source, "config/b.local.json", "{}")
	write(source, "config/shared.json", "{}")
	write(source, ".env.local", "mine")
	write(worktree, ".env.local", "already here")
	write(outside, "secret", "nope")
	if err := os.Symlink(filepath.Join(outside, "secret"), filepath.Join(source, ".npmrc")); err != nil {
		t.Fatal(err)
	}
	res, err := CopyFiles(source, worktree, []string{".env", "config/*.local.json", ".env.local", ".env.production", ".npmrc"})
	if err == nil || !strings.Contains(err.Error(), ".npmrc is not a regular file") {
		t.Fatalf("a symbolic link must not be followed: %v", err)
	}
	if !reflect.DeepEqual(res.Copied, []string{".env", "config/a.local.json", "config/b.local.json"}) || !reflect.DeepEqual(res.Kept, []string{".env.local"}) || !reflect.DeepEqual(res.Missing, []string{".env.production"}) {
		t.Fatalf("copy: %+v", res)
	}
	if data, _ := os.ReadFile(filepath.Join(worktree, ".env.local")); string(data) != "already here" {
		t.Fatal("an existing file was overwritten")
	}
	if info, err := os.Stat(filepath.Join(worktree, ".env")); err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("a copy keeps its permissions: %v %v", info, err)
	}
	if _, err := os.Stat(filepath.Join(worktree, ".npmrc")); err == nil {
		t.Fatal("a file outside the source checkout was copied through a link")
	}
	if _, err := os.Stat(filepath.Join(worktree, "config/shared.json")); err == nil {
		t.Fatal("a file the pattern does not match was copied")
	}
}
