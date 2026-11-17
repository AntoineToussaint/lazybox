package claude

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// modelWorker acknowledges only a model switch to "claude-opus-5-5".
func modelWorker(features string) string {
	return `read -r line
printf '%s\n' '{"id":"lazybox-1","result":{"reference":"ref","features":` + features + `}}'
while IFS= read -r line; do
  case "$line" in
    *'"method":"model"'*'"model":"claude-opus-5-5"'*) printf '%s\n' '{"id":"lazybox-2","result":{"delivery":"acknowledged"}}' ;;
    *) printf '%s\n' '{"id":"lazybox-2","error":{"message":"unexpected"}}' ;;
  esac
done
`
}

// A worker that reports the model feature can switch model live, and the
// switch reaches it as its own operation; an older worker has no live switch,
// so the Runner offers a restart instead.
func TestLiveModelFollowsTheWorkersFeatures(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	for _, tc := range []struct {
		features string
		live     bool
	}{{`["permission","context","stop_agent","model"]`, true}, {`["permission","context","stop_agent"]`, false}} {
		worker := filepath.Join(t.TempDir(), "worker.sh")
		if err := os.WriteFile(worker, []byte(modelWorker(tc.features)), 0700); err != nil {
			t.Fatal(err)
		}
		session, err := Adapter{Node: "/bin/sh", Worker: worker}.Open(ctx, agent.Open{Cwd: t.TempDir()})
		if err != nil {
			t.Fatal(err)
		}
		if session.Capabilities().LiveModel != tc.live {
			t.Fatalf("features %s: LiveModel %v", tc.features, !tc.live)
		}
		if tc.live {
			r, err := session.Apply(ctx, agent.Operation{ID: "switch", Kind: agent.Model, Model: "claude-opus-5-5"})
			if err != nil || r.Delivery != "acknowledged" || r.ID != "switch" {
				t.Fatalf("model switch not acknowledged: %+v %v", r, err)
			}
		}
		_ = session.Close()
	}
}
