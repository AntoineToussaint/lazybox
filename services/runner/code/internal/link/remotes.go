package link

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"syscall"
	"time"
)

// Remote is a Runner on another computer this client enrolled with: where to
// reach it and the Runner key it pinned. `<config>/remotes/<name>/remote.json`,
// 0600. It holds no secret; the keys are the KeyProvider's.
type Remote struct {
	Kubernetes *KubernetesTarget `json:"kubernetes,omitempty"`
	Name       string            `json:"name"`
	Route      string            `json:"route"`
	// Host is the ssh destination; Command the lazybox on the host; Runner
	// the host's registered Runner the link serves.
	Host    string `json:"host"`
	Command string `json:"command"`
	Runner  string `json:"runner,omitempty"`
	// RunnerKey is the pinned static key, Fingerprint how a person reads it.
	RunnerKey   string    `json:"runnerKey"`
	Fingerprint string    `json:"fingerprint"`
	RunnerID    string    `json:"runnerId,omitempty"`
	Machine     string    `json:"machine,omitempty"`
	OS          string    `json:"os,omitempty"`
	Arch        string    `json:"arch,omitempty"`
	Release     string    `json:"release,omitempty"`
	KeyID       string    `json:"keyId"`
	AddedAt     time.Time `json:"addedAt"`
}

// PinnedKey is the Runner key to pin.
func (r Remote) PinnedKey() ([]byte, error) {
	key, err := Decode(r.RunnerKey)
	if err != nil || len(key) != 32 {
		return nil, fmt.Errorf("%s has no usable pinned Runner key; add it again", r.Name)
	}
	return key, nil
}

// SSH is the route to it.
func (r Remote) SSH(controlDir string) SSHRoute {
	return SSHRoute{Host: r.Host, Remote: r.Command, Runner: r.Runner, ControlDir: controlDir}
}

// OwnerRoute is the saved transport used for enrollment and reconnect.
func (r Remote) OwnerRoute(controlDir string) (OwnerRoute, error) {
	switch r.Route {
	case "", "ssh":
		return r.SSH(controlDir), nil
	case "kubernetes":
		if r.Kubernetes != nil {
			return KubernetesRoute{Target: *r.Kubernetes}, r.Kubernetes.Validate()
		}
	}
	return nil, fmt.Errorf("%s has an invalid remote route %q", r.Name, r.Route)
}

// LinkStatus is what `lazybox link connect` writes beside the endpoint, for
// the window and `lazybox remote ls`: the link's state and who keeps it.
type LinkStatus struct {
	Status
	PID   int       `json:"pid"`
	Host  string    `json:"host"`
	Route string    `json:"route"`
	At    time.Time `json:"at"`
}

// Remotes is this client's list of remote Runners, in its configuration
// directory (`$XDG_CONFIG_HOME/lazybox` or ~/.config/lazybox). Each also has a
// locator in `runners/<name>.json`, so `lazybox runners`, `--runner NAME`
// and the desktop's Runner list find it like any local Runner.
type Remotes struct{ Config string }

var namePattern = regexp.MustCompile(`^[a-z0-9][a-z0-9._-]{0,47}$`)

// reserved names belong to this computer's own Runners.
var reserved = map[string]bool{"dev": true, "working": true, "lazybox": true, "release": true, "window": true}

// NameFor makes a Runner name from an ssh host: "me@build-box.lan" → "build-box.lan".
func NameFor(host string) string {
	if _, after, ok := strings.Cut(host, "@"); ok {
		host = after
	}
	host = strings.Trim(strings.ToLower(host), "[]")
	var b strings.Builder
	for _, r := range host {
		switch {
		case r >= 'a' && r <= 'z', r >= '0' && r <= '9', r == '.', r == '-', r == '_':
			b.WriteRune(r)
		default:
			b.WriteRune('-')
		}
	}
	name := strings.Trim(b.String(), "-.")
	if len(name) > 48 {
		name = name[:48]
	}
	return name
}

// CheckName refuses a name that is not a file name or is one of this
// computer's own Runners'.
func CheckName(name string) error {
	if !namePattern.MatchString(name) {
		return fmt.Errorf("%q cannot name a Runner: use lowercase letters, digits, dots and dashes", name)
	}
	if reserved[name] {
		return fmt.Errorf("%q is the name of one of this computer's own Runners; choose another with --name", name)
	}
	return nil
}

func (s Remotes) Dir(name string) string      { return filepath.Join(s.Config, "remotes", name) }
func (s Remotes) Endpoint(name string) string { return filepath.Join(s.Dir(name), "endpoint.json") }
func (s Remotes) StatusFile(name string) string {
	return filepath.Join(s.Dir(name), "status.json")
}
func (s Remotes) Locator(name string) string { return filepath.Join(s.Config, "runners", name+".json") }

// ControlDir holds ssh's ControlMaster sockets: short, private.
func (s Remotes) ControlDir() string { return filepath.Join(s.Config, "ssh") }

// Keys is this client's key file.
func (s Remotes) Keys() FileKeys {
	return FileKeys{Path: filepath.Join(s.Config, "keys", "link-client.json")}
}

// Save writes the record and its locator.
func (s Remotes) Save(r Remote) error {
	if err := CheckName(r.Name); err != nil {
		return err
	}
	if err := writePrivateJSON(filepath.Join(s.Dir(r.Name), "remote.json"), r); err != nil {
		return err
	}
	locator := map[string]any{
		"name": r.Name, "product": "Lazybox", "endpoint": s.Endpoint(r.Name),
		"remote": map[string]string{"host": r.Host, "route": r.Route, "machine": firstNonEmpty(r.Machine, r.Host), "fingerprint": r.Fingerprint, "os": r.OS},
	}
	return writePrivateJSON(s.Locator(r.Name), locator)
}

// Load reads one record.
func (s Remotes) Load(name string) (Remote, error) {
	if err := CheckName(name); err != nil {
		return Remote{}, err
	}
	data, err := os.ReadFile(filepath.Join(s.Dir(name), "remote.json"))
	if errors.Is(err, fs.ErrNotExist) {
		return Remote{}, fmt.Errorf("no computer named %q was added (lazybox remote ls)", name)
	}
	if err != nil {
		return Remote{}, err
	}
	var r Remote
	if err = json.Unmarshal(data, &r); err != nil {
		return Remote{}, fmt.Errorf("%s: %w", name, err)
	}
	return r, nil
}

// List reads every record, by name.
func (s Remotes) List() ([]Remote, error) {
	entries, err := os.ReadDir(filepath.Join(s.Config, "remotes"))
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var out []Remote
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		if r, err := s.Load(e.Name()); err == nil {
			out = append(out, r)
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out, nil
}

// Remove forgets a remote here: its record, locator, endpoint and status.
func (s Remotes) Remove(name string) error {
	if err := CheckName(name); err != nil {
		return err
	}
	return errors.Join(ignoreMissing(os.Remove(s.Locator(name))), os.RemoveAll(s.Dir(name)))
}

// Status reads the last status the link agent wrote, and whether it still runs.
func (s Remotes) Status(name string) (LinkStatus, bool) {
	data, err := os.ReadFile(s.StatusFile(name))
	if err != nil {
		return LinkStatus{}, false
	}
	var st LinkStatus
	if json.Unmarshal(data, &st) != nil {
		return LinkStatus{}, false
	}
	return st, st.PID > 0 && processAlive(st.PID) && st.State != "stopped"
}

// WriteEndpoint writes the loopback origin and token the agent serves, in
// the shape of a Runner's endpoint.json.
func (s Remotes) WriteEndpoint(name, url, token, runnerID string) error {
	value := map[string]string{"url": url, "token": token}
	if validRunnerID(runnerID) {
		value["runnerId"] = runnerID
	}
	return writePrivateJSON(s.Endpoint(name), value)
}

// WriteStatus records the agent's status.
func (s Remotes) WriteStatus(name string, st LinkStatus) error {
	return writePrivateJSON(s.StatusFile(name), st)
}

func validRunnerID(id string) bool {
	if len(id) < 16 || len(id) > 128 {
		return false
	}
	for _, c := range id {
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9') {
			return false
		}
	}
	return true
}

func ignoreMissing(err error) error {
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	return err
}

// processAlive reports whether pid names a running process.
func processAlive(pid int) bool {
	err := syscall.Kill(pid, 0)
	return err == nil || errors.Is(err, syscall.EPERM)
}

// writePrivateJSON writes v atomically, 0600, in a 0700 directory.
func writePrivateJSON(path string, v any) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(path), ".tmp-*")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	_, err = f.Write(append(data, '\n'))
	if err = errors.Join(err, f.Chmod(0o600), f.Close()); err != nil {
		return err
	}
	return os.Rename(f.Name(), path)
}
