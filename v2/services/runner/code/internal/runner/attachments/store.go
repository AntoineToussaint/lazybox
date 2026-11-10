// Package attachments retains generic message files under Runner ownership.
package attachments

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"unicode"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
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
