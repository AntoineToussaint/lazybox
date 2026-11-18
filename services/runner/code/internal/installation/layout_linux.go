package installation

import "golang.org/x/sys/unix"

// statfsReadOnly reads the mount flags statfs(2) reports on Linux, where a
// read-only mount sets ST_RDONLY (MNT_RDONLY is a mount(2) flag on macOS).
func statfsReadOnly(fs *unix.Statfs_t) bool { return fs.Flags&unix.ST_RDONLY != 0 }

const readOnlyAdvice = "Lazybox is running from a read-only location. Unpack the release under your home directory (see `lazybox doctor`), then run its `lazybox service install`."
