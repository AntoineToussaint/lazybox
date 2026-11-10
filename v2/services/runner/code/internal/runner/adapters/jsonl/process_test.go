package jsonl

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"
)

func TestStartupFailureRetainsBoundedRedactedDiagnostics(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	p, err := Start(ctx, "/bin/sh", []string{"-c", `printf 'startup failed with %s\n' "$FIXTURE_TOKEN" >&2; exit 1`}, []string{"FIXTURE_TOKEN=credential-value"}, t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	_, err = p.Receive(ctx)
	if err == nil || !strings.Contains(err.Error(), "startup failed") || strings.Contains(err.Error(), "credential-value") {
		t.Fatalf("startup diagnostic: %v", err)
	}
	// The bound itself is internal/redact's to test.
}

func TestCallKeepsAcknowledgementSeparateFromServerRequest(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	// A real subprocess emits a server request before the operation response.
	p, err := Start(ctx, "/bin/sh", []string{"-c", `read line; printf '%s\n' '{"id":0,"method":"pending","params":{"text":"question"}}' '{"id":"lazybox-1","result":{"accepted":true}}'; cat >/dev/null`}, nil, t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	result, err := p.Call(ctx, "start", map[string]string{"text": "hello"})
	if err != nil || !strings.Contains(string(result), "true") {
		t.Fatalf("%s %v", result, err)
	}
	request, err := p.Receive(ctx)
	if err != nil || string(request.ID) != "0" || request.Method != "pending" {
		t.Fatalf("%+v %v", request, err)
	}
	if err = p.Reply(request.ID, map[string]string{"answer": "yes"}); err != nil {
		t.Fatal(err)
	}
}

func TestCloseConfirmsExitBeforeReturning(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	p, err := Start(ctx, "/bin/sh", []string{"-c", `trap '' TERM; printf '%s\n' '{"method":"ready"}'; while :; do sleep 1; done`}, nil, t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	if _, err = p.Receive(ctx); err != nil {
		t.Fatal(err)
	}
	if err = p.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case <-p.exited:
	default:
		t.Fatal("Close returned before reaping native writer")
	}
	if p.cmd.ProcessState == nil {
		t.Fatal("missing process exit evidence")
	}
}

func TestLostGuardCannotClaimConfirmedCleanup(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	p, err := Start(ctx, "/bin/sh", []string{"-c", `printf '%s\n' '{"method":"ready"}'; cat >/dev/null`}, nil, t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	if _, err = p.Receive(ctx); err != nil {
		t.Fatal(err)
	}
	// Only the owned helper is killed. EOF on the target's stdin lets this
	// fixture exit; production must still refuse to claim cleanup evidence.
	if err = p.cmd.Process.Kill(); err != nil {
		t.Fatal(err)
	}
	if err = p.Close(); err == nil || !strings.Contains(err.Error(), "cleanup evidence") {
		t.Fatalf("lost guard appeared safely stopped: %v", err)
	}
}

func TestProtocolOutputDrainsBeforeConfirmedClose(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	p, err := Start(ctx, "/bin/sh", []string{"-c", `i=0; while [ "$i" -lt 80 ]; do printf '{"method":"event","params":%s}\n' "$i"; i=$((i+1)); done`}, nil, t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	for i := 0; i < 80; i++ {
		m, err := p.Receive(ctx)
		if err != nil || string(m.Params) != strconv.Itoa(i) {
			t.Fatalf("message %d lost while reaping helper: %+v %v", i, m, err)
		}
	}
	if err = p.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestDetachedProtocolFixture(t *testing.T) {
	path := os.Getenv("LAZYBOX_DETACHED_PROTOCOL_FIXTURE")
	if path == "" {
		return
	}
	child := exec.Command("/bin/sleep", "60")
	child.Stdout = os.Stdout
	child.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	if err := child.Start(); err != nil {
		os.Exit(91)
	}
	if err := os.WriteFile(path, []byte(strconv.Itoa(child.Process.Pid)), 0600); err != nil {
		_ = child.Process.Kill()
		os.Exit(92)
	}
	fmt.Println(`{"method":"ready"}`)
	os.Exit(0)
}

func TestEscapedProtocolDescriptorDoesNotPreventReapingOrClaimCleanExit(t *testing.T) {
	binary, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	root := t.TempDir()
	path := filepath.Join(root, "child")
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	p, err := Start(ctx, binary, []string{"-test.run=^TestDetachedProtocolFixture$"}, append(os.Environ(), "LAZYBOX_DETACHED_PROTOCOL_FIXTURE="+path), root)
	if err != nil {
		t.Fatal(err)
	}
	defer p.Close()
	if _, err = p.Receive(ctx); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	pid, err := strconv.Atoi(string(data))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = syscall.Kill(pid, syscall.SIGKILL) })
	if err = p.Close(); err == nil || !strings.Contains(err.Error(), "cleanup evidence") {
		t.Fatalf("escaped descriptor appeared clean: %v", err)
	}
	select {
	case <-p.exited:
	default:
		t.Fatal("helper was not reaped")
	}
}
