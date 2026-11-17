package work

import (
	"bytes"
	"encoding/json"
	"flag"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

var update = flag.Bool("update", false, "rewrite the golden boards from the mission logs")

// goldenBoard is what a golden file pins: the derived row of every task.
type goldenBoard struct {
	Mission string       `json:"mission"`
	Title   string       `json:"title"`
	Process string       `json:"process"`
	Seq     uint64       `json:"seq"`
	Ignored int          `json:"ignored"`
	Tasks   []goldenTask `json:"tasks"`
}
type goldenTask struct {
	ID            string   `json:"id"`
	Status        string   `json:"status"`
	Trust         string   `json:"trust,omitempty"`
	EvidenceTrust string   `json:"evidence_trust"`
	Flags         []string `json:"flags,omitempty"`
	Gate          string   `json:"gate,omitempty"`
	Live          string   `json:"live,omitempty"`
	Reason        string   `json:"reason"`
}

func readLog(t *testing.T, path string) []Event {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var events []Event
	for _, line := range bytes.Split(bytes.TrimSpace(data), []byte("\n")) {
		var e Event
		if err := json.Unmarshal(line, &e); err != nil {
			t.Fatal(err)
		}
		events = append(events, e)
	}
	return events
}

func board(s *State, obs Observations) goldenBoard {
	b := goldenBoard{Mission: s.ID, Title: s.Title, Seq: s.Seq, Ignored: s.Ignored}
	if s.Policy != nil {
		b.Process = s.Policy.Preset()
	}
	for _, v := range Derive(s, obs) {
		b.Tasks = append(b.Tasks, goldenTask{ID: v.ID, Status: v.Status, Trust: v.Trust, EvidenceTrust: v.EvidenceTrust, Flags: v.Flags, Gate: v.Gate, Live: v.Live.State, Reason: v.Reason})
	}
	return b
}

// Golden mission logs replay to the same board every time: the log, not any
// stored status, is the truth, and an old log keeps meaning what it meant.
func TestGoldenMissionLogs(t *testing.T) {
	logs, err := filepath.Glob("testdata/missions/*.jsonl")
	if err != nil || len(logs) < 4 {
		t.Fatalf("golden logs missing: %v %v", logs, err)
	}
	for _, path := range logs {
		name := strings.TrimSuffix(filepath.Base(path), ".jsonl")
		t.Run(name, func(t *testing.T) {
			events := readLog(t, path)
			var obs Observations
			if data, err := os.ReadFile(strings.TrimSuffix(path, ".jsonl") + ".observations.json"); err == nil {
				if err := json.Unmarshal(data, &obs.Conversations); err != nil {
					t.Fatal(err)
				}
			}
			state, err := Fold(events)
			if err != nil {
				t.Fatalf("a golden log must fold strictly: %v", err)
			}
			got := board(state, obs)
			// Deterministic: the same log and observations give the same board.
			again, _ := Fold(events)
			if a, b := mustJSON(t, got), mustJSON(t, board(again, obs)); !bytes.Equal(a, b) {
				t.Fatalf("replay is not deterministic:\n%s\n%s", a, b)
			}
			golden := strings.TrimSuffix(path, ".jsonl") + ".board.json"
			data := mustJSON(t, got)
			if *update {
				if err := os.WriteFile(golden, data, 0o644); err != nil {
					t.Fatal(err)
				}
			}
			want, err := os.ReadFile(golden)
			if err != nil {
				t.Fatalf("no golden board (run go test ./internal/work -update): %v", err)
			}
			if !bytes.Equal(bytes.TrimSpace(want), bytes.TrimSpace(data)) {
				t.Fatalf("board differs from %s:\n%s", golden, data)
			}
		})
	}
}

func mustJSON(t *testing.T, v any) []byte {
	t.Helper()
	data, err := json.MarshalIndent(v, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	return append(data, '\n')
}
