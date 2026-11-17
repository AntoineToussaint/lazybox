package commands

import "strings"

// Label names a command for people: its title, or else its command line.
func Label(spec Spec) string {
	if spec.Title != "" {
		return spec.Title
	}
	return strings.Join(spec.Argv, " ")
}

// Hosted maps the session host of every command that ran in one to the
// command's status: what the Runner's process inventory names a host by.
// A command's host is fixed when it is registered.
func (m *Manager) Hosted() map[string]Status {
	m.mu.Lock()
	items := make([]*Command, 0, len(m.items))
	for _, c := range m.items {
		items = append(items, c)
	}
	m.mu.Unlock()
	hosted := map[string]Status{}
	for _, c := range items {
		if c.hostID != "" {
			hosted[c.hostID] = c.Status()
		}
	}
	return hosted
}
