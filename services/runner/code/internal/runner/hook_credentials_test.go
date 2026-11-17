package runner

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"testing"
	"time"
)

// A stream a hook's credential opened ends when the hook's run ends
// (revokeHook), not when the hook happens to close it; a run that ended
// cannot bind a new one.
func TestHookStreamEndsWithItsRun(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir(), Address: "http://127.0.0.1:1"})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	path, err := b.grantHook("run1", []string{"read"})
	if err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var endpoint struct{ Token string }
	if err = json.Unmarshal(data, &endpoint); err != nil {
		t.Fatal(err)
	}
	run, _, err := b.HookScope(endpoint.Token)
	if err != nil || run != "run1" {
		t.Fatalf("HookScope = %q, %v", run, err)
	}
	ctx, release, err := b.BindHook(context.Background(), run)
	if err != nil {
		t.Fatal(err)
	}
	defer release()
	if ctx.Err() != nil {
		t.Fatal("bound context ended before the run did")
	}
	b.revokeHook(run)
	select {
	case <-ctx.Done():
	case <-time.After(time.Second):
		t.Fatal("the hook's stream outlived its run")
	}
	if !errors.Is(context.Cause(ctx), ErrHookCredentialEnded) {
		t.Fatalf("cause = %v", context.Cause(ctx))
	}
	if _, _, err = b.BindHook(context.Background(), run); err == nil {
		t.Fatal("an ended run bound a new stream")
	}
	if _, _, err = b.HookScope(endpoint.Token); err == nil {
		t.Fatal("an ended run's credential still authenticates")
	}
}
