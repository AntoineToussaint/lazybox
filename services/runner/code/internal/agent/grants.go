package agent

import (
	"encoding/json"
	"errors"
	"time"
)

// GrantEvent is the event a provider sends when the person chose an "always
// allow" for this conversation that its process holds in memory: its Data is
// the Grant's provider half (id, label, tool and the provider's own spec).
const GrantEvent = "permission_grant"

// GrantLimit bounds the grants one conversation keeps.
const GrantLimit = 64

// Grant is an "always allow" the person gave for one conversation. The
// provider's process enforces it but forgets it when it ends; the Runner
// keeps it with the conversation, hands it to every later process of the
// same provider (a restart, a resume, a switch of subscription) and lists it
// so the person can take it back (RevokeGrant). A grant written to the
// workspace's own settings is the provider's, and not one of these.
type Grant struct {
	ID string `json:"id"`
	// Label is what it allows, in words: "commands starting with `npm run`".
	Label string `json:"label"`
	// Tool is the provider's tool it applies to.
	Tool      string    `json:"tool,omitempty"`
	GrantedAt time.Time `json:"grantedAt,omitzero"`
	// Spec is the provider's own description of the grant, handed back to a
	// new process unchanged (agent.Open.Grants).
	Spec json.RawMessage `json:"spec,omitempty"`
}

// GrantOf reads a GrantEvent: the provider's spec is the whole Data.
func GrantOf(ev Event, now time.Time) (Grant, error) {
	var g Grant
	if err := json.Unmarshal(ev.Data, &g); err != nil {
		return Grant{}, err
	}
	if g.ID == "" || len(g.ID) > 128 || g.Label == "" || len(g.Label) > 512 || len(ev.Data) > 16<<10 {
		return Grant{}, errors.New("grant needs an id and a label, within limits")
	}
	g.GrantedAt = now.UTC()
	g.Spec = append(json.RawMessage(nil), ev.Data...)
	return g, nil
}

// WithGrant is grants with g added, replacing one of the same id; the oldest
// go past GrantLimit.
func WithGrant(grants []Grant, g Grant) []Grant {
	out := WithoutGrant(grants, g.ID)
	out = append(out, g)
	if len(out) > GrantLimit {
		out = out[len(out)-GrantLimit:]
	}
	return out
}

// WithoutGrant is grants without the one named id; a new slice either way.
func WithoutGrant(grants []Grant, id string) []Grant {
	out := make([]Grant, 0, len(grants))
	for _, g := range grants {
		if g.ID != id {
			out = append(out, g)
		}
	}
	return out
}

// HasGrant reports whether grants holds id.
func HasGrant(grants []Grant, id string) bool {
	for _, g := range grants {
		if g.ID == id {
			return true
		}
	}
	return false
}

// GrantSpecs are the provider halves of grants, in order: what a new process
// of the provider is opened with.
func GrantSpecs(grants []Grant) []json.RawMessage {
	out := make([]json.RawMessage, 0, len(grants))
	for _, g := range grants {
		if len(g.Spec) > 0 {
			out = append(out, g.Spec)
		}
	}
	return out
}
