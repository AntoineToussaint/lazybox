package accounts

import "slices"

// Policy is an account policy (EXTENSIONS.md, "Account policy"): what an
// enabled extension asks the Runner to do when a conversation of a provider
// reaches a limit. Without one, the core never moves a conversation to
// another account by itself; the person switches explicitly.
type Policy struct {
	// Extension is the id of the extension the policy comes from.
	Extension string
	// Accounts are the accounts to try, in order; empty is every other
	// signed-in account of the provider (DefaultRotationPool).
	Accounts []string
}

// Pool is the accounts a conversation on selected may move to under p: its
// ordered accounts that are signed-in (isolated) accounts of the provider,
// without selected and the ones excluded in Settings, or the default pool.
func (p Policy) Pool(profiles []Profile, selected Profile, excluded []string) []string {
	if len(p.Accounts) == 0 {
		return DefaultRotationPool(profiles, selected, excluded)
	}
	pool := []string{}
	for _, id := range p.Accounts {
		if id == selected.ID || slices.Contains(excluded, id) || slices.Contains(pool, id) {
			continue
		}
		i := slices.IndexFunc(profiles, func(q Profile) bool { return q.ID == id })
		if i >= 0 && profiles[i].Provider == selected.Provider && profiles[i].Isolation == "environment" {
			pool = append(pool, id)
		}
		if len(pool) == MaxRotationProfiles {
			break
		}
	}
	return pool
}
