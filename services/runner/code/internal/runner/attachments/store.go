// Package attachments retains generic message files under Runner ownership.
package attachments

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"unicode"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

const MaxFiles = 4
const MaxFileBytes = 20 << 20
const MaxMessageBytes = 20 << 20
const SessionBudget = 64 << 20
const SessionFiles = 256

type Input struct {
	Name, MediaType string
	Data            []byte
}
type Store struct {
	mu       sync.Mutex
	root     string
	sessions map[string]map[string]int64
}

func New(root string) (*Store, error) {
	if !filepath.IsAbs(root) {
		return nil, errors.New("absolute attachment root required")
	}
	if err := os.MkdirAll(root, 0700); err != nil {
		return nil, err
	}
	return &Store{root: root, sessions: map[string]map[string]int64{}}, nil
}

func validName(name string) bool {
	return name != "" && name != "." && name != ".." && len(name) <= 255 && !strings.ContainsAny(name, "/\\") && !strings.ContainsFunc(name, unicode.IsControl)
}

// Put saves a complete validated batch, deduplicating retries by content/name.
// Counts and budgets are scoped to an execution and never depend on a viewer.
func (s *Store) Put(session string, inputs []Input) ([]agent.File, error) {
	if !validName(session) {
		return nil, errors.New("invalid execution identity")
	}
	if len(inputs) > MaxFiles {
		return nil, errors.New("at most four attachments per message")
	}
	var size int
	result := make([]agent.File, 0, len(inputs))
	for _, input := range inputs {
		size += len(input.Data)
		if !validName(input.Name) || len(input.Data) > MaxFileBytes || size > MaxMessageBytes || len(input.MediaType) > 255 || strings.ContainsFunc(input.MediaType, unicode.IsControl) {
			return nil, errors.New("invalid attachment name, type, or size (20 MiB per file/message)")
		}
		digest := sha256.Sum256(input.Data)
		mediaType := input.MediaType
		if mediaType == "" {
			mediaType = http.DetectContentType(input.Data)
		}
		result = append(result, agent.File{Name: input.Name, MediaType: mediaType, Size: int64(len(input.Data)), Path: filepath.Join(s.root, session, hex.EncodeToString(digest[:]), input.Name)})
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	known := s.sessions[session]
	var total int64
	for _, size := range known {
		total += size
	}
	pending := map[string]int64{}
	for _, file := range result {
		if _, exists := known[file.Path]; !exists {
			pending[file.Path] = file.Size
		}
	}
	for _, size := range pending {
		total += size
	}
	if total > SessionBudget || len(known)+len(pending) > SessionFiles {
		return nil, errors.New("attachment retention budget exhausted (64 MiB / 256 files per execution)")
	}
	created := []string{}
	rollback := func(err error) ([]agent.File, error) {
		for _, path := range created {
			_ = os.Remove(path)
		}
		for _, path := range created {
			_ = os.Remove(filepath.Dir(path))
		}
		return nil, err
	}
	for i, file := range result {
		if _, exists := known[file.Path]; exists {
			continue
		}
		if _, exists := pending[file.Path]; !exists {
			continue
		}
		if err := os.MkdirAll(filepath.Dir(file.Path), 0700); err != nil {
			return rollback(err)
		}
		f, err := os.OpenFile(file.Path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0600)
		if err != nil {
			return rollback(fmt.Errorf("retain attachment: %w", err))
		}
		created = append(created, file.Path)
		_, writeErr := f.Write(inputs[i].Data)
		if writeErr == nil {
			writeErr = f.Sync()
		}
		closeErr := f.Close()
		if writeErr != nil {
			return rollback(writeErr)
		}
		if closeErr != nil {
			return rollback(closeErr)
		}
		delete(pending, file.Path)
	}
	if known == nil {
		known = map[string]int64{}
		s.sessions[session] = known
	}
	for _, file := range result {
		known[file.Path] = file.Size
	}
	return result, nil
}

// Delete removes only uploads owned by this execution, never workspace files.
func (s *Store) Delete(session string) error {
	if !validName(session) {
		return errors.New("invalid execution identity")
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if err := os.RemoveAll(filepath.Join(s.root, session)); err != nil {
		return err
	}
	delete(s.sessions, session)
	return nil
}

// ErrNotPreviewable is a retained file that is neither an image a browser
// displays nor text.
var ErrNotPreviewable = errors.New("only PNG, JPEG, GIF and WebP images and text attachments can be previewed")

// TextMediaType is the type a previewed text file is served as, whatever its
// bytes resemble (HTML, SVG): a client shows it as text, never renders it.
const TextMediaType = "text/plain; charset=utf-8"

// ErrNotAttachment is a path that is not one this store retained.
var ErrNotAttachment = errors.New("not a retained message attachment")

// ErrTooLarge is a retained file over the per-file bound, which Put never
// writes: something else changed the store.
var ErrTooLarge = fmt.Errorf("attachment is larger than %d MiB", MaxFileBytes>>20)

// Previewable is the image types a conversation shows from their bytes. SVG is
// left out on purpose: it is a document that can carry script.
func Previewable(mediaType string) bool {
	switch mediaType {
	case "image/png", "image/jpeg", "image/gif", "image/webp":
		return true
	}
	return false
}

// Read returns a retained attachment's bytes for display, when path is
// exactly one Put made (root/<execution>/<sha256>/<name>), nothing on the way
// links out of the store, the file is a regular file of at most MaxFileBytes
// whose content still has that digest, and the bytes are an image a browser
// displays or text (pasted text, a source file: valid UTF-8 without NUL). The
// media type is detected from the bytes, never taken from the sender, and
// text is always TextMediaType. Retention bookkeeping is not consulted, so attachments sent before
// a Runner restart stay readable until their conversation is deleted.
func (s *Store) Read(path string) (agent.File, []byte, error) {
	if !filepath.IsAbs(path) || filepath.Clean(path) != path {
		return agent.File{}, nil, ErrNotAttachment
	}
	rel, err := filepath.Rel(s.root, path)
	if err != nil {
		return agent.File{}, nil, ErrNotAttachment
	}
	parts := strings.Split(rel, string(filepath.Separator))
	if len(parts) != 3 || !validName(parts[0]) || !isDigest(parts[1]) || !validName(parts[2]) {
		return agent.File{}, nil, ErrNotAttachment
	}
	root, err := filepath.EvalSymlinks(s.root)
	if err != nil {
		return agent.File{}, nil, err
	}
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil {
		return agent.File{}, nil, err
	}
	if resolved != filepath.Join(root, rel) {
		return agent.File{}, nil, ErrNotAttachment
	}
	f, err := os.Open(resolved)
	if err != nil {
		return agent.File{}, nil, err
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil {
		return agent.File{}, nil, err
	}
	if !info.Mode().IsRegular() {
		return agent.File{}, nil, ErrNotAttachment
	}
	if info.Size() > MaxFileBytes {
		return agent.File{}, nil, ErrTooLarge
	}
	data, err := io.ReadAll(io.LimitReader(f, MaxFileBytes+1))
	if err != nil {
		return agent.File{}, nil, err
	}
	if len(data) > MaxFileBytes {
		return agent.File{}, nil, ErrTooLarge
	}
	if digest := sha256.Sum256(data); hex.EncodeToString(digest[:]) != parts[1] {
		return agent.File{}, nil, errors.New("attachment changed on disk since it was sent")
	}
	mediaType := http.DetectContentType(data)
	switch {
	case Previewable(mediaType):
	case agent.IsText(data):
		mediaType = TextMediaType
	default:
		return agent.File{}, nil, ErrNotPreviewable
	}
	return agent.File{Name: parts[2], MediaType: mediaType, Path: path, Size: int64(len(data))}, data, nil
}

func isDigest(value string) bool {
	if len(value) != sha256.Size*2 {
		return false
	}
	for _, c := range value {
		if (c < '0' || c > '9') && (c < 'a' || c > 'f') {
			return false
		}
	}
	return true
}
