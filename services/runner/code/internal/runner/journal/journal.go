// Package journal provides a bounded append-only, fsynced event spool. Readers
// use independent cursors and wakeups; there is no per-viewer producer queue.
//
// A journal is one file, or, opened with Rollover, a chain of segment files
// read as one: the file at its path is segment 0 and `<path>.1`, `<path>.2`…
// follow it. Each segment holds at most the journal's segment budget (a
// record larger than that has a segment of its own); when the next record
// does not fit, the journal starts the next segment. Sequence
// numbers, byte positions (Span, Fit) and turn marks run across segments, so
// a reader never sees where one ends. A journal written before segments
// existed is segment 0 of its chain.
//
// A record whose line is incomplete (the process or the machine stopped part
// way through writing it, or the disk filled) was never acknowledged. Opening
// the journal for writing truncates it, keeping its bytes beside the segment
// in `<segment>.torn`; opening it read-only reads up to it. A complete record
// that does not read is damage: opening for writing refuses it, and
// OpenExisting with Salvage reads up to it.
package journal

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
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

// segment is one file of the journal: its bytes are the journal's from start.
type segment struct {
	f     *os.File
	path  string
	start int64 // the journal position of its first byte
	size  int64 // the bytes of its complete records
}

type Journal struct {
	mu        sync.Mutex
	path      string
	segments  []segment
	positions []int64 // where each record starts, across segments
	size      int64   // the bytes of every segment's complete records
	limit     int64   // a segment's budget (the whole journal's without Rollover)
	rollover  bool
	salvage   bool
	total     int64 // with Rollover, the budget of every segment together; 0: none
	// reserve is the end of the budget only AppendReserved may use.
	reserve int64
	notify  chan struct{}
	closed  bool
	// broken is a write that could not be undone: nothing more is appended.
	broken error
	// damage is why a read-only open stopped before the end of the journal.
	damage error
	// The records Marking selects, by sequence, ascending.
	mark  func(json.RawMessage) bool
	marks []uint64
}

// Option configures a journal as it opens.
type Option func(*Journal)

// Marking indexes the records whose value mark selects (a conversation's turn
// starts) as the journal is read at open and as records are appended, so a
// reader finds them without parsing the file. mark sees each value once.
func Marking(mark func(json.RawMessage) bool) Option {
	return func(j *Journal) { j.mark = mark }
}

// Rollover makes the journal a chain of segments, each within the budget
// Open is given (a record larger than that has a segment of its own),
// instead of one file that is full at that budget. total
// bounds every segment together (0: no bound); an append past it is ErrFull.
func Rollover(total int64) Option {
	return func(j *Journal) { j.rollover, j.total = true, total }
}

// Reserve keeps the last n bytes of the journal's budget (its total with
// Rollover) for AppendReserved: an ordinary Append that would reach into them
// is ErrFull. The records that close a journal, such as a command's
// completion, then still fit once its output has filled it.
func Reserve(n int64) Option {
	return func(j *Journal) { j.reserve = n }
}

// Salvage makes OpenExisting read a journal with a damaged record up to it,
// instead of refusing it; Damage says why it stopped. Opening for writing
// still refuses it: nothing may be appended after a damaged record.
func Salvage() Option {
	return func(j *Journal) { j.salvage = true }
}

func Open(path string, limit int64, options ...Option) (*Journal, error) {
	return open(path, limit, false, options)
}

// OpenExisting reads retained evidence without creating or changing a file.
// It reads up to a last record whose write never finished; with Salvage, up
// to the first record it cannot read.
func OpenExisting(path string, limit int64, options ...Option) (*Journal, error) {
	return open(path, limit, true, options)
}

// Unreadable is an empty, closed journal standing for one that could not be
// opened at all: a reader sees no records, and Damage returns err.
func Unreadable(err error) *Journal {
	j := &Journal{notify: make(chan struct{}), closed: true, damage: err}
	close(j.notify)
	return j
}

func open(path string, limit int64, readonly bool, options []Option) (*Journal, error) {
	if limit < 1024 {
		return nil, errors.New("journal budget must be at least 1024 bytes")
	}
	j := &Journal{path: path, limit: limit, notify: make(chan struct{})}
	for _, option := range options {
		option(j)
	}
	if j.rollover && j.total != 0 && j.total < limit {
		return nil, errors.New("a journal's total budget must be at least its segment budget")
	}
	if budget := j.budget(); j.reserve < 0 || (j.reserve > 0 && j.reserve >= budget) {
		return nil, errors.New("a journal's reserve must be less than its budget")
	}
	paths := []string{path}
	if j.rollover {
		var err error
		if paths, err = Files(path); err != nil {
			return nil, err
		}
		if len(paths) == 0 {
			paths = []string{path}
		}
	}
	for i, p := range paths {
		last := i == len(paths)-1
		stop, err := j.load(p, readonly, last)
		if err != nil {
			j.release()
			return nil, err
		}
		if stop {
			break
		}
	}
	if readonly {
		j.closed = true
		close(j.notify)
	}
	return j, nil
}

// load reads one segment into the journal. It reports stop when a read-only
// open found damage there: nothing after it is read.
func (j *Journal) load(path string, readonly, last bool) (stop bool, err error) {
	flags := os.O_CREATE | os.O_RDWR
	if readonly {
		flags = os.O_RDONLY
	}
	f, err := os.OpenFile(path, flags, 0600)
	if err != nil {
		return false, err
	}
	info, err := f.Stat()
	if err != nil {
		f.Close()
		return false, err
	}
	if !j.rollover && info.Size() > j.limit {
		f.Close()
		return false, ErrFull
	}
	s := segment{f: f, path: path, start: j.size}
	// A record that does not read: refused when writing (nothing may follow
	// it), the end of what is read otherwise.
	damaged := func(err error) (bool, error) {
		if !readonly || !j.salvage {
			f.Close()
			return false, err
		}
		j.damage = err
		j.segments = append(j.segments, s)
		return true, nil
	}
	r := bufio.NewReader(f)
	for {
		line, e := r.ReadBytes('\n')
		if e == io.EOF && len(line) == 0 {
			break
		}
		if e == io.EOF {
			// The last line is incomplete: its write never finished.
			if !last {
				return damaged(fmt.Errorf("incomplete journal at %d of %s", s.size, path))
			}
			if !readonly {
				if err = tear(f, path, s.size, line); err != nil {
					f.Close()
					return false, fmt.Errorf("recover incomplete journal record at %d of %s: %w", s.size, path, err)
				}
			}
			break
		}
		if e != nil {
			f.Close()
			return false, fmt.Errorf("read journal at %d of %s: %w", s.size, path, e)
		}
		var record Record
		if e = json.Unmarshal(line, &record); e != nil || record.Sequence != uint64(len(j.positions)+1) {
			return damaged(fmt.Errorf("corrupt journal at %d of %s", s.size, path))
		}
		j.positions = append(j.positions, j.size)
		j.size += int64(len(line))
		s.size += int64(len(line))
		if j.mark != nil && j.mark(record.Value) {
			j.marks = append(j.marks, record.Sequence)
		}
		if !j.rollover && j.size > j.limit {
			f.Close()
			return false, ErrFull
		}
	}
	j.segments = append(j.segments, s)
	return false, nil
}

// tear removes an incomplete last record from a segment, keeping its bytes
// in `<segment>.torn` for diagnosis.
func tear(f *os.File, path string, at int64, partial []byte) error {
	torn, err := os.OpenFile(path+".torn", os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0600)
	if err != nil {
		return err
	}
	if _, err = torn.Write(partial); err == nil {
		err = torn.Sync()
	}
	if closeErr := torn.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	if err = f.Truncate(at); err != nil {
		return err
	}
	return f.Sync()
}

// budget is what bounds the journal as a whole: its total with Rollover (0 is
// none), its limit otherwise.
func (j *Journal) budget() int64 {
	if j.rollover {
		return j.total
	}
	return j.limit
}

// Append records value, leaving the reserve (Reserve) untouched.
func (j *Journal) Append(value any) (Record, error) { return j.append(value, false) }

// AppendReserved records value, using the reserve if it must.
func (j *Journal) AppendReserved(value any) (Record, error) { return j.append(value, true) }

func (j *Journal) append(value any, reserved bool) (Record, error) {
	j.mu.Lock()
	defer j.mu.Unlock()
	if j.closed {
		return Record{}, os.ErrClosed
	}
	if j.broken != nil {
		// Undo what is left of the failed record now, or refuse again: a
		// caller retrying a write the disk refused gets it once there is room.
		if err := j.undo(&j.segments[len(j.segments)-1]); err != nil {
			return Record{}, fmt.Errorf("journal unwritable after a failed write: %w", j.broken)
		}
		j.broken = nil
	}
	b, err := json.Marshal(value)
	if err != nil {
		return Record{}, err
	}
	r := Record{Sequence: uint64(len(j.positions) + 1), Time: time.Now().UTC(), Value: b}
	marked := j.mark != nil && j.mark(b)
	b, err = json.Marshal(r)
	if err != nil {
		return Record{}, err
	}
	b = append(b, '\n')
	n := int64(len(b))
	available := j.budget()
	if !reserved {
		available -= j.reserve
	}
	if !j.rollover {
		if j.size+n > available {
			return Record{}, ErrFull
		}
	} else {
		if j.total > 0 && j.size+n > available {
			return Record{}, fmt.Errorf("%w: the journal holds its %d-byte total budget", ErrFull, j.total)
		}
		// A record larger than a segment's budget gets a segment of its own:
		// refusing it would leave its agent's output with nowhere to go.
		if last := j.segments[len(j.segments)-1]; last.size > 0 && last.size+n > j.limit {
			if err = j.roll(); err != nil {
				return Record{}, err
			}
		}
	}
	s := &j.segments[len(j.segments)-1]
	written, err := writeAt(s.f, b, s.size)
	if err == nil && int64(written) != n {
		err = io.ErrShortWrite
	}
	if err == nil {
		err = s.f.Sync()
	}
	if err != nil {
		// Nothing of a failed record stays: what follows starts where it
		// did. The descriptor stays open for readers of what is recorded.
		if j.undo(s) != nil {
			j.broken = err
		}
		return Record{}, err
	}
	j.positions = append(j.positions, j.size)
	j.size += n
	s.size += n
	if marked {
		j.marks = append(j.marks, r.Sequence)
	}
	close(j.notify)
	j.notify = make(chan struct{})
	return r, nil
}

// undo cuts a segment back to its complete records.
func (j *Journal) undo(s *segment) error {
	if err := s.f.Truncate(s.size); err != nil {
		return err
	}
	return s.f.Sync()
}

// writeAt writes a record at its place; tests make it fail part way.
var writeAt = (*os.File).WriteAt

// roll starts the next segment; j.mu is held.
func (j *Journal) roll() error {
	path := segmentPath(j.path, len(j.segments))
	f, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_RDWR, 0600)
	if err != nil {
		return fmt.Errorf("start journal segment: %w", err)
	}
	// The new name must survive a crash as surely as what is written in it.
	if err = syncDir(filepath.Dir(path)); err != nil {
		f.Close()
		_ = os.Remove(path)
		return fmt.Errorf("start journal segment: %w", err)
	}
	j.segments = append(j.segments, segment{f: f, path: path, start: j.size})
	return nil
}

func syncDir(dir string) error {
	d, err := os.Open(dir)
	if err != nil {
		return err
	}
	err = d.Sync()
	if closeErr := d.Close(); err == nil {
		err = closeErr
	}
	return err
}

// readAt reads len(b) journal bytes from position at, across segments.
// Written records never change, so the segments may be a copy taken under
// j.mu and read without it.
func readAt(segments []segment, b []byte, at int64) error {
	for _, s := range segments {
		if len(b) == 0 {
			return nil
		}
		if at >= s.start+s.size {
			continue
		}
		part := b[:min(int64(len(b)), s.start+s.size-at)]
		if _, err := s.f.ReadAt(part, at-s.start); err != nil {
			return err
		}
		b, at = b[len(part):], at+int64(len(part))
	}
	if len(b) > 0 {
		return io.ErrUnexpectedEOF
	}
	return nil
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
			b := make([]byte, j.end(cursor+1)-start)
			err := readAt(j.segments, b, start)
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

// Marks returns the sequences of the marked records (see Marking), ascending.
func (j *Journal) Marks() []uint64 {
	j.mu.Lock()
	defer j.mu.Unlock()
	return append([]uint64(nil), j.marks...)
}

// end is where record n ends in the journal (zero: the start); j.mu is held.
func (j *Journal) end(n uint64) int64 {
	if n < uint64(len(j.positions)) {
		return j.positions[n]
	}
	return j.size
}

// Span is the size in bytes of the records after `after` up to `upTo`.
func (j *Journal) Span(after, upTo uint64) int64 {
	j.mu.Lock()
	defer j.mu.Unlock()
	upTo = min(upTo, uint64(len(j.positions)))
	if after >= upTo {
		return 0
	}
	return j.end(upTo) - j.end(after)
}

// Fit returns the earliest `after` for which the records after it up to
// `upTo` fit in budget bytes: upTo itself when not even the last one does.
func (j *Journal) Fit(upTo uint64, budget int64) uint64 {
	j.mu.Lock()
	defer j.mu.Unlock()
	upTo = min(upTo, uint64(len(j.positions)))
	stop := j.end(upTo)
	// The span grows as `after` falls: the first that fits, by binary search.
	low, high := uint64(0), upTo
	for low < high {
		mid := low + (high-low)/2
		if stop-j.end(mid) <= budget {
			high = mid
		} else {
			low = mid + 1
		}
	}
	return low
}

// Range returns the records after `after` up to `upTo`, read in one pass.
func (j *Journal) Range(after, upTo uint64) ([]Record, error) {
	j.mu.Lock()
	if upTo > uint64(len(j.positions)) || after > upTo {
		j.mu.Unlock()
		return nil, ErrCursor
	}
	if after == upTo {
		j.mu.Unlock()
		return nil, nil
	}
	start := j.end(after)
	b := make([]byte, j.end(upTo)-start)
	bounds := append([]int64(nil), j.positions[after:upTo]...)
	segments := append([]segment(nil), j.segments...)
	j.mu.Unlock()
	// Written records never change: a large read does not hold appends back.
	if err := readAt(segments, b, start); err != nil {
		return nil, err
	}
	records := make([]Record, len(bounds))
	for i, at := range bounds {
		stop := int64(len(b))
		if i+1 < len(bounds) {
			stop = bounds[i+1] - start
		}
		if err := json.Unmarshal(b[at-start:stop], &records[i]); err != nil {
			return nil, err
		}
	}
	return records, nil
}

func (j *Journal) Count() uint64 { j.mu.Lock(); defer j.mu.Unlock(); return uint64(len(j.positions)) }

// Segments is how many files the journal spans.
func (j *Journal) Segments() int { j.mu.Lock(); defer j.mu.Unlock(); return len(j.segments) }

// Damage is why OpenExisting stopped before the end of the journal, or why
// an Unreadable journal could not be read; nil when it read everything.
func (j *Journal) Damage() error { j.mu.Lock(); defer j.mu.Unlock(); return j.damage }

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
func (j *Journal) Dispose() error {
	_ = j.Close()
	j.mu.Lock()
	defer j.mu.Unlock()
	return j.release()
}

// release closes every segment's file; j.mu is held or the journal unshared.
func (j *Journal) release() error {
	var errs []error
	for _, s := range j.segments {
		if err := s.f.Close(); err != nil && !errors.Is(err, os.ErrClosed) {
			errs = append(errs, err)
		}
	}
	return errors.Join(errs...)
}

// segmentPath is the file of segment n of the journal at path.
func segmentPath(path string, n int) string {
	if n == 0 {
		return path
	}
	return path + "." + strconv.Itoa(n)
}

// Files returns the segment files of the journal at path, in order: none
// when it has no file yet.
func Files(path string) ([]string, error) {
	var files []string
	for n := 0; ; n++ {
		p := segmentPath(path, n)
		if _, err := os.Stat(p); errors.Is(err, os.ErrNotExist) {
			return files, nil
		} else if err != nil {
			return nil, err
		}
		files = append(files, p)
	}
}

// Remove deletes the journal at path: every segment, and what was torn from
// them. A journal that is not there is not an error.
func Remove(path string) error {
	dir, base := filepath.Split(path)
	if dir == "" {
		dir = "."
	}
	entries, err := os.ReadDir(dir)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	var errs []error
	for _, entry := range entries {
		name := entry.Name()
		if name != base && !(strings.HasPrefix(name, base+".") && journalSuffix(strings.TrimPrefix(name, base+"."))) {
			continue
		}
		if err := os.Remove(filepath.Join(dir, name)); err != nil && !errors.Is(err, os.ErrNotExist) {
			errs = append(errs, err)
		}
	}
	return errors.Join(errs...)
}

// journalSuffix: what follows a journal's path in the name of one of its
// files — a segment number, "torn", or a segment number and ".torn".
func journalSuffix(s string) bool {
	s = strings.TrimSuffix(s, "torn")
	s = strings.TrimSuffix(s, ".")
	if s == "" {
		return true
	}
	_, err := strconv.ParseUint(s, 10, 32)
	return err == nil
}

// Tail reads the last limit complete records of the journal at path, from at
// most about maxBytes of its end, without opening it as a journal: another
// writer may be appending to it, and its last line may not be complete yet.
func Tail(path string, limit int, maxBytes int64) ([]Record, error) {
	files, err := Files(path)
	if err != nil {
		return nil, err
	}
	if len(files) == 0 {
		return nil, &os.PathError{Op: "open", Path: path, Err: os.ErrNotExist}
	}
	var lines [][]byte
	for i := len(files) - 1; i >= 0 && len(lines) < limit && maxBytes > 0; i-- {
		data, cut, err := tailOf(files[i], maxBytes)
		if err != nil {
			return nil, err
		}
		maxBytes -= int64(len(data))
		var found [][]byte
		if len(data) > 0 {
			found = bytes.Split(data, []byte{'\n'})
		}
		lines = append(found, lines...)
		if cut {
			break
		}
	}
	if len(lines) > limit {
		lines = lines[len(lines)-limit:]
	}
	out := make([]Record, 0, len(lines))
	for _, line := range lines {
		var record Record
		if len(line) > 0 && json.Unmarshal(line, &record) == nil {
			out = append(out, record)
		}
	}
	return out, nil
}

// tailOf reads the complete lines in the last maxBytes of one segment,
// without their last newline; cut reports that it did not reach the start.
func tailOf(path string, maxBytes int64) (data []byte, cut bool, err error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, false, err
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil {
		return nil, false, err
	}
	start := max(info.Size()-maxBytes, 0)
	data = make([]byte, info.Size()-start)
	if _, err = f.ReadAt(data, start); err != nil && !errors.Is(err, io.EOF) {
		return nil, false, err
	}
	if start > 0 {
		// The first line read is the end of a record.
		if at := bytes.IndexByte(data, '\n'); at >= 0 {
			data = data[at+1:]
		} else {
			data = nil
		}
	}
	// A last line without its newline is still being written (or was torn).
	if at := bytes.LastIndexByte(data, '\n'); at >= 0 {
		data = data[:at]
	} else {
		data = nil
	}
	return data, start > 0, nil
}
