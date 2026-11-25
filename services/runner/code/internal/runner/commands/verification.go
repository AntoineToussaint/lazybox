package commands

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"time"
)

// isolateVerification is the command boundary: verification never inherits a
// reviewer credential, user home, network beyond loopback, or write access to
// the host tree. No supported isolation runtime means no execution, never a
// host fallback.
//
// Loopback is allowed because tests start local servers (a Vite dev server,
// a fixture Runner) and connect to them on 127.0.0.1. On Linux bubblewrap's
// own network namespace has only its own loopback interface, which it brings
// up: the command reaches the servers it started and nothing else, not even
// the host's loopback. Seatbelt on macOS has no private network namespace,
// and its only address filter is "localhost", which means this machine's
// own addresses: the command may listen and connect there, and cannot
// connect to any other machine (loopbackProfile).
//
// hw.pagesize_compat is what the numeric HW_PAGESIZE MIB resolves to on
// arm64: without it no Go program (a test binary `go test` built, for one)
// gets past its runtime's start.
//
// loopbackProfile is the only network a macOS verification command has:
// binding, listening and connecting on this machine's addresses. A
// connection to any other machine is refused (EPERM) before a packet leaves.
// Seatbelt cannot narrow it further: it accepts no literal address (only "*"
// or "localhost"), and it does not check the peer a listening socket accepts,
// so a server the test binds to a wildcard address is reachable from the
// network, unless the macOS firewall stops it, for the command's lifetime
// (MaxDurationMs). The host's own loopback services are reachable too.
const loopbackProfile = `(allow network-bind network-inbound (local ip "localhost:*"))(allow network-outbound (remote ip "localhost:*"))`

func isolateVerification(spec Spec) (Spec, error) {
	root, err := filepath.EvalSymlinks(spec.Cwd)
	if err != nil {
		return spec, err
	}
	// Allocate a fresh directory: a repository cannot pre-create a symlink
	// at HOME or reserve its name to prevent verification.
	home, err := os.MkdirTemp(root, ".verification-home-")
	if err != nil {
		return spec, fmt.Errorf("create private verification home: %w", err)
	}
	tmp := filepath.Join(home, "tmp")
	if err := os.Mkdir(tmp, 0700); err != nil {
		return spec, err
	}
	spec.Cwd = root
	spec.Env = []string{"PATH=/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin", "HOME=" + home, "TMPDIR=" + tmp, "LANG=C", "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null"}
	switch runtime.GOOS {
	case "darwin":
		sandbox, err := exec.LookPath("sandbox-exec")
		if err != nil {
			return spec, errors.New("verification requires sandbox-exec")
		}
		profile := `(version 1)(deny default)(import "dyld-support.sb")(allow syscall* mach-bootstrap process-exec process-fork)(allow sysctl-read (sysctl-name "hw.ncpu" "hw.activecpu" "hw.physicalcpu" "hw.logicalcpu" "hw.memsize" "hw.pagesize" "hw.pagesize_compat" "hw.optional.arm64" "kern.ostype" "kern.osrelease" "kern.osversion" "kern.version" "kern.argmax"))(allow file-read-metadata)`
		for _, path := range []string{"/System/Library", "/usr/bin", "/usr/sbin", "/usr/lib", "/usr/libexec", "/usr/share", "/bin", "/sbin", "/Library/Developer", "/usr/local/bin", "/usr/local/lib", "/opt/homebrew/Cellar", "/opt/homebrew/opt", "/opt/homebrew/bin"} {
			profile += "(allow file-read* file-map-executable (subpath " + strconv.Quote(path) + "))"
		}
		profile += "(allow file-read* file-write* file-map-executable (subpath " + strconv.Quote(root) + "))"
		profile += `(allow file-read* (literal "/dev/null") (literal "/dev/urandom") (literal "/dev/random"))(allow file-write* (literal "/dev/null"))`
		profile += loopbackProfile
		spec.Argv = append([]string{sandbox, "-p", profile, "--"}, spec.Argv...)
	case "linux":
		sandbox, err := exec.LookPath("bwrap")
		if err != nil {
			return spec, errors.New("verification requires bubblewrap with unprivileged namespaces")
		}
		args := []string{sandbox, "--unshare-all", "--die-with-parent", "--new-session"}
		for _, path := range []string{"/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc/ld.so.cache"} {
			if _, err := os.Stat(path); err == nil {
				args = append(args, "--ro-bind", path, path)
			}
		}
		args = append(args, "--proc", "/proc", "--dev", "/dev", "--bind", root, root, "--chdir", root, "--")
		spec.Argv = append(args, spec.Argv...)
	default:
		return spec, errors.New("verification isolation is unavailable on this platform")
	}
	spec.Isolated = true
	spec.MaxDurationMs = (15 * time.Minute).Milliseconds()
	return spec, nil
}
