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
// reviewer credential, user home, network, or write access to the host tree.
// No supported isolation runtime means no execution, never a host fallback.
func isolateVerification(spec Spec) (Spec, error) {
	root, err := filepath.EvalSymlinks(spec.Cwd)
	if err != nil {
		return spec, err
	}
	home := filepath.Join(root, ".verification-home")
	tmp := filepath.Join(home, "tmp")
	// A repository must not redirect HOME/TMPDIR through a committed symlink.
	if err := os.Mkdir(home, 0700); err != nil {
		return spec, fmt.Errorf("create private verification home: %w", err)
	}
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
		profile := `(version 1)(deny default)(import "dyld-support.sb")(allow syscall* mach-bootstrap process-exec process-fork sysctl-read)(allow file-read-metadata)`
		for _, path := range []string{"/System/Library", "/usr/bin", "/usr/sbin", "/usr/lib", "/usr/libexec", "/usr/share", "/bin", "/sbin", "/Library/Developer", "/usr/local/bin", "/usr/local/lib", "/opt/homebrew/Cellar", "/opt/homebrew/opt", "/opt/homebrew/bin"} {
			profile += "(allow file-read* file-map-executable (subpath " + strconv.Quote(path) + "))"
		}
		profile += "(allow file-read* file-write* file-map-executable (subpath " + strconv.Quote(root) + "))"
		profile += `(allow file-read* (literal "/dev/null") (literal "/dev/urandom") (literal "/dev/random"))(allow file-write* (literal "/dev/null"))`
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
