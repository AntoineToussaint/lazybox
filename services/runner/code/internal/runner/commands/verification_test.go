package commands

import (
	"context"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"
)

func TestVerificationIsolation(t *testing.T) {
	outside := filepath.Join(t.TempDir(), "secret")
	if err := os.WriteFile(outside, []byte("private"), 0600); err != nil {
		t.Fatal(err)
	}
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { requests.Add(1) }))
	defer server.Close()
	m := manager(t, 1<<20)
	tests := []struct {
		name, script string
		pass         bool
	}{
		{"inside", "printf ok > result; test \"$(cat result)\" = ok", true},
		{"outside write", "printf changed > \"$1\"", false},
		{"outside read", "cat \"$1\"", false},
		{"environment", "test -z \"$LAZYBOX_SECRET\"", true},
		{"network", "curl --max-time 1 --silent " + server.URL, false},
		{"symlink", "ln -s \"$1\" link; cat link", false},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			c, err := m.Start(Spec{SessionID: "test", Kind: "verification", Action: "review/1/check/base", Cwd: t.TempDir(), Argv: []string{"sh", "-c", test.script, "sh", outside}, Env: []string{"LAZYBOX_SECRET=private"}})
			if err != nil {
				t.Fatal(err)
			}
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			state, err := c.Wait(ctx)
			output := c.Tail(ctx, 10000)
			if err != nil || state.ExitCode == nil || state.Signal != "" || (*state.ExitCode == 0) != test.pass {
				t.Fatalf("state=%+v err=%v output=%s", state, err, output)
			}
		})
	}
	if requests.Load() != 0 {
		t.Fatal("verification reached host network")
	}
	data, err := os.ReadFile(outside)
	if err != nil || string(data) != "private" {
		t.Fatal("external file changed")
	}
}

func TestCommandDeadlineIndependentOfReviewPreparation(t *testing.T) {
	m := manager(t, 1<<20)
	c, err := m.Start(Spec{SessionID: "test", Cwd: t.TempDir(), Argv: []string{"sh", "-c", "sleep 30"}, MaxDurationMs: 100})
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	state, err := c.Wait(ctx)
	if err != nil || !state.Done || (state.ExitCode != nil && *state.ExitCode == 0) {
		t.Fatalf("deadline did not cancel: %+v %v", state, err)
	}
}
