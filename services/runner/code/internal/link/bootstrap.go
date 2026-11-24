package link

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	_ "embed"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"
)

// Bootstrap over SSH (REMOTE_RUNNERS §4.1): probe the host and change
// nothing; install a Runner bundle whose SHA-256 the host checks before
// unpacking; start the Runner; enrol this client's keys as an owner over the
// same SSH login and pin the Runner's key from its answer. SSH (its host key
// check and the person's login) is what makes that first answer trusted, as
// it is for authorized_keys.

//go:embed scripts/probe.sh
var probeScript string

//go:embed scripts/install.sh
var installScript string

// Probe is what a host says about itself before anything changes.
type Probe struct {
	OS       string `json:"os"`
	Arch     string `json:"arch"`
	Hostname string `json:"hostname"`
	// Lazybox is the lazybox CLI found on the host, empty when there is none;
	// Version is its version, Link whether it serves links (1) or predates them (0).
	Lazybox string `json:"lazybox"`
	Version string `json:"version"`
	Link    int    `json:"link"`
	// DataDir is where an install goes (${XDG_DATA_HOME:-~/.local/share}/lazybox/runner).
	DataDir  string `json:"dataDir"`
	Systemd  bool   `json:"systemd"`
	Launchd  bool   `json:"launchd"`
	FreeKB   int64  `json:"freeKB"`
	Claude   bool   `json:"claude"`
	Codex    bool   `json:"codex"`
	Git      bool   `json:"git"`
	Platform string `json:"platform"`
}

// Summary is the probe as one line: "Linux arm64 · systemd · 38 GB free · Claude Code found · Codex missing".
func (p Probe) Summary() string {
	parts := []string{strings.TrimSpace(osName(p.OS) + " " + p.Arch)}
	if p.Systemd {
		parts = append(parts, "systemd")
	}
	if p.FreeKB > 0 {
		parts = append(parts, fmt.Sprintf("%d GB free", p.FreeKB/(1<<20)))
	}
	if p.Lazybox != "" {
		parts = append(parts, "Lazybox "+firstNonEmpty(p.Version, "found"))
	} else {
		parts = append(parts, "no Lazybox yet")
	}
	found := func(ok bool, name string) string {
		if ok {
			return name + " found"
		}
		return name + " missing"
	}
	parts = append(parts, found(p.Claude, "Claude Code"), found(p.Codex, "Codex"))
	if !p.Git {
		parts = append(parts, "git missing")
	}
	return strings.Join(parts, " · ")
}

func osName(s string) string {
	switch strings.ToLower(s) {
	case "darwin":
		return "macOS"
	case "linux":
		return "Linux"
	}
	return s
}

// GoPlatform is the host as GOOS/GOARCH ("linux-arm64"), the bundle it needs.
func (p Probe) GoPlatform() string {
	goos := strings.ToLower(p.OS)
	arch := strings.ToLower(p.Arch)
	switch arch {
	case "x86_64", "amd64":
		arch = "amd64"
	case "aarch64", "arm64":
		arch = "arm64"
	}
	return goos + "-" + arch
}

// RunProbe runs the probe script on the host (an ssh host, a Kubernetes container).
func RunProbe(ctx context.Context, r OwnerRoute) (Probe, error) {
	out, err := r.Run(ctx, strings.NewReader(probeScript), "sh", "-s")
	if err != nil {
		return Probe{}, err
	}
	var p Probe
	line := lastLine(string(out))
	if err = json.Unmarshal([]byte(line), &p); err != nil || p.OS == "" {
		return Probe{}, fmt.Errorf("%s answered the probe with something else than Lazybox expects: %q", r.Describe().Host, truncate(string(out), 200))
	}
	return p, nil
}

// decodeOutput reads a host command's JSON answer: the whole output (an
// indented --json report), else its last line (a script's one-line answer
// after anything a login profile printed).
func decodeOutput(out []byte, v any) error {
	if err := json.Unmarshal(bytes.TrimSpace(out), v); err == nil {
		return nil
	}
	return json.Unmarshal([]byte(lastLine(string(out))), v)
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}

var versionPattern = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$`)
var shaPattern = regexp.MustCompile(`^[0-9a-f]{64}$`)

// Bundle is a Runner bundle to install: a tar.gz of a runtime directory
// (VERSION, bin/lazybox, bin/lazybox-runner, and node, worker and web for a
// full release), its SHA-256, and the version it is.
type Bundle struct {
	Path    string
	SHA256  string
	Version string
}

// OpenBundle reads a bundle file and checks it against want when given.
func OpenBundle(path, version, want string) (Bundle, error) {
	f, err := os.Open(path)
	if err != nil {
		return Bundle{}, err
	}
	defer f.Close()
	sum := sha256.New()
	if _, err = io.Copy(sum, f); err != nil {
		return Bundle{}, err
	}
	got := hex.EncodeToString(sum.Sum(nil))
	if want != "" && !strings.EqualFold(want, got) {
		return Bundle{}, fmt.Errorf("%s has SHA-256 %s, not the %s it should have", path, got, want)
	}
	if version == "" {
		if version, err = bundleVersion(path); err != nil {
			return Bundle{}, err
		}
	}
	if !versionPattern.MatchString(version) {
		return Bundle{}, fmt.Errorf("%q is not a version", version)
	}
	return Bundle{Path: path, SHA256: got, Version: version}, nil
}

// bundleVersion reads VERSION from inside a bundle.
func bundleVersion(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	gz, err := gzip.NewReader(f)
	if err != nil {
		return "", fmt.Errorf("%s is not a .tar.gz bundle: %w", path, err)
	}
	tr := tar.NewReader(gz)
	for {
		h, err := tr.Next()
		if err != nil {
			return "", fmt.Errorf("%s has no VERSION file", path)
		}
		if strings.TrimPrefix(h.Name, "./") == "VERSION" {
			data, _ := io.ReadAll(io.LimitReader(tr, 128))
			return strings.TrimSpace(string(data)), nil
		}
	}
}

// PackRuntime writes a runtime directory as a bundle into dir.
func PackRuntime(root, dir string) (Bundle, error) {
	data, err := os.ReadFile(filepath.Join(root, "VERSION"))
	if err != nil {
		return Bundle{}, fmt.Errorf("%s is not a Lazybox runtime (no VERSION)", root)
	}
	version := strings.TrimSpace(string(data))
	out, err := os.CreateTemp(dir, "lazybox-runner-*.tar.gz")
	if err != nil {
		return Bundle{}, err
	}
	defer out.Close()
	gz := gzip.NewWriter(out)
	tw := tar.NewWriter(gz)
	err = filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, _ := filepath.Rel(root, path)
		if rel == "." {
			return nil
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		link := ""
		if info.Mode()&fs.ModeSymlink != 0 {
			if link, err = os.Readlink(path); err != nil {
				return err
			}
		}
		h, err := tar.FileInfoHeader(info, link)
		if err != nil {
			return err
		}
		h.Name = filepath.ToSlash(rel)
		h.Uname, h.Gname, h.Uid, h.Gid = "", "", 0, 0
		if err = tw.WriteHeader(h); err != nil {
			return err
		}
		if info.Mode().IsRegular() {
			f, err := os.Open(path)
			if err != nil {
				return err
			}
			_, err = io.Copy(tw, f)
			f.Close()
			return err
		}
		return nil
	})
	if err = errors.Join(err, tw.Close(), gz.Close(), out.Close()); err != nil {
		_ = os.Remove(out.Name())
		return Bundle{}, err
	}
	return OpenBundle(out.Name(), version, "")
}

// Install uploads a bundle over SSH and installs it on the host: the host
// checks the SHA-256 this computer computed before it unpacks anything, then
// switches `current` to it, keeping the previous version for rollback.
// It returns the installed lazybox's path on the host.
func Install(ctx context.Context, r SSHRoute, b Bundle) (string, error) {
	if !shaPattern.MatchString(b.SHA256) || !versionPattern.MatchString(b.Version) {
		return "", errors.New("the bundle has no valid version or SHA-256")
	}
	f, err := os.Open(b.Path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	upload := ".cache/lazybox/upload-" + b.SHA256[:16] + ".tar.gz"
	if _, err = r.Run(ctx, f, "sh", "-c", `umask 077 && mkdir -p "$HOME/.cache/lazybox" && cat > "$HOME/$1"`, "sh", upload); err != nil {
		return "", fmt.Errorf("uploading the Runner: %w", err)
	}
	out, err := r.Run(ctx, strings.NewReader(installScript), "sh", "-s", "--", b.Version, b.SHA256, upload)
	if err != nil {
		var sshErr *SSHError
		if errors.As(err, &sshErr) && strings.Contains(sshErr.Stderr, "checksum mismatch") {
			return "", fmt.Errorf("%s received a Runner whose SHA-256 does not match: nothing was installed (%s)", r.Host, lastLine(sshErr.Stderr))
		}
		return "", fmt.Errorf("installing the Runner: %w", err)
	}
	var result struct {
		Path string `json:"path"`
	}
	if err = decodeOutput(out, &result); err != nil || result.Path == "" {
		return "", fmt.Errorf("the install on %s said %q", r.Host, truncate(string(out), 200))
	}
	return result.Path, nil
}

// Up is what `lazybox link up` reports: the host's Runner, running.
type Up struct {
	State    string `json:"state"`
	Service  string `json:"service"`
	Runner   string `json:"runner"`
	Endpoint string `json:"endpoint"`
	Version  string `json:"version"`
	Problem  string `json:"problem,omitempty"`
	// PID is a Runner started detached (service none); also <state>/runner.pid.
	PID int `json:"pid,omitempty"`
	// StartsWhen says when a service starts the Runner by itself (at login,
	// at boot with lingering, or not), from `lazybox service install`.
	StartsWhen string `json:"startsWhen,omitempty"`
	// Note is why the Runner runs detached when a service was wanted.
	Note string `json:"note,omitempty"`
}

// StartRunner makes sure the host's Runner runs (`lazybox link up`).
func StartRunner(ctx context.Context, r SSHRoute, service string) (Up, error) {
	out, err := r.Run(ctx, nil, r.Remote, "link", "up", "--service", firstNonEmpty(service, "auto"), "--json")
	var up Up
	if jsonErr := decodeOutput(out, &up); jsonErr != nil {
		if err != nil {
			return Up{}, fmt.Errorf("starting the Runner: %w", err)
		}
		return Up{}, fmt.Errorf("starting the Runner: %q", truncate(string(out), 200))
	}
	if err != nil || up.State != "running" {
		return up, fmt.Errorf("the Runner on %s did not start: %s", r.Host, firstNonEmpty(up.Problem, fmt.Sprint(err)))
	}
	return up, nil
}

// Enrolment is the host's answer to `lazybox link owners add`: the Runner
// key this client pins, and who it is.
type Enrolment struct {
	KeyID       string `json:"keyId"`
	RunnerKey   string `json:"runnerKey"`
	Fingerprint string `json:"fingerprint"`
	RunnerID    string `json:"runnerId"`
	Machine     string `json:"machine"`
	Release     string `json:"release"`
}

// Enrol adds this client's public keys to the host Runner's owners over the selected route
// and returns the Runner key to pin.
func Enrol(ctx context.Context, r OwnerRoute, keys ClientKeys, name string) (Enrolment, error) {
	identity, err := keys.IdentityPublic()
	if err != nil {
		return Enrolment{}, err
	}
	words := r.ownerCommand()
	words = append(words, "link", "owners", "add", "--name", name, "--channel-key", Encode(keys.ChannelPublic()), "--identity-key", Encode(identity), "--via", r.Kind(), "--json")
	out, err := r.Run(ctx, nil, words...)
	if err != nil {
		return Enrolment{}, fmt.Errorf("adding this computer as an owner: %w", err)
	}
	var e Enrolment
	if err = decodeOutput(out, &e); err != nil {
		return Enrolment{}, fmt.Errorf("the host's answer to the enrolment: %q", truncate(string(out), 200))
	}
	key, err := Decode(e.RunnerKey)
	if err != nil || len(key) != 32 {
		return Enrolment{}, errors.New("the host answered without a usable Runner key")
	}
	if e.KeyID != keys.ID() {
		return Enrolment{}, errors.New("the host enrolled another key than this computer's")
	}
	if e.Fingerprint != Fingerprint(key) {
		return Enrolment{}, errors.New("the host's key fingerprint does not match its key")
	}
	return e, nil
}

// Revoke removes this client's key from the host's owners over the selected route.
func Revoke(ctx context.Context, r OwnerRoute, keyID string) error {
	words := r.ownerCommand()
	words = append(words, "link", "owners", "revoke", keyID, "--json")
	_, err := r.Run(ctx, nil, words...)
	return err
}

// stepTimeout bounds one bootstrap step that should be quick.
const stepTimeout = 2 * time.Minute

// StepTimeout is how long a quick bootstrap step (probe, start, enrol) may take.
func StepTimeout() time.Duration { return stepTimeout }
