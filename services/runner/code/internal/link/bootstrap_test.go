package link

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// localSSH is ssh to "this computer, another home": options dropped, the
// command line run by /bin/sh in that home.
const localSSH = `#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in -o) shift 2 ;; -*) shift ;; *) break ;; esac
done
shift
exec env -i HOME="$FAKE_HOME" PATH=/usr/bin:/bin /bin/sh -c "$*"
`

func localRoute(t *testing.T) (SSHRoute, string) {
	t.Helper()
	dir := t.TempDir()
	home := filepath.Join(dir, "home")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	ssh := filepath.Join(dir, "ssh")
	if err := os.WriteFile(ssh, []byte(localSSH), 0o755); err != nil {
		t.Fatal(err)
	}
	return SSHRoute{Host: "box", SSH: ssh, Env: []string{"FAKE_HOME=" + home}}, home
}

func bundle(t *testing.T, version string) Bundle {
	t.Helper()
	root := t.TempDir()
	if err := os.MkdirAll(filepath.Join(root, "bin"), 0o755); err != nil {
		t.Fatal(err)
	}
	script := "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'lazybox " + version + " (Lazybox V2)'; exit 0; fi\nif [ \"$1 $2 $3\" = 'link serve --probe' ]; then echo 'lazybox-link 1'; fi\n"
	if err := os.WriteFile(filepath.Join(root, "bin", "lazybox"), []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, "VERSION"), []byte(version+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	b, err := PackRuntime(root, t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	if b.Version != version || len(b.SHA256) != 64 {
		t.Fatalf("bundle %+v", b)
	}
	return b
}

func TestProbeInstallAndAChecksumTheHostRefuses(t *testing.T) {
	route, home := localRoute(t)
	ctx := context.Background()
	p, err := RunProbe(ctx, route)
	if err != nil {
		t.Fatal(err)
	}
	if p.OS == "" || p.Arch == "" || p.Lazybox != "" || p.Link != 0 || !strings.HasSuffix(p.DataDir, "/.local/share/lazybox/runner") {
		t.Fatalf("probe of an empty host: %+v", p)
	}
	if !strings.Contains(p.Summary(), "no Lazybox yet") {
		t.Fatalf("summary: %s", p.Summary())
	}

	b := bundle(t, "0.1.30")
	// A bundle that changed after this computer checked it: the host refuses
	// it before unpacking, and removes the upload.
	tampered := b
	tampered.SHA256 = strings.Repeat("a", 64)
	if _, err = Install(ctx, route, tampered); err == nil || !strings.Contains(err.Error(), "does not match") {
		t.Fatalf("a wrong checksum: %v", err)
	}
	if _, err := os.Stat(filepath.Join(home, ".local/share/lazybox/runner/versions/0.1.30")); err == nil {
		t.Fatal("a bundle with a wrong checksum was unpacked")
	}
	if uploads, _ := filepath.Glob(filepath.Join(home, ".cache/lazybox/upload-*")); len(uploads) != 0 {
		t.Fatalf("the refused upload stayed: %v", uploads)
	}

	path, err := Install(ctx, route, b)
	if err != nil {
		t.Fatal(err)
	}
	if path != filepath.Join(home, ".local/share/lazybox/runner/current/bin/lazybox") {
		t.Fatalf("installed at %s", path)
	}
	if p, err = RunProbe(ctx, route); err != nil || p.Lazybox != path || p.Version != "0.1.30" || p.Link != 1 {
		t.Fatalf("probe after install: %+v %v", p, err)
	}
	// An upgrade keeps the previous version for rollback; the one before goes.
	if _, err = Install(ctx, route, bundle(t, "0.1.31")); err != nil {
		t.Fatal(err)
	}
	if _, err = Install(ctx, route, bundle(t, "0.1.32")); err != nil {
		t.Fatal(err)
	}
	entries, _ := os.ReadDir(filepath.Join(home, ".local/share/lazybox/runner/versions"))
	var names []string
	for _, e := range entries {
		names = append(names, e.Name())
	}
	if strings.Join(names, ",") != "0.1.31,0.1.32" {
		t.Fatalf("versions kept: %v", names)
	}
	if target, _ := os.Readlink(filepath.Join(home, ".local/share/lazybox/runner/current")); target != "versions/0.1.32" {
		t.Fatalf("current → %s", target)
	}
	info, _ := os.Stat(filepath.Join(home, ".local/share/lazybox/runner"))
	if info.Mode().Perm() != 0o700 {
		t.Fatalf("the install directory is %v", info.Mode().Perm())
	}
}

func TestSSHConfigHosts(t *testing.T) {
	home := t.TempDir()
	ssh := filepath.Join(home, ".ssh")
	if err := os.MkdirAll(filepath.Join(ssh, "config.d"), 0o700); err != nil {
		t.Fatal(err)
	}
	config := `# mine
Include config.d/*
Host build-box bb
  HostName 10.0.0.2
  User me
Host *.corp !secret
  User corp
Host mini
  HostName=mac-mini.local
  Port 2222
Match host foo
  User nobody
Host *
  ServerAliveInterval 30
`
	if err := os.WriteFile(filepath.Join(ssh, "config"), []byte(config), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(ssh, "config.d", "orb"), []byte("Host lazybox-test-linux@orb\n  HostName 127.0.0.1\n# Host commented\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	hosts := SSHHosts(home)
	var names []string
	for _, h := range hosts {
		names = append(names, h.Name)
	}
	if strings.Join(names, ",") != "lazybox-test-linux@orb,build-box,bb,mini" {
		t.Fatalf("hosts: %v", names)
	}
	if hosts[1].HostName != "10.0.0.2" || hosts[1].User != "me" || hosts[3].HostName != "mac-mini.local" || hosts[3].Port != "2222" {
		t.Fatalf("details: %+v", hosts)
	}
	if len(SSHHosts(t.TempDir())) != 0 {
		t.Fatal("hosts without a config")
	}
}

func TestRemoteNames(t *testing.T) {
	for host, want := range map[string]string{"build-box": "build-box", "me@Build-Box.lan": "build-box.lan", "lazybox-test-linux@orb": "orb"} {
		if got := NameFor(host); got != want {
			t.Errorf("NameFor(%q) = %q, want %q", host, got, want)
		}
	}
	for _, bad := range []string{"dev", "working", "lazybox", "", "Has Space", "../x"} {
		if CheckName(bad) == nil {
			t.Errorf("%q accepted", bad)
		}
	}
	store := Remotes{Config: t.TempDir()}
	r := Remote{Name: "build-box", Route: "ssh", Host: "build-box", RunnerKey: Encode(make([]byte, 32)), KeyID: "k"}
	if err := store.Save(r); err != nil {
		t.Fatal(err)
	}
	got, err := store.Load("build-box")
	if err != nil || got.Host != "build-box" {
		t.Fatalf("load: %+v %v", got, err)
	}
	data, _ := os.ReadFile(store.Locator("build-box"))
	if !strings.Contains(string(data), `"endpoint": "`+store.Endpoint("build-box")+`"`) || !strings.Contains(string(data), `"route": "ssh"`) {
		t.Fatalf("locator: %s", data)
	}
	if err = store.Remove("build-box"); err != nil {
		t.Fatal(err)
	}
	if list, _ := store.List(); len(list) != 0 {
		t.Fatalf("after remove: %+v", list)
	}
}
