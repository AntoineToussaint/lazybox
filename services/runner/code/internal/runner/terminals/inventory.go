package terminals

import "path/filepath"

// Label names a terminal for people: its title, or else its directory.
func Label(spec Spec) string {
	if spec.Title != "" {
		return spec.Title
	}
	if spec.Directory != "" {
		return "Terminal · " + spec.Directory
	}
	return "Terminal · " + filepath.Base(spec.Cwd)
}

// Hosted maps the session host of every terminal this Runner knows to the
// terminal's status: what the Runner's process inventory names a host by.
func (m *Manager) Hosted() map[string]Status {
	m.mu.Lock()
	items := make([]*Terminal, 0, len(m.items))
	for _, t := range m.items {
		items = append(items, t)
	}
	m.mu.Unlock()
	hosted := map[string]Status{}
	for _, t := range items {
		if t.hostID != "" {
			hosted[t.hostID] = t.Status()
		}
	}
	return hosted
}
