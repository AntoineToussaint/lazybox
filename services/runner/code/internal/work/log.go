package work

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/atomicfile"
)

// file is one append-only JSONL log: one event per line, written and fsynced
// before an append returns. A line without its newline was never
// acknowledged (the Runner stopped mid-write); opening truncates it.
type file struct {
	f    *os.File
	size int64
	// limit bounds the file; an append past it is refused.
	limit int64
	// broken is set when a failed append could not be undone: bytes past
	// size may be on disk, so nothing more is written until a rewrite
	// replaces the file.
	broken error
	// unsyncedDir is set when a rewrite's rename landed but syncing its
	// directory failed. The rewrite stands (the file is the log now); the
	// next append syncs the directory first, so nothing more is acknowledged
	// until the rename is durable.
	unsyncedDir string
}

// openFile reads every complete event of a log and leaves it open for appending.
func openFile(path string, limit int64) (*file, []Event, error) {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, nil, err
	}
	var events []Event
	reader := bufio.NewReaderSize(f, 64<<10)
	var size int64
	for {
		line, err := reader.ReadBytes('\n')
		if errors.Is(err, io.EOF) {
			// A torn last line: never acknowledged, so it is not an event.
			if len(line) > 0 {
				if err := f.Truncate(size); err != nil {
					f.Close()
					return nil, nil, err
				}
			}
			break
		}
		if err != nil {
			f.Close()
			return nil, nil, err
		}
		var e Event
		if err := json.Unmarshal(bytes.TrimSpace(line), &e); err != nil {
			f.Close()
			return nil, nil, fmt.Errorf("corrupt event at byte %d of %s: %w", size, path, err)
		}
		events = append(events, e)
		size += int64(len(line))
	}
	return &file{f: f, size: size, limit: limit}, events, nil
}

// ErrFull: the log reached its bound.
var ErrFull = errors.New("the log is full")

// append writes events as one write and one fsync.
func (l *file) append(events []Event) error {
	if l.broken != nil {
		return l.broken
	}
	if l.unsyncedDir != "" {
		if err := atomicfile.SyncDir(l.unsyncedDir); err != nil {
			return fmt.Errorf("the log's last rewrite is not durable yet: %w", err)
		}
		l.unsyncedDir = ""
	}
	var buffer bytes.Buffer
	for _, e := range events {
		line, err := json.Marshal(e)
		if err != nil {
			return err
		}
		buffer.Write(line)
		buffer.WriteByte('\n')
	}
	if l.limit > 0 && l.size+int64(buffer.Len()) > l.limit {
		return ErrFull
	}
	n, err := l.f.WriteAt(buffer.Bytes(), l.size)
	if err == nil && n != buffer.Len() {
		err = io.ErrShortWrite
	}
	if err == nil {
		err = l.f.Sync()
	}
	if err != nil {
		// Cut what may have reached the disk, so a later append cannot
		// complete a torn line into an event nobody acknowledged.
		if truncErr := l.f.Truncate(l.size); truncErr != nil {
			l.broken = fmt.Errorf("the log could not be repaired after a failed write, so nothing more is written to it: %w", errors.Join(err, truncErr))
			return l.broken
		}
		return err
	}
	l.size += int64(n)
	return nil
}

// rewrite replaces the log with events (compaction of the prompt log).
func (l *file) rewrite(path string, events []Event) error {
	tmp := path + ".tmp"
	var buffer bytes.Buffer
	for _, e := range events {
		line, err := json.Marshal(e)
		if err != nil {
			return err
		}
		buffer.Write(line)
		buffer.WriteByte('\n')
	}
	if err := os.WriteFile(tmp, buffer.Bytes(), 0o600); err != nil {
		return err
	}
	f, err := os.OpenFile(tmp, os.O_RDWR, 0o600)
	if err != nil {
		return err
	}
	if err = f.Sync(); err == nil {
		err = os.Rename(tmp, path)
	}
	if err != nil {
		f.Close()
		return err
	}
	// The new file is the log now, so the rewrite has happened and its caller
	// keeps its result; only the rename's durability may still be owed, and
	// the next append pays it before acknowledging anything.
	_ = l.f.Close()
	l.f, l.size, l.broken, l.unsyncedDir = f, int64(buffer.Len()), nil, ""
	if err = atomicfile.SyncDir(filepath.Dir(path)); err != nil {
		l.unsyncedDir = filepath.Dir(path)
	}
	return nil
}

func (l *file) close() error { return l.f.Close() }

// newID is a ULID-shaped identifier: 48 bits of milliseconds and 80 random
// bits in Crockford base32, so IDs sort by creation time.
func newID(at time.Time) string {
	const alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
	var raw [16]byte
	ms := uint64(at.UnixMilli())
	for i := 5; i >= 0; i-- {
		raw[i] = byte(ms)
		ms >>= 8
	}
	_, _ = rand.Read(raw[6:])
	var out strings.Builder
	// 128 bits read as 130 with two leading zero bits: 26 characters.
	acc, bits := uint64(0), 2
	for _, b := range raw {
		acc = acc<<8 | uint64(b)
		bits += 8
		for bits >= 5 {
			bits -= 5
			out.WriteByte(alphabet[(acc>>uint(bits))&31])
		}
	}
	return out.String()
}

// shortID is a readable identifier with a kind prefix: m_3f9a2c1b.
func shortID(prefix string) string {
	var raw [4]byte
	_, _ = rand.Read(raw[:])
	return fmt.Sprintf("%s_%x", prefix, raw)
}
