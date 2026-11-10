package collections

import (
	"context"
	"errors"
)

// InspectionPath resolves and verifies an owned member; callers cannot supply
// arbitrary filesystem locations through the review API.
func (m *Manager) InspectionPath(ctx context.Context, id, name string) (string, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return "", errors.New("workspace manager closed")
	}
	c, ok := m.items[id]
	if !ok {
		return "", errors.New("workspace not found")
	}
	for _, member := range c.Members {
		if member.Name == name && member.Ready {
			if err := m.validate(ctx, member); err != nil {
				return "", err
			}
			return member.Path, nil
		}
	}
	return "", errors.New("ready repository member not found")
}
