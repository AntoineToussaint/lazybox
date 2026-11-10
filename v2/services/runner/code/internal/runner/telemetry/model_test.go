package telemetry

import (
	"context"
	"path/filepath"
	"testing"
	"time"
)

func TestLatestModelExcludesChildrenAndOrdersReports(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "usage.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	ctx := context.Background()
	sc := Scope{Execution: "exec", Conversation: "conv", Profile: "account", Provider: "claude"}
	for _, r := range []Record{{ID: "old", Scope: sc, Agent: "root", Model: "old", ObservedAt: time.Now().Add(-time.Minute)}, {ID: "new", Scope: sc, Agent: "root", Model: "new", ObservedAt: time.Now()}, {ID: "child", Scope: sc, Agent: "child", Model: "child", ObservedAt: time.Now().Add(time.Minute)}} {
		if err = s.Put(ctx, r); err != nil {
			t.Fatal(err)
		}
	}
	model, err := s.LatestModel(ctx, "exec")
	if err != nil || model != "new" {
		t.Fatal(model, err)
	}
	if model, err = s.LatestModel(ctx, "absent"); err != nil || model != "" {
		t.Fatal(model, err)
	}
}
