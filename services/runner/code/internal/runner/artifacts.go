package runner

import (
	"context"
	"errors"
	"io/fs"
	"time"
)

// watchArtifacts follows every tracked artifact file until the Runner stops:
// a file an agent (or anyone) changes on disk becomes the artifact's next
// version once it stops changing.
func (b *Bench) watchArtifacts() {
	if b.Collections == nil || b.Artifacts == nil {
		return
	}
	interval, debounce := b.Config.ArtifactPoll, b.Config.ArtifactDebounce
	if interval <= 0 {
		interval = 2 * time.Second
	}
	if debounce <= 0 {
		debounce = 2 * time.Second
	}
	b.watchers.Add(1)
	go func() {
		defer b.watchers.Done()
		b.Artifacts.Watch(b.ctx, b.readArtifactFile, interval, debounce)
	}()
}

// readArtifactFile reads a tracked file through the workspace's own
// confinement rules, naming why it cannot be followed when that is the case.
func (b *Bench) readArtifactFile(ctx context.Context, workspace, path string) (string, string, error) {
	file, err := b.Collections.ReadFile(ctx, workspace, path)
	switch {
	case errors.Is(err, fs.ErrNotExist):
		return "", "the file was removed; the last version is kept", nil
	case err != nil:
		return "", "", err
	case file.Directory:
		return "", "the path is now a folder; the last version is kept", nil
	case file.Binary:
		return "", "not a text file", nil
	case file.Truncated:
		return "", "larger than 1 MiB", nil
	}
	return file.Text, "", nil
}

// SessionWorkspace is the workspace an execution (live or saved) works in, or
// "" for one outside any workspace.
func (b *Bench) SessionWorkspace(id string) (string, error) {
	e, err := b.Get(id)
	if err != nil {
		return "", err
	}
	return e.CollectionID, nil
}

// SessionConversation is the conversation an execution belongs to.
func (b *Bench) SessionConversation(id string) (string, error) {
	e, err := b.Get(id)
	if err != nil {
		return "", err
	}
	return e.Saved.ConversationID, nil
}
