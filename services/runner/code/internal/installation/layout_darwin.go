package installation

import "golang.org/x/sys/unix"

// statfsReadOnly reads the mount flags statfs(2) reports on macOS.
func statfsReadOnly(fs *unix.Statfs_t) bool { return fs.Flags&unix.MNT_RDONLY != 0 }

const readOnlyAdvice = "Lazybox is running from a disk image or another read-only location. Drag it to the Applications folder, eject the disk image, then open Lazybox from Applications."
