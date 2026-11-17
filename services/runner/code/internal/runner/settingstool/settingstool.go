// Package settingstool is the help assistant's boundary onto Lazybox's own
// settings. It reads the Runner preferences an assistant may see and applies a
// change to an allowlisted subset of them. It never reads credentials, account
// profiles or prices, and it never decides approval: the provider puts every
// update to the user before the Runner receives it, and the Runner applies it.
package settingstool

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"sort"
	"strings"
	"unicode/utf16"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/preferences"
)

// Settings is everything the assistant may read: the Settings → Agents &
// workspaces page, by the field names an update uses.
type Settings struct {
	Revision      uint64 `json:"revision"`
	Naming        string `json:"naming"`
	Permission    string `json:"permission"`
	Context       string `json:"context"`
	ClaudeContext string `json:"claudeContext"`
	CodexContext  string `json:"codexContext"`
}

// Result answers an applied update.
type Result struct {
	Applied  bool     `json:"applied"`
	Changed  []string `json:"changed"`
	Settings Settings `json:"settings"`
	Note     string   `json:"note"`
}

// field is one setting the assistant may change. Limits are the Settings
// page's, in UTF-16 code units like its maxLength; a nil allowed set means free text.
type field struct {
	label   string
	limit   int
	allowed []string
	value   func(*preferences.Data) *string
}

var fields = map[string]field{
	"naming":        {label: "Suggested workspace names", allowed: []string{"repositories", "generated"}, value: func(d *preferences.Data) *string { return &d.Naming }},
	"permission":    {label: "Default permissions", allowed: []string{"", "ask", "read-only", "yolo"}, value: func(d *preferences.Data) *string { return &d.Permission }},
	"context":       {label: "Lazybox context for every new agent", limit: 32768, value: func(d *preferences.Data) *string { return &d.Context }},
	"claudeContext": {label: "Additional Claude context", limit: 16384, value: func(d *preferences.Data) *string { return &d.ClaudeContext }},
	"codexContext":  {label: "Additional Codex context", limit: 16384, value: func(d *preferences.Data) *string { return &d.CodexContext }},
}

// notChangeable names what the Settings pages hold that this tool never
// touches, so a refusal can say where the user changes it instead.
const notChangeable = "model prices (Settings → Pricing), accounts and credentials, GitHub, repositories, keyboard shortcuts and snippets"

const reasonLimit = 2000

// Handle runs one settings tool call and returns its JSON result.
func Handle(store *preferences.Store, name string, args json.RawMessage) (string, error) {
	if store == nil {
		return "", errors.New("lazybox settings are unavailable on this Runner")
	}
	var value any
	switch name {
	case agent.SettingsReadTool:
		value = Get(store)
	case agent.SettingsUpdateTool:
		result, err := Update(store, args)
		if err != nil {
			return "", err
		}
		value = result
	default:
		return "", agent.ErrUnsupported
	}
	data, err := json.Marshal(value)
	return string(data), err
}

// Get returns the readable settings and the revision an update must name.
func Get(store *preferences.Store) Settings {
	return view(store.Snapshot())
}

func view(d preferences.Data) Settings {
	return Settings{Revision: d.Revision, Naming: d.Naming, Permission: d.Permission, Context: d.Context, ClaudeContext: d.ClaudeContext, CodexContext: d.CodexContext}
}

// Update applies an approved proposal. It refuses a proposal made against a
// revision that is no longer current, so it never overwrites a change the
// assistant did not see — including its own, if a call is delivered twice.
func Update(store *preferences.Store, args json.RawMessage) (Result, error) {
	var p struct {
		Revision uint64                     `json:"revision"`
		Reason   string                     `json:"reason"`
		Changes  map[string]json.RawMessage `json:"changes"`
	}
	decoder := json.NewDecoder(bytes.NewReader(args))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&p); err != nil {
		return Result{}, fmt.Errorf("invalid settings proposal: %v; send {revision, reason, changes}", err)
	}
	if p.Revision == 0 {
		return Result{}, fmt.Errorf("revision required: call %s and propose the change against the revision it returns", agent.SettingsReadTool)
	}
	if strings.TrimSpace(p.Reason) == "" || len(p.Reason) > reasonLimit {
		return Result{}, fmt.Errorf("reason required (at most %d bytes): it explains the change to the user on the approval card", reasonLimit)
	}
	if len(p.Changes) == 0 {
		return Result{}, errors.New("changes must name at least one setting")
	}
	current := store.Snapshot()
	if current.Revision != p.Revision {
		return Result{}, conflict(p.Revision, current.Revision)
	}
	next := current
	keys := make([]string, 0, len(p.Changes))
	for key := range p.Changes {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	var changed []string
	for _, key := range keys {
		raw := p.Changes[key]
		f, ok := fields[key]
		if !ok {
			return Result{}, fmt.Errorf("%q cannot be changed by the assistant; it can change only %s. It cannot change %s", key, changeable(), notChangeable)
		}
		var value string
		if err := json.Unmarshal(raw, &value); err != nil {
			return Result{}, fmt.Errorf("%s must be a string", key)
		}
		if err := check(key, f, value); err != nil {
			return Result{}, err
		}
		if target := f.value(&next); *target != value {
			*target = value
			changed = append(changed, key)
		}
	}
	if len(changed) == 0 {
		return Result{}, errors.New("every proposed value matches the current settings; nothing to change")
	}
	if err := store.Save(next); err != nil {
		if errors.Is(err, preferences.ErrConflict) {
			return Result{}, conflict(p.Revision, store.Snapshot().Revision)
		}
		return Result{}, err
	}
	return Result{Applied: true, Changed: changed, Settings: Get(store), Note: "Saved. Defaults apply to new sessions; existing and resumed conversations keep their saved settings."}, nil
}

func check(key string, f field, value string) error {
	if f.allowed != nil {
		for _, allowed := range f.allowed {
			if value == allowed {
				return nil
			}
		}
		quoted := make([]string, len(f.allowed))
		for i, allowed := range f.allowed {
			quoted[i] = fmt.Sprintf("%q", allowed)
		}
		return fmt.Errorf("%s must be one of %s", key, strings.Join(quoted, ", "))
	}
	if n := len(utf16.Encode([]rune(value))); n > f.limit {
		return fmt.Errorf("%s (%s) is %d characters; the limit is %d", key, f.label, n, f.limit)
	}
	return nil
}

func changeable() string {
	keys := make([]string, 0, len(fields))
	for key := range fields {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return strings.Join(keys, ", ")
}

func conflict(proposed, current uint64) error {
	return fmt.Errorf("lazybox settings changed since revision %d (now %d), so nothing was saved. Call %s, check the change against the current values with the user, and propose it again: %w", proposed, current, agent.SettingsReadTool, preferences.ErrConflict)
}
