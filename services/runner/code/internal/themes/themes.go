// Package themes is the user's themes folder for Settings → Appearance: .json
// and .toml files the Runner parses to JSON on every read, so an edit applies
// at the client's next poll without a restart. It knows nothing about tokens:
// the client validates each document against its own token catalogue. It
// keeps no state; the folder is the only record.
//
//	folder, err := themes.List(dir)        // every theme, parsed; folder.Revision
//	file, path, err := themes.Create(dir, "Dusk", tomlText)
package themes

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"math"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/BurntSushi/toml"
)

const (
	// MaxFiles is how many theme files are listed; the rest, by name, are ignored.
	MaxFiles = 64
	// MaxFileBytes bounds one theme file; a larger one is listed with an error.
	MaxFileBytes = 64 << 10
	// MaxNameLength bounds a theme's base name (its file name without extension).
	MaxNameLength = 64
)

var (
	// ErrExists is returned by Create when the theme file already exists.
	ErrExists = errors.New("a theme with that name already exists")
	// ErrInvalid wraps every refusal of a name or content Create was given.
	ErrInvalid = errors.New("invalid theme")
	// ErrFull is returned by Create when the folder already lists MaxFiles themes.
	ErrFull = fmt.Errorf("the themes folder already holds %d themes", MaxFiles)
)

// File is one theme file as List reports it.
type File struct {
	// Name is the file name, extension included.
	Name string
	// Format is "json" or "toml".
	Format string
	// JSON is the parsed document as compact JSON; empty when Error is set.
	JSON string
	// Error says why the file cannot be used: a parse error with its line
	// (and column when known), or a file over MaxFileBytes.
	Error    string
	Size     int64
	Modified time.Time
}

// Folder is the themes folder: its files sorted by name, and a revision that
// changes whenever a file is added, removed or edited.
type Folder struct {
	Directory string
	Files     []File
	Revision  string
}

// List reads every theme file in dir. A missing dir is an empty folder, not
// an error. Only regular files count (never a link or a subdirectory), hidden
// files and names outside the theme name charset are skipped, and at most
// MaxFiles are listed. A file that cannot be parsed is listed with its error.
func List(dir string) (Folder, error) {
	abs, err := filepath.Abs(dir)
	if err != nil {
		return Folder{}, err
	}
	out := Folder{Directory: abs, Files: []File{}}
	root, err := os.OpenRoot(abs)
	if errors.Is(err, fs.ErrNotExist) {
		out.Revision = revision(nil)
		return out, nil
	}
	if err != nil {
		return Folder{}, err
	}
	defer root.Close()
	entries, err := fs.ReadDir(root.FS(), ".") // sorted by name
	if err != nil {
		return Folder{}, err
	}
	for _, entry := range entries {
		if len(out.Files) == MaxFiles {
			break
		}
		name := entry.Name()
		format, ok := formatOf(name)
		if !ok || validName(strings.TrimSuffix(name, filepath.Ext(name))) != nil {
			continue
		}
		info, err := root.Lstat(name)
		if err != nil || !info.Mode().IsRegular() {
			continue // removed since the listing, a link, or not a file
		}
		f := File{Name: name, Format: format, Size: info.Size(), Modified: info.ModTime()}
		data, err := readRegular(root, name, info)
		switch {
		case errors.Is(err, errSkip):
			continue
		case err != nil:
			f.Error = err.Error()
		default:
			f.JSON, err = parse(format, data)
			if err != nil {
				f.Error = err.Error()
			}
		}
		out.Files = append(out.Files, f)
	}
	out.Revision = revision(out.Files)
	return out, nil
}

// Create writes a new theme file <name>.toml in dir, creating dir (0700) when
// missing. It never replaces a file: an existing one is ErrExists. The name
// and content are checked first (ErrInvalid): the content must parse as TOML
// and fit MaxFileBytes. It returns the file name and its absolute path.
func Create(dir, name, content string) (file, path string, err error) {
	if err = validName(name); err != nil {
		return "", "", fmt.Errorf("%w: %v", ErrInvalid, err)
	}
	if len(content) > MaxFileBytes {
		return "", "", fmt.Errorf("%w: a theme is at most %d KiB", ErrInvalid, MaxFileBytes>>10)
	}
	if _, err = parse("toml", []byte(content)); err != nil {
		return "", "", fmt.Errorf("%w: %v", ErrInvalid, err)
	}
	abs, err := filepath.Abs(dir)
	if err != nil {
		return "", "", err
	}
	folder, err := List(abs)
	if err != nil {
		return "", "", err
	}
	if len(folder.Files) >= MaxFiles {
		return "", "", ErrFull
	}
	if err = os.MkdirAll(abs, 0o700); err != nil {
		return "", "", err
	}
	file = name + ".toml"
	path = filepath.Join(abs, file)
	// O_EXCL also refuses a link at path, whatever it points to.
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if errors.Is(err, fs.ErrExist) {
		return "", "", fmt.Errorf("%w: %s", ErrExists, file)
	}
	if err != nil {
		return "", "", err
	}
	_, err = f.WriteString(content)
	if closeErr := f.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		_ = os.Remove(path)
		return "", "", err
	}
	return file, path, nil
}

// errSkip marks a file that changed into something List does not read
// between its listing and its opening.
var errSkip = errors.New("not a regular file")

// readRegular reads name only when it is still the regular file listed, at
// most one byte over MaxFileBytes so an oversized file is never read whole.
func readRegular(root *os.Root, name string, listed fs.FileInfo) ([]byte, error) {
	if listed.Size() > MaxFileBytes {
		return nil, fmt.Errorf("larger than %d KiB", MaxFileBytes>>10)
	}
	f, err := root.Open(name)
	if err != nil {
		return nil, errSkip
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil || !info.Mode().IsRegular() || !os.SameFile(info, listed) {
		return nil, errSkip
	}
	data, err := io.ReadAll(io.LimitReader(f, MaxFileBytes+1))
	if err != nil {
		return nil, err
	}
	if len(data) > MaxFileBytes {
		return nil, fmt.Errorf("larger than %d KiB", MaxFileBytes>>10)
	}
	return data, nil
}

// formatOf is a theme file's format from its extension, in any case.
func formatOf(name string) (string, bool) {
	switch strings.ToLower(filepath.Ext(name)) {
	case ".json":
		return "json", true
	case ".toml":
		return "toml", true
	}
	return "", false
}

// validName admits 1 to MaxNameLength letters, digits, spaces, '.', '_' and
// '-', not starting with '.'.
func validName(name string) error {
	if name == "" || len(name) > MaxNameLength {
		return fmt.Errorf("a theme name is 1 to %d characters", MaxNameLength)
	}
	if name[0] == '.' {
		return errors.New("a theme name cannot start with '.'")
	}
	for _, r := range name {
		if !(r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' || r == ' ' || r == '.' || r == '_' || r == '-') {
			return fmt.Errorf("a theme name uses letters, digits, spaces, '.', '_' and '-' only, not %q", r)
		}
	}
	return nil
}

// parse turns a theme document into compact JSON. It must be one object.
func parse(format string, data []byte) (string, error) {
	var doc map[string]any
	var err error
	if format == "json" {
		doc, err = parseJSON(data)
	} else {
		doc, err = parseTOML(data)
	}
	if err != nil {
		return "", err
	}
	var b bytes.Buffer
	enc := json.NewEncoder(&b)
	enc.SetEscapeHTML(false)
	if err = enc.Encode(doc); err != nil {
		return "", err
	}
	return strings.TrimSuffix(b.String(), "\n"), nil
}

func parseJSON(data []byte) (map[string]any, error) {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.UseNumber() // numbers are re-marshalled exactly as written
	var doc any
	if err := dec.Decode(&doc); err != nil {
		var syntax *json.SyntaxError
		if errors.As(err, &syntax) {
			return nil, fmt.Errorf("%s: %s", position(data, syntax.Offset), syntax.Error())
		}
		if errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) {
			return nil, fmt.Errorf("%s: unexpected end of JSON input", position(data, int64(len(data))))
		}
		return nil, err
	}
	object, ok := doc.(map[string]any)
	if !ok {
		return nil, fmt.Errorf("%s: a theme is one JSON object", position(data, nextByte(data, 0)))
	}
	end := dec.InputOffset()
	if _, err := dec.Token(); !errors.Is(err, io.EOF) {
		return nil, fmt.Errorf("%s: unexpected content after the theme object", position(data, nextByte(data, end)))
	}
	return object, nil
}

// nextByte is the offset just past the first non-space byte at or after
// from, the way position expects an offending byte.
func nextByte(data []byte, from int64) int64 {
	rest := data[from:]
	return from + int64(len(rest)-len(bytes.TrimLeft(rest, " \t\r\n"))) + 1
}

// position is "line L, column C" of the byte just before offset, as
// encoding/json reports an error once it has read the offending byte.
func position(data []byte, offset int64) string {
	pos := int(offset) - 1
	if pos < 0 {
		pos = 0
	}
	if pos > len(data) {
		pos = len(data)
	}
	prefix := data[:pos]
	line := bytes.Count(prefix, []byte("\n")) + 1
	column := pos - bytes.LastIndexByte(prefix, '\n')
	return fmt.Sprintf("line %d, column %d", line, column)
}

func parseTOML(data []byte) (map[string]any, error) {
	doc := map[string]any{}
	if _, err := toml.Decode(string(data), &doc); err != nil {
		var pe toml.ParseError
		if errors.As(err, &pe) {
			if pe.Position.Col > 0 {
				return nil, fmt.Errorf("line %d, column %d: %s", pe.Position.Line, pe.Position.Col, pe.Message)
			}
			return nil, fmt.Errorf("line %d: %s", pe.Position.Line, pe.Message)
		}
		return nil, err
	}
	return normalize(doc).(map[string]any), nil
}

// normalize makes TOML values JSON can carry as they were written: a local
// date, time or datetime stays local (never gains the Runner's offset), an
// offset datetime is RFC 3339, and nan and inf, which JSON has no number for,
// are the strings TOML spells them with.
func normalize(v any) any {
	switch x := v.(type) {
	case map[string]any:
		for k, value := range x {
			x[k] = normalize(value)
		}
		return x
	case []map[string]any:
		out := make([]any, len(x))
		for i, value := range x {
			out[i] = normalize(value)
		}
		return out
	case []any:
		for i, value := range x {
			x[i] = normalize(value)
		}
		return x
	case time.Time:
		switch x.Location().String() {
		case "date-local":
			return x.Format(time.DateOnly)
		case "time-local":
			return x.Format("15:04:05.999999999")
		case "datetime-local":
			return x.Format("2006-01-02T15:04:05.999999999")
		}
		return x.Format(time.RFC3339Nano)
	case float64:
		switch {
		case math.IsNaN(x):
			return "nan"
		case math.IsInf(x, 1):
			return "inf"
		case math.IsInf(x, -1):
			return "-inf"
		}
	}
	return v
}

// revision is a short sha256 over every listed file's name, size and
// modification time: it changes whenever one is added, removed or edited.
func revision(files []File) string {
	h := sha256.New()
	for _, f := range files {
		fmt.Fprintf(h, "%s\x00%d\x00%d\n", f.Name, f.Size, f.Modified.UnixNano())
	}
	return hex.EncodeToString(h.Sum(nil))[:16]
}
