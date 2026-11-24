// Package atomicfile replaces private state files so a crash or power loss
// leaves either the old content or the new, never a mix and never neither.
//
// A rename is only durable once the directory that holds the name is synced:
// without it, a file synced and renamed can come back after power loss under
// its old name, or not at all. Every store that renames into place goes
// through here so none forgets that step.
package atomicfile

import (
	"errors"
	"os"
	"path/filepath"
)

// WriteFile replaces path with data, readable and writable by its owner
// only. The data reaches the disk before the rename, and the rename before
// WriteFile returns. Its temporary file, in path's directory, is named
// ".<base>.tmp-*" and is removed on failure.
func WriteFile(path string, data []byte) error {
	dir := filepath.Dir(path)
	f, err := os.CreateTemp(dir, "."+filepath.Base(path)+".tmp-*")
	if err != nil {
		return err
	}
	name := f.Name()
	renamed := false
	defer func() {
		if !renamed {
			_ = os.Remove(name)
		}
	}()
	if err = f.Chmod(0o600); err == nil {
		_, err = f.Write(data)
	}
	if err == nil {
		err = f.Sync()
	}
	if err = errors.Join(err, f.Close()); err != nil {
		return err
	}
	if err = os.Rename(name, path); err != nil {
		return err
	}
	renamed = true
	return SyncDir(dir)
}

// SyncDir makes the creations, renames and removals already done in dir
// durable.
func SyncDir(dir string) error {
	d, err := os.Open(dir)
	if err != nil {
		return err
	}
	return errors.Join(d.Sync(), d.Close())
}
