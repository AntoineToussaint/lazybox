package preferences

import (
	"errors"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/telemetry"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestSettingsPersistWithoutMutatingBaseAndRejectLostUpdates(t *testing.T) {
	path := filepath.Join(t.TempDir(), "private", "settings.json")
	base := telemetry.DefaultRates()
	s, err := Open(path, base)
	if err != nil {
		t.Fatal(err)
	}
	original := s.Snapshot()
	if original.Permission != "" || original.Context != DefaultContext || original.Naming != "repositories" {
		t.Fatal(original)
	}
	next := original
	next.Permission = "yolo"
	next.Naming = "generated"
	next.ClaudeContext = "Claude instructions"
	if err = s.Save(next); err != nil {
		t.Fatal(err)
	}
	if !errors.Is(s.Save(original), ErrConflict) {
		t.Fatal("stale settings overwrote newer preferences")
	}
	rate := telemetry.Rate{Input: 1000, Output: 2000, CacheRead: 100}
	if err = s.SetRate(s.Snapshot().Revision, "codex/custom-model", rate, false); err != nil {
		t.Fatal(err)
	}
	d, rates := s.SnapshotRates()
	if !strings.HasPrefix(rates["codex/custom-model"].Version, "custom-") {
		t.Fatal(rates)
	}
	d.Prices["codex/custom-model"] = telemetry.Rate{}
	delete(base, "codex/gpt-5.4")
	if s.Rates()["codex/custom-model"].Input != 1000 || s.Rates()["codex/gpt-5.4"].Input == 0 {
		t.Fatal("caller mutated store")
	}
	reopened, err := Open(path, telemetry.DefaultRates())
	if err != nil {
		t.Fatal(err)
	}
	if reopened.Snapshot().Permission != "yolo" || reopened.Rates()["codex/custom-model"].Input != 1000 {
		t.Fatal("restart lost settings")
	}
	stat, err := os.Stat(path)
	if err != nil || stat.Mode().Perm() != 0600 {
		t.Fatal("settings are not private", err)
	}
	if err = s.SetRate(s.Snapshot().Revision, "codex/custom-model", telemetry.Rate{}, true); err != nil {
		t.Fatal(err)
	}
	if _, ok := s.Rates()["codex/custom-model"]; ok {
		t.Fatal("override not removed")
	}
}
func TestInvalidSettingsAndFailedWriteDoNotChangeMemory(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "settings.json"), nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, change := range []func(*Data){func(d *Data) { d.Naming = "no" }, func(d *Data) { d.Permission = "bypass" }, func(d *Data) { d.Context = strings.Repeat("x", 65537) }} {
		d := s.Snapshot()
		change(&d)
		if s.Save(d) == nil {
			t.Fatal("invalid settings accepted")
		}
	}
	for _, key := range []string{"unknown/model", "claude/", "codex/bad/model"} {
		if s.SetRate(1, key, telemetry.Rate{Input: 1, Output: 1}, false) == nil {
			t.Fatal("invalid key accepted", key)
		}
	}
	if s.SetRate(1, "codex/model", telemetry.Rate{Input: -1, Output: 1}, false) == nil {
		t.Fatal("invalid price accepted")
	}
	// A directory at the destination forces the atomic rename to fail.
	if err = os.Mkdir(s.path, 0700); err != nil {
		t.Fatal(err)
	}
	d := s.Snapshot()
	d.Permission = "yolo"
	if s.Save(d) == nil {
		t.Fatal("write unexpectedly succeeded")
	}
	if s.Snapshot().Revision != 1 || s.Snapshot().Permission != "" {
		t.Fatal("failed write changed active defaults")
	}
}
func TestLivePricingUsesNewRate(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "settings.json"), nil)
	if err != nil {
		t.Fatal(err)
	}
	one, zero := int64(1), int64(0)
	r := telemetry.Record{Scope: telemetry.Scope{Provider: "codex"}, Model: "custom", Status: "complete", Tokens: telemetry.Tokens{Input: &one, Output: &one, CacheRead: &zero, CacheWrite5m: &zero, CacheWrite1h: &zero}}
	if s.Price(r).Nanos != nil {
		t.Fatal("unknown model priced")
	}
	if err = s.SetRate(1, "codex/custom", telemetry.Rate{Input: 1000, Output: 2000}, false); err != nil {
		t.Fatal(err)
	}
	priced := s.Price(r)
	if priced.Nanos == nil || *priced.Nanos != 3000 {
		t.Fatal(priced)
	}
}
