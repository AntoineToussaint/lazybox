// Package journal provides a bounded append-only, fsynced event spool. Readers
// use independent cursors and wakeups; there is no per-viewer producer queue.
package journal

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"sync"
	"time"
)

var ErrFull = errors.New("journal capacity exhausted")
var ErrCursor = errors.New("cursor outside retained range")

type Record struct {
	Sequence uint64          `json:"sequence"`
	Time     time.Time       `json:"time"`
	Value    json.RawMessage `json:"value"`
}

type Journal struct {
	mu          sync.Mutex
	f           *os.File
	positions   []int64
	size, limit int64
	notify      chan struct{}
	closed      bool
}

func Open(path string, limit int64) (*Journal, error) { return open(path, limit, false) }

// OpenExisting reads retained evidence without creating or changing a file.
func OpenExisting(path string, limit int64) (*Journal, error) { return open(path, limit, true) }
func open(path string, limit int64, readonly bool) (*Journal, error) {
	if limit < 1024 {
		return nil, errors.New("journal budget must be at least 1024 bytes")
	}
	flags := os.O_CREATE | os.O_RDWR
	if readonly {
		flags = os.O_RDONLY
	}
	f, err := os.OpenFile(path, flags, 0600)
	if err != nil {
		return nil, err
	}
	info, err := f.Stat()
	if err != nil {
		f.Close()
		return nil, err
	}
	if info.Size() > limit {
		f.Close()
		return nil, ErrFull
	}
	j := &Journal{f: f, limit: limit, notify: make(chan struct{})}
	r := bufio.NewReader(f)
	for {
		line, e := r.ReadBytes('\n')
		if e == io.EOF && len(line) == 0 {
			break
		}
		if e != nil {
			f.Close()
			return nil, fmt.Errorf("incomplete journal at %d: %w", j.size, e)
		}
		var record Record
		if e = json.Unmarshal(line, &record); e != nil || record.Sequence != uint64(len(j.positions)+1) {
			f.Close()
			return nil, fmt.Errorf("corrupt journal at %d", j.size)
		}
		j.positions = append(j.positions, j.size)
		j.size += int64(len(line))
		if j.size > limit {
			f.Close()
			return nil, ErrFull
		}
	}
	if readonly {
		j.closed = true
		close(j.notify)
	}
	return j, nil
}

func (j *Journal) Append(value any) (Record, error) {
	j.mu.Lock()
	defer j.mu.Unlock()
	if j.closed {
		return Record{}, os.ErrClosed
	}
	b, err := json.Marshal(value)
	if err != nil {
		return Record{}, err
	}
	r := Record{Sequence: uint64(len(j.positions) + 1), Time: time.Now().UTC(), Value: b}
	b, err = json.Marshal(r)
	if err != nil {
		return Record{}, err
	}
	b = append(b, '\n')
	if j.size+int64(len(b)) > j.limit {
		return Record{}, ErrFull
	}
	n, err := j.f.WriteAt(b, j.size)
	if err == nil && n != len(b) {
		err = io.ErrShortWrite
	}
	if err == nil {
		err = j.f.Sync()
	}
	if err != nil {
		j.closed = true
		close(j.notify)
		j.f.Close()
		return Record{}, err
	}
	j.positions = append(j.positions, j.size)
	j.size += int64(n)
	close(j.notify)
	j.notify = make(chan struct{})
	return r, nil
}

// Next returns the record after cursor; cancellation affects this reader only.
func (j *Journal) Next(ctx context.Context, cursor uint64) (Record, error) {
	for {
		j.mu.Lock()
		if cursor > uint64(len(j.positions)) {
			j.mu.Unlock()
			return Record{}, ErrCursor
		}
		if cursor < uint64(len(j.positions)) {
			start := j.positions[cursor]
			end := j.size
			if int(cursor)+1 < len(j.positions) {
				end = j.positions[cursor+1]
			}
			b := make([]byte, end-start)
			_, err := j.f.ReadAt(b, start)
			j.mu.Unlock()
			if err != nil {
				return Record{}, err
			}
			var r Record
			err = json.Unmarshal(b, &r)
			return r, err
		}
		if j.closed {
			j.mu.Unlock()
			return Record{}, io.EOF
		}
		wake := j.notify
		j.mu.Unlock()
		select {
		case <-ctx.Done():
			return Record{}, ctx.Err()
		case <-wake:
		}
	}
}

func (j *Journal) Count() uint64 { j.mu.Lock(); defer j.mu.Unlock(); return uint64(len(j.positions)) }
func (j *Journal) Close() error {
	j.mu.Lock()
	defer j.mu.Unlock()
	if j.closed {
		return nil
	}
	j.closed = true
	close(j.notify)
	// Retain the descriptor until readers have replayed; callers close readers
	// before releasing the journal with Dispose.
	return nil
}
func (j *Journal) Dispose() error { _ = j.Close(); return j.f.Close() }
