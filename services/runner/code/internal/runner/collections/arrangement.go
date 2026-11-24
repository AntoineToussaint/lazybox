package collections

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sort"
)

// The person's order of their workspaces (ArrangeWorkspaces): the order List
// answers in, so every window and a paired phone list them alike. It is kept
// beside the records, in .lazybox-collections/arrangement.json, and survives a
// Runner restart. It is presentation only: nothing else reads it.

const arrangementFile = "arrangement.json"

// maxArranged bounds the kept order like the record capacity bounds records.
const maxArranged = 16384

// ErrArrangementTooLong refuses an order longer than any Runner keeps workspaces.
var ErrArrangementTooLong = errors.New("workspace order exceeds the workspace capacity")

// loadArrangement reads the kept order. A missing file is no order; an
// unreadable one is none either: the order is the person's convenience, and
// losing it must never keep a Runner from starting.
func (m *Manager) loadArrangement() {
	data, err := os.ReadFile(filepath.Join(m.root, ".lazybox-collections", arrangementFile))
	if err != nil || len(data) > 1<<20 {
		return
	}
	var ids []string
	if json.Unmarshal(data, &ids) != nil || len(ids) > maxArranged {
		return
	}
	m.order = cleanOrder(ids, nil)
}

// cleanOrder drops duplicates and IDs that cannot be a workspace's, and, when
// known is given, IDs it does not hold.
func cleanOrder(ids []string, known func(string) bool) []string {
	seen := map[string]bool{}
	out := make([]string, 0, len(ids))
	for _, id := range ids {
		if seen[id] || !component.MatchString(id) || (known != nil && !known(id)) {
			continue
		}
		seen[id] = true
		out = append(out, id)
	}
	return out
}

// Arrange keeps the person's order of their workspaces and answers the list
// in it. A workspace left out goes first, as a new one does; an unknown or
// deleted one is ignored; an empty order clears it.
func (m *Manager) Arrange(ids []string) ([]Collection, error) {
	if len(ids) > maxArranged {
		return nil, ErrArrangementTooLong
	}
	m.mu.Lock()
	order := cleanOrder(ids, func(id string) bool { c, ok := m.items[id]; return ok && c.State != "deleted" })
	err := m.saveArrangement(order)
	if err == nil {
		m.recordsMu.Lock()
		m.order = order
		m.recordsMu.Unlock()
	}
	m.mu.Unlock()
	if err != nil {
		return nil, err
	}
	return m.List(), nil
}

// saveArrangement writes the order atomically. The caller holds m.mu.
func (m *Manager) saveArrangement(order []string) error {
	data, err := json.Marshal(order)
	if err != nil {
		return err
	}
	dir := filepath.Join(m.root, ".lazybox-collections")
	f, err := os.CreateTemp(dir, ".arrangement-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	if err = errors.Join(err, f.Close()); err != nil {
		return err
	}
	if err = os.Rename(f.Name(), filepath.Join(dir, arrangementFile)); err != nil {
		return err
	}
	d, err := os.Open(dir)
	if err != nil {
		return err
	}
	defer d.Close()
	return d.Sync()
}

// arrange sorts workspaces in the person's order: those never placed (created
// since) first, newest first; then the placed ones in their order. With no
// order kept, by ID, as before there was one.
func arrange(list []Collection, order []string) {
	sort.Slice(list, func(i, j int) bool { return list[i].ID < list[j].ID })
	if len(order) == 0 {
		return
	}
	at := make(map[string]int, len(order))
	for i, id := range order {
		at[id] = i
	}
	sort.SliceStable(list, func(i, j int) bool {
		a, placedA := at[list[i].ID]
		b, placedB := at[list[j].ID]
		switch {
		case placedA != placedB:
			return !placedA
		case !placedA:
			return list[i].CreatedAt > list[j].CreatedAt
		default:
			return a < b
		}
	})
}

// ArrangeConversations keeps sidebar positions on their workspace record.
func (m *Manager) ArrangeConversations(id string, ids []string) (Collection, error) {
	if len(ids) > maxArranged {
		return Collection{}, ErrArrangementTooLong
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.items[id]
	if !ok || c.State == "deleted" {
		return Collection{}, os.ErrNotExist
	}
	if m.closed {
		return Collection{}, errors.New("workspace manager closed")
	}
	c = clone(c)
	c.ConversationOrder = cleanOrder(ids, nil)
	if err := m.save(c); err != nil {
		return Collection{}, err
	}
	return clone(c), nil
}
