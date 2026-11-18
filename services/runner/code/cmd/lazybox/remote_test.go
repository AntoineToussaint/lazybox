package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/installation"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/link"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
)

// fakeSSH stands in for /usr/bin/ssh when this Mac has no Remote Login: it
// drops ssh's options and runs the remote command line with /bin/sh as the
// host's login shell, in the host's own home and environment, with stdin and
// stdout passed through. Everything Lazybox does over SSH (probe, upload,
// install, link up, owners add, link serve --stdio) runs exactly as it would
// over a real ssh; only the transport and sshd's authentication are not real.
//
// Like sshd, it keeps the command's stdin and stdout open itself until the
// command exits (no exec), so a command that closes its stdout is not seen as
// gone until it really ends: a host side that lingers in a read shows here.
const fakeSSH = `#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    -o|-S|-p|-i|-l|-F|-J|-E) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
host=$1; shift
echo "$host $*" >> "$FAKE_SSH_LOG"
if [ -n "$FAKE_SSH_REFUSE" ]; then echo "ssh: connect to host $host port 22: Connection refused" >&2; exit 255; fi
env -i HOME="$FAKE_SSH_HOME" PATH=/usr/bin:/bin:/usr/sbin:/sbin USER="$USER" TMPDIR="$TMPDIR" SSH_CONNECTION="127.0.0.1 50022 127.0.0.1 22" /bin/sh -c "$*; exit \$?"
exit $?
`

type remoteWorld struct {
	t                      *testing.T
	bin                    string // this build's lazybox and lazybox-runner
	client, host           string // two homes: this Mac, and the host's
	ssh, log               string
	bundle                 string
	hostRunner             int
	connect                *exec.Cmd
	connectLog             *os.File
	extraHostEnv, extraEnv []string
}

func buildBinaries(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	for target, pkg := range map[string]string{"lazybox": ".", "lazybox-runner": "../.."} {
		cmd := exec.Command("go", "build", "-o", filepath.Join(dir, target), pkg)
		cmd.Env = append(os.Environ(), "CGO_ENABLED=0")
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("building %s: %v\n%s", target, err, out)
		}
	}
	return dir
}

func newRemoteWorld(t *testing.T) *remoteWorld {
	t.Helper()
	if testing.Short() {
		t.Skip("builds the Runner and runs it: not in -short")
	}
	w := &remoteWorld{t: t, bin: buildBinaries(t)}
	base, err := os.MkdirTemp("", "lbx-remote-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(base) })
	w.client, w.host = filepath.Join(base, "mac"), filepath.Join(base, "box")
	for _, d := range []string{w.client, w.host} {
		if err := os.MkdirAll(d, 0o700); err != nil {
			t.Fatal(err)
		}
	}
	w.ssh, w.log = filepath.Join(base, "ssh"), filepath.Join(base, "ssh.log")
	if err := os.WriteFile(w.ssh, []byte(fakeSSH), 0o755); err != nil {
		t.Fatal(err)
	}
	// The bundle this Mac uploads: a runtime with the CLI and the Runner.
	runtimeDir := filepath.Join(base, "runtime")
	for _, f := range []string{"lazybox", "lazybox-runner"} {
		data, err := os.ReadFile(filepath.Join(w.bin, f))
		if err != nil {
			t.Fatal(err)
		}
		if err = os.MkdirAll(filepath.Join(runtimeDir, "bin"), 0o755); err != nil {
			t.Fatal(err)
		}
		if err = os.WriteFile(filepath.Join(runtimeDir, "bin", f), data, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(runtimeDir, "VERSION"), []byte("0.0.0-e2e\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	b, err := link.PackRuntime(runtimeDir, base)
	if err != nil {
		t.Fatal(err)
	}
	w.bundle = b.Path
	t.Cleanup(w.stop)
	return w
}

func (w *remoteWorld) env() []string {
	return append([]string{"HOME=" + w.client, "XDG_CONFIG_HOME=" + filepath.Join(w.client, ".config"), "PATH=/usr/bin:/bin", "TMPDIR=" + os.TempDir(), "USER=" + os.Getenv("USER"),
		"LAZYBOX_SSH=" + w.ssh, "FAKE_SSH_HOME=" + w.host, "FAKE_SSH_LOG=" + w.log}, w.extraEnv...)
}

// lazybox runs this Mac's lazybox.
func (w *remoteWorld) lazybox(args ...string) (string, error) {
	cmd := exec.Command(filepath.Join(w.bin, "lazybox"), args...)
	cmd.Env = w.env()
	out, err := cmd.CombinedOutput()
	return string(out), err
}

// onHost runs the host's installed lazybox, as a person logged in there.
func (w *remoteWorld) onHost(args ...string) (string, error) {
	cmd := exec.Command(filepath.Join(w.host, ".local/share/lazybox/runner/current/bin/lazybox"), args...)
	cmd.Env = append([]string{"HOME=" + w.host, "PATH=/usr/bin:/bin", "TMPDIR=" + os.TempDir()}, w.extraHostEnv...)
	out, err := cmd.CombinedOutput()
	return string(out), err
}

func (w *remoteWorld) startConnect(name string) {
	w.t.Helper()
	w.connect = exec.Command(filepath.Join(w.bin, "lazybox"), "link", "connect", name, "--json")
	w.connect.Env = w.env()
	var err error
	if w.connectLog, err = os.Create(filepath.Join(w.client, "connect.log")); err != nil {
		w.t.Fatal(err)
	}
	w.connect.Stdout, w.connect.Stderr = w.connectLog, w.connectLog
	if err = w.connect.Start(); err != nil {
		w.t.Fatal(err)
	}
}

// stopConnect ends the link agent (the host's Runner keeps running).
func (w *remoteWorld) stopConnect() {
	if w.connect != nil && w.connect.Process != nil {
		_ = w.connect.Process.Signal(syscall.SIGTERM)
		done := make(chan struct{})
		go func() { _ = w.connect.Wait(); close(done) }()
		select {
		case <-done:
		case <-time.After(5 * time.Second):
			_ = w.connect.Process.Kill()
		}
	}
	w.connect = nil
}

// hostState is the host Runner's state directory: this platform's layout
// (installation.UserPaths) under the host's home.
func (w *remoteWorld) hostState() string {
	paths, err := installation.UserPaths(func(k string) string {
		if k == "HOME" {
			return w.host
		}
		return ""
	})
	if err != nil {
		panic(err)
	}
	return paths.State
}

// stop ends the link agent and the host's Runner, whichever `link up` started.
func (w *remoteWorld) stop() {
	w.stopConnect()
	if data, err := os.ReadFile(filepath.Join(w.hostState(), "runner.pid")); err == nil {
		w.hostRunner, _ = strconv.Atoi(strings.TrimSpace(string(data)))
	}
	if w.hostRunner > 0 {
		_ = syscall.Kill(w.hostRunner, syscall.SIGTERM)
		for i := 0; i < 100 && syscall.Kill(w.hostRunner, 0) == nil; i++ {
			time.Sleep(50 * time.Millisecond)
		}
	}
}

func (w *remoteWorld) status(name string) link.LinkStatus {
	data, _ := os.ReadFile(filepath.Join(w.client, ".config/lazybox/remotes", name, "status.json"))
	var st link.LinkStatus
	_ = json.Unmarshal(data, &st)
	return st
}

func (w *remoteWorld) waitState(name, state string, within time.Duration) link.LinkStatus {
	w.t.Helper()
	deadline := time.Now().Add(within)
	for {
		st := w.status(name)
		if st.State == state {
			return st
		}
		if time.Now().After(deadline) {
			log, _ := os.ReadFile(filepath.Join(w.client, "connect.log"))
			w.t.Fatalf("%s never became %q (last %+v)\n%s", name, state, st, log)
		}
		time.Sleep(20 * time.Millisecond)
	}
}

// waitReconnected waits until the link is connected by a connection made
// after since: its state changed (to reconnecting and back) since then.
func (w *remoteWorld) waitReconnected(name string, since time.Time, within time.Duration) link.LinkStatus {
	w.t.Helper()
	deadline := time.Now().Add(within)
	for {
		st := w.status(name)
		if st.State == "connected" && st.Since.After(since) {
			return st
		}
		if time.Now().After(deadline) {
			log, _ := os.ReadFile(filepath.Join(w.client, "connect.log"))
			w.t.Fatalf("%s did not reconnect after %s (last %+v)\n%s", name, since.Format(time.RFC3339Nano), st, log)
		}
		time.Sleep(20 * time.Millisecond)
	}
}

// sshPID is the live link's ssh: the connect process's child, the leader of
// its own process group (the fake ssh and the host's `link serve` under it).
func (w *remoteWorld) sshPID() int {
	w.t.Helper()
	out, err := exec.Command("/usr/bin/pgrep", "-P", strconv.Itoa(w.connect.Process.Pid)).Output()
	if err != nil {
		w.t.Fatalf("no ssh under the link agent: %v", err)
	}
	pid, _ := strconv.Atoi(strings.Fields(string(out))[0])
	return pid
}

// The whole owner link, end to end over (fake) SSH: Add a computer probes the
// host, uploads and checks the Runner, starts it, enrols this Mac and pins the
// host's key; the link agent then serves that Runner on loopback, the CLI
// works through it, a dropped ssh reconnects within seconds, a revoke on the
// host ends it at once, another Runner behind the same name is refused, and
// removing the computer revokes this Mac's key there. The owner bearer never
// reaches this Mac.
func TestRemoteRunnerOverSSHEndToEnd(t *testing.T) {
	w := newRemoteWorld(t)

	// An unreachable host is said plainly, and nothing is written.
	w.extraEnv = []string{"FAKE_SSH_REFUSE=1"}
	out, err := w.lazybox("remote", "add", "build-box", "--bundle", w.bundle, "--progress", "json")
	if err == nil || !strings.Contains(out, "refused the SSH connection") {
		t.Fatalf("unreachable host: %v\n%s", err, out)
	}
	w.extraEnv = nil
	if _, err := os.Stat(filepath.Join(w.client, ".config/lazybox/remotes/build-box")); err == nil {
		t.Fatal("a failed add left a record")
	}

	// A bundle whose SHA-256 is not the one expected is refused before upload.
	if out, err = w.lazybox("remote", "add", "build-box", "--bundle", w.bundle, "--sha256", strings.Repeat("0", 64)); err == nil || !strings.Contains(out, "SHA-256") {
		t.Fatalf("wrong checksum: %v\n%s", err, out)
	}

	out, err = w.lazybox("remote", "add", "build-box", "--bundle", w.bundle, "--service", "none", "--progress", "json")
	if err != nil {
		t.Fatalf("remote add: %v\n%s", err, out)
	}
	steps := map[string]string{}
	var final struct {
		Done   bool        `json:"done"`
		Remote link.Remote `json:"remote"`
	}
	scanner := bufio.NewScanner(strings.NewReader(out))
	for scanner.Scan() {
		var ev progressEvent
		if json.Unmarshal(scanner.Bytes(), &ev) == nil && ev.Step != "" {
			steps[ev.Step] = ev.State
		}
		_ = json.Unmarshal(scanner.Bytes(), &final)
	}
	for _, step := range []string{"connect", "probe", "install", "start", "enrol", "connected"} {
		if steps[step] != "done" {
			t.Fatalf("step %s: %q\n%s", step, steps[step], out)
		}
	}
	if !final.Done || final.Remote.Name != "build-box" {
		t.Fatalf("final: %+v\n%s", final, out)
	}
	var up struct{ PID int }
	if data, err := os.ReadFile(filepath.Join(w.hostState(), "runner.pid")); err == nil {
		up.PID, _ = strconv.Atoi(strings.TrimSpace(string(data)))
	}
	if up.PID == 0 {
		t.Fatalf("the host Runner left no pid\n%s", out)
	}
	w.hostRunner = up.PID

	// The host recorded this Mac's key; this Mac pinned the host Runner's key.
	owners, err := w.onHost("link", "owners", "ls", "--json")
	if err != nil {
		t.Fatalf("owners ls on the host: %v\n%s", err, owners)
	}
	var listed struct {
		RunnerKey string       `json:"runnerKey"`
		Owners    []link.Owner `json:"owners"`
	}
	if err = json.Unmarshal([]byte(owners), &listed); err != nil || len(listed.Owners) != 1 || listed.Owners[0].AddedVia != "ssh" {
		t.Fatalf("host owners: %v %s", err, owners)
	}
	if final.Remote.RunnerKey != listed.RunnerKey || final.Remote.KeyID != listed.Owners[0].KeyID {
		t.Fatalf("pinned %s/%s, host has %s/%s", final.Remote.RunnerKey, final.Remote.KeyID, listed.RunnerKey, listed.Owners[0].KeyID)
	}
	// Installed under versions/<v> with `current` pointing at it; the upload is gone.
	if target, err := os.Readlink(filepath.Join(w.host, ".local/share/lazybox/runner/current")); err != nil || target != "versions/0.0.0-e2e" {
		t.Fatalf("current: %q %v", target, err)
	}
	if uploads, _ := filepath.Glob(filepath.Join(w.host, ".cache/lazybox/upload-*")); len(uploads) != 0 {
		t.Fatalf("uploads left: %v", uploads)
	}

	// `remote ls` and `remote hosts`.
	if out, err = w.lazybox("remote", "ls", "--json"); err != nil || !strings.Contains(out, `"name": "build-box"`) {
		t.Fatalf("remote ls: %v\n%s", err, out)
	}

	// The link agent: the host's Runner on a loopback origin, through which the CLI works.
	w.startConnect("build-box")
	st := w.waitState("build-box", "connected", 15*time.Second)
	// On localhost a round trip can measure 0 ms; a negative one means none was taken.
	if st.RTTMillis < 0 || st.Machine == "" {
		t.Fatalf("status %+v", st)
	}
	if out, err = w.lazybox("--runner", "build-box", "ls"); err != nil || !strings.Contains(out, "No workspaces") {
		t.Fatalf("lazybox --runner build-box ls: %v\n%s", err, out)
	}
	if out, err = w.lazybox("runners"); err != nil || !strings.Contains(out, "build-box") || !strings.Contains(out, "running") {
		t.Fatalf("runners: %v\n%s", err, out)
	}
	endpoint, err := loadTarget("build-box", filepath.Join(w.client, ".config/lazybox/remotes/build-box/endpoint.json"), false)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(endpoint.URL, "http://127.0.0.1:") {
		t.Fatalf("the link agent serves %s", endpoint.URL)
	}
	rpc := func() error {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_, err := dial(endpoint).Version(ctx, connect.NewRequest(&pb.VersionRequest{}))
		return err
	}
	if err = rpc(); err != nil {
		t.Fatal(err)
	}

	// The owner bearer never reached this Mac: no file here holds it.
	hostEndpoint, err := link.ReadEndpoint(filepath.Join(w.hostState(), "endpoint.json"))
	if err != nil {
		t.Fatal(err)
	}
	_ = filepath.WalkDir(w.client, func(path string, d fs.DirEntry, err error) error {
		if err == nil && d.Type().IsRegular() {
			if data, _ := os.ReadFile(path); strings.Contains(string(data), hostEndpoint.Token) {
				t.Errorf("%s holds the host's owner bearer", path)
			}
		}
		return nil
	})
	// The bearer is not a way in through the link either: the host refuses it.
	bad := genconnect.NewRunnerServiceClient(newHTTPClient(hostEndpoint.Token), endpoint.URL)
	if _, err = bad.Version(context.Background(), connect.NewRequest(&pb.VersionRequest{})); connect.CodeOf(err) != connect.CodeUnauthenticated {
		t.Fatalf("the host's bearer at the link's origin: %v", err)
	}

	// ssh dies (sleep, a network change): back within 5 seconds.
	killed := time.Now()
	if err = syscall.Kill(-w.sshPID(), syscall.SIGKILL); err != nil {
		t.Fatal(err)
	}
	// Connected again: a connection made after the kill. The reconnecting
	// state in between can be shorter than a poll of status.json (on Linux it
	// lasts a few milliseconds), so its Since is the evidence, not seeing it.
	w.waitReconnected("build-box", killed, 5*time.Second)
	if err = rpc(); err != nil {
		t.Fatalf("after reconnecting: %v", err)
	}
	t.Logf("reconnected %s after ssh was killed", time.Since(killed).Round(time.Millisecond))

	// Revoked on the host: the link ends within a second and says why.
	revoked := time.Now()
	if out, err = w.onHost("link", "owners", "revoke", final.Remote.KeyID); err != nil {
		t.Fatalf("revoke on the host: %v\n%s", err, out)
	}
	st = w.waitState("build-box", "refused", 3*time.Second)
	if st.Code != "unknown-key" || !strings.Contains(st.Problem, "no longer accepts") {
		t.Fatalf("after revoke: %+v", st)
	}
	t.Logf("refused %s after the revoke", time.Since(revoked).Round(time.Millisecond))
	if err = rpc(); connect.CodeOf(err) != connect.CodeUnauthenticated {
		t.Fatalf("a call after the revoke: %v", err)
	}
	w.stopConnect()

	// Adding it again enrols the same key again: the Runner is installed and
	// running, and the agents it runs were never touched.
	if out, err = w.lazybox("remote", "add", "build-box", "--progress", "json"); err != nil || !strings.Contains(out, `"step":"install","title":"Install the Runner","state":"skipped"`) || !strings.Contains(out, "already running") {
		t.Fatalf("add again: %v\n%s", err, out)
	}

	// Another Runner behind the same name (the alias now points elsewhere, or
	// the host was replaced): refused before any request.
	keyFile := filepath.Join(w.hostState(), link.RunnerKeyFile)
	if err = os.Rename(keyFile, keyFile+".real"); err != nil {
		t.Fatal(err)
	}
	w.startConnect("build-box")
	st = w.waitState("build-box", "refused", 10*time.Second)
	if st.Code != "wrong-runner" || !strings.Contains(st.Problem, "This is not build-box's Runner") {
		t.Fatalf("another Runner: %+v", st)
	}
	w.stopConnect()
	_ = os.Remove(keyFile)
	if err = os.Rename(keyFile+".real", keyFile); err != nil {
		t.Fatal(err)
	}

	// Remove: this Mac's key is revoked on the host, and everything here goes.
	if out, err = w.lazybox("remote", "rm", "build-box", "--json"); err != nil || !strings.Contains(out, `"revoked": true`) {
		t.Fatalf("remote rm: %v\n%s", err, out)
	}
	if owners, _ = w.onHost("link", "owners", "ls", "--json"); !strings.Contains(owners, `"owners": []`) {
		t.Fatalf("host owners after rm: %s", owners)
	}
	for _, gone := range []string{".config/lazybox/remotes/build-box", ".config/lazybox/runners/build-box.json"} {
		if _, err := os.Stat(filepath.Join(w.client, gone)); err == nil {
			t.Fatalf("%s is still there", gone)
		}
	}
	// What ran over ssh, for the record.
	if log, err := os.ReadFile(w.log); err == nil {
		t.Logf("ssh commands:\n%s", log)
	}
	fmt.Fprint(os.Stderr)
}

// The link endpoint never listens: without --stdio it refuses, and from
// inside Lazybox (an agent's environment) it refuses too.
func TestLinkServeIsStdioOnlyAndNotForAgents(t *testing.T) {
	r := newTestRunner(t)
	env := map[string]string{}
	if out := r.cli(env, "", "link", "serve"); out.code != exitUsage || out.err == nil || !strings.Contains(out.err.Error(), "opens no port") {
		t.Fatalf("link serve without --stdio: %+v", out)
	}
	if out := r.cli(map[string]string{"LAZYBOX_V2_SESSION_ID": "s1"}, "", "link", "owners", "ls"); out.code != exitDenied {
		t.Fatalf("owners from an agent: %+v", out)
	}
	if out := r.cli(env, "", "link", "serve", "--probe"); out.code != 0 || strings.TrimSpace(out.stdout) != "lazybox-link 1" {
		t.Fatalf("probe: %+v", out)
	}
}
