package codex

import (
	"context"
	"encoding/json"
	"os"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// The version an initialize response names (userAgent, as 0.169.0 writes it)
// is checked against TestedVersions: inside, nothing is said; outside, one
// plain line naming the version and the tested range; unreadable, nothing.
func TestCodexVersionOutsideTheTestedRangeIsSaid(t *testing.T) {
	agentOf := func(v string) json.RawMessage {
		return json.RawMessage(`{"userAgent":"lazybox_v2_testbench/` + v + ` (Mac OS 27.2.1; arm64) ghostty/1.4.0 (lazybox_v2_testbench; 0.1.0)","codexHome":"/h","platformFamily":"unix","platformOs":"macos"}`)
	}
	for v, untested := range map[string]bool{
		"0.164.0": false, "0.166.2": false, "0.169.0": false, "0.169.7": false,
		"0.163.9": true, "0.170.0": true, "0.170.0-alpha.2": true, "1.0.0": true,
	} {
		ev, said := versionNotice(agentOf(v))
		if said != untested {
			t.Fatalf("%s: notice %v", v, said)
		}
		if said && (ev.Type != UntestedVersionNotice || ev.Status != "untested_version" || ev.Text != "This Codex version ("+v+") isn't tested with Lazybox yet; some features may not work. Lazybox is tested with Codex 0.164 to 0.169.") {
			t.Fatalf("%s: %+v", v, ev)
		}
	}
	for _, odd := range []string{`{}`, `{"userAgent":"codex"}`, `{"userAgent":"codex/next (x)"}`, `not json`} {
		if _, said := versionNotice(json.RawMessage(odd)); said {
			t.Fatalf("an unreadable version was reported: %s", odd)
		}
	}
}

// Through the app-server: a Codex outside the range says so as the
// conversation's first event; one inside says nothing.
func TestUntestedCodexShowsOneLineAtStart(t *testing.T) {
	self, _ := os.Executable()
	for _, tc := range []struct {
		version string
		said    bool
	}{{"0.171.0", true}, {"0.169.0", false}} {
		t.Setenv("FIXTURE_CODEX_VERSION", tc.version)
		s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Host: &agent.Hosting{Root: fixtureRoot(t)}})
		if err != nil {
			t.Fatal(err)
		}
		if tc.said {
			ev := receive(t, s, UntestedVersionNotice)
			if ev.Text == "" || ev.Status != "untested_version" {
				t.Fatalf("%s: %+v", tc.version, ev)
			}
		} else {
			_, _ = s.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "hi"})
			ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
			for {
				ev, err := s.Receive(ctx)
				if err != nil {
					t.Fatal(err)
				}
				if ev.Type == UntestedVersionNotice {
					t.Fatalf("a tested Codex was reported: %+v", ev)
				}
				if ev.Type == "turn_started" {
					break
				}
			}
			cancel()
		}
		_ = s.Close()
	}
}
