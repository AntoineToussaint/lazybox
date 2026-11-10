package sessionhost

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"regexp"
)

// Protocol is the newest host wire version this binary speaks, and the version
// of every host it launches. A host speaks the version of the binary that
// started it for its whole life, so a Runner keeps speaking older versions
// for as long as a supported release could have launched a host with them.
//
// Compatibility window: a wire change bumps Protocol; minProtocol stays at the
// oldest version still spoken. The client reads the host's version from its
// record and says hello in that version, so a host never needs to know newer
// ones. Terminate and release are version-stable, so a host outside the window
// can still be stopped.
const Protocol = 1

// supported is the inclusive range of host versions this client speaks. It is a
// variable only so tests can exercise the window before a second version exists.
var supported = [2]int{1, Protocol}

// Speaks reports whether this binary can adopt a host of the given version.
func Speaks(version int) bool { return version >= supported[0] && version <= supported[1] }

const helperArgument = "--lazybox-session-host-v1"

// ErrIncompatible means the host speaks another wire protocol version.
var ErrIncompatible = errors.New("session host protocol is incompatible with this Runner")

// ErrSuperseded is reported to a client after another client attached to its
// host: the newer Runner owns the program now, so this one must not touch it.
var ErrSuperseded = errors.New("session host was adopted by another Runner")

// ErrDetached is reported by a client that left its host running.
var ErrDetached = errors.New("detached from session host")

var validID = regexp.MustCompile(`^[A-Za-z0-9_-]{8,64}$`)

// Record is the host's private discovery file, written 0600 in a 0700 directory.
// The token is the only credential for the control connection.
type Record struct {
	Protocol int    `json:"protocol"`
	ID       string `json:"id"`
	PID      int    `json:"pid"`
	Port     int    `json:"port"`
	Token    string `json:"token"`
	Proxy    string `json:"proxy"`
}

// Spec is the program the host runs. It travels over the control connection,
// never argv, because Env carries resolved credentials.
type Spec struct {
	Path string   `json:"path"`
	Args []string `json:"args"`
	Env  []string `json:"env"`
	Cwd  string   `json:"cwd"`
	// Stream runs a program whose output is bytes, not JSONL: a managed command.
	// It gets no stdin; every read from its stdout or stderr becomes one Chunk
	// line, sequenced and acknowledged exactly like an agent line.
	Stream bool `json:"stream,omitempty"`
}

// Chunk is one read of a streamed program's output.
type Chunk struct {
	Channel string `json:"channel"`
	Data    []byte `json:"data"`
}

// ChunkSize bounds one Chunk's data.
const ChunkSize = 32 << 10

// Outcome mirrors processguard.Outcome for the agent process.
type Outcome struct {
	Started   bool   `json:"started"`
	ExitCode  int    `json:"exitCode"`
	Signal    string `json:"signal,omitempty"`
	Error     string `json:"error,omitempty"`
	Cancelled bool   `json:"cancelled,omitempty"`
}

// frame is one control message in either direction.
type frame struct {
	Op string `json:"op"`
	// hello
	Token    string `json:"token,omitempty"`
	Ack      uint64 `json:"ack,omitempty"`
	Protocol int    `json:"protocol,omitempty"`
	// welcome
	AgentPID int    `json:"agentPid,omitempty"`
	Proxy    string `json:"proxy,omitempty"`
	Spawned  bool   `json:"spawned,omitempty"`
	Exited   bool   `json:"exited,omitempty"`
	Attached bool   `json:"attached,omitempty"`
	// spawn
	Spec *Spec `json:"spec,omitempty"`
	// send / msg
	Seq         uint64          `json:"seq,omitempty"`
	Line        json.RawMessage `json:"line,omitempty"`
	Redelivered bool            `json:"redelivered,omitempty"`
	// upstream
	URL string `json:"url,omitempty"`
	// exit / error
	Outcome *Outcome `json:"outcome,omitempty"`
	Text    string   `json:"text,omitempty"`
}

func recordPath(dir string) string { return filepath.Join(dir, "host.json") }

// ReadRecord loads a host's discovery record from its directory.
func ReadRecord(dir string) (Record, error) {
	data, err := os.ReadFile(recordPath(dir))
	if err != nil {
		return Record{}, err
	}
	var r Record
	if err = json.Unmarshal(data, &r); err != nil {
		return Record{}, err
	}
	if !validID.MatchString(r.ID) || r.Port <= 0 || r.Token == "" {
		return Record{}, errors.New("invalid session host record")
	}
	return r, nil
}

func writeRecord(dir string, r Record) error {
	data, err := json.Marshal(r)
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(dir, ".host-*")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	if closeErr := f.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	return os.Rename(f.Name(), recordPath(dir))
}

// classify returns the JSON-RPC id (canonical bytes) and whether the line is a
// request (id and method) or a response (id, no method).
func classify(line []byte) (id string, request, response bool) {
	var m struct {
		ID     json.RawMessage `json:"id"`
		Method string          `json:"method"`
	}
	if json.Unmarshal(line, &m) != nil || len(m.ID) == 0 || string(m.ID) == "null" {
		return "", false, false
	}
	return string(m.ID), m.Method != "", m.Method == ""
}
