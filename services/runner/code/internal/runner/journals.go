package runner

import (
	"path/filepath"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

// journalPath is the first segment of execution id's event journal; later
// segments follow it as <id>.jsonl.1, <id>.jsonl.2… (journal.Rollover).
func (b *Bench) journalPath(id string) string {
	return filepath.Join(b.Config.State, "sessions", id+".jsonl")
}

func (b *Bench) journalOptions() []journal.Option {
	return []journal.Option{journal.Marking(turnStart), journal.Rollover(b.Config.EventHistoryBudget)}
}

// openJournal opens execution id's event journal for appending, creating it.
func (b *Bench) openJournal(id string) (*journal.Journal, error) {
	return journal.Open(b.journalPath(id), b.Config.EventBudget, b.journalOptions()...)
}

// readJournal opens execution id's event journal read-only, up to the first
// record it cannot read (journal.Journal.Damage).
func (b *Bench) readJournal(id string) (*journal.Journal, error) {
	return journal.OpenExisting(b.journalPath(id), b.Config.EventBudget, append(b.journalOptions(), journal.Salvage())...)
}
