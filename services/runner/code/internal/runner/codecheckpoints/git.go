package codecheckpoints

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"io"
	"os"
	"os/exec"
	"strings"
	"time"
)

// gitTimeout bounds one Git invocation; a whole snapshot is bounded by its caller.
const gitTimeout = 60 * time.Second

// diffLimit bounds a diff shown to a person, like workspace review does.
const diffLimit = 1 << 20

type bounded struct {
	bytes.Buffer
	truncated bool
	limit     int
}

func (b *bounded) Write(p []byte) (int, error) {
	n := len(p)
	if b.limit > 0 {
		remaining := b.limit - b.Len()
		if len(p) > remaining {
			b.truncated = true
			p = p[:max(remaining, 0)]
		}
	}
	_, _ = b.Buffer.Write(p)
	return n, nil
}

// gitEnv is the parent environment without anything that would point Git at
// another repository or index, plus a fixed identity for checkpoint commits.
// The person's Git identity and signing configuration are not needed for a
// private ref, and an unconfigured identity must not fail a checkpoint.
func gitEnv(index string) []string {
	env := []string{}
	for _, entry := range os.Environ() {
		name, _, _ := strings.Cut(entry, "=")
		switch name {
		case "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_NAMESPACE", "GIT_PREFIX", "GIT_LITERAL_PATHSPECS", "GIT_GLOB_PATHSPECS", "GIT_NOGLOB_PATHSPECS", "GIT_ICASE_PATHSPECS":
			continue
		}
		env = append(env, entry)
	}
	env = append(env, "GIT_TERMINAL_PROMPT=0", "GIT_LITERAL_PATHSPECS=1", "LC_ALL=C",
		"GIT_AUTHOR_NAME=Lazybox", "GIT_AUTHOR_EMAIL=checkpoints@lazybox.invalid",
		"GIT_COMMITTER_NAME=Lazybox", "GIT_COMMITTER_EMAIL=checkpoints@lazybox.invalid")
	if index != "" {
		env = append(env, "GIT_INDEX_FILE="+index)
	}
	return env
}

type gitCall struct {
	dir   string
	index string
	stdin []byte
	limit int
}

// command is Git in c.dir with the call's index and stdin.
func (c gitCall) command(ctx context.Context, args []string) *exec.Cmd {
	base := []string{"-c", "core.fsmonitor=false", "-c", "core.safecrlf=false", "-c", "gc.auto=0", "-c", "advice.addEmbeddedRepo=false", "-c", "core.quotePath=false", "-C", c.dir}
	cmd := exec.CommandContext(ctx, "git", append(base, args...)...)
	cmd.Env = gitEnv(c.index)
	if c.stdin != nil {
		cmd.Stdin = bytes.NewReader(c.stdin)
	}
	return cmd
}

// run executes Git in dir. index, when set, is the private index the call
// reads and writes instead of the repository's own.
func (c gitCall) run(ctx context.Context, args ...string) ([]byte, bool, error) {
	ctx, cancel := context.WithTimeout(ctx, gitTimeout)
	defer cancel()
	cmd := c.command(ctx, args)
	out := &bounded{limit: c.limit}
	stderr := &bounded{limit: 4096}
	cmd.Stdout, cmd.Stderr = out, stderr
	err := cmd.Run()
	if ctx.Err() != nil {
		return nil, false, errors.New("git " + args[0] + " timed out")
	}
	if err != nil {
		message := strings.TrimSpace(stderr.String())
		if message == "" {
			message = err.Error()
		}
		return out.Bytes(), out.truncated, errors.New("git " + args[0] + ": " + message)
	}
	return out.Bytes(), out.truncated, nil
}

// maxField bounds one NUL-terminated field of streamed output: a path, or a
// diff-tree status line.
const maxField = 64 << 10

// errStop, returned by a stream callback, ends the stream early without error.
var errStop = errors.New("stop")

// stream runs Git and hands each NUL-terminated field of its output to each,
// in order, as it arrives, so output of any length is read in bounded memory.
// When each returns errStop, Git is stopped and stream returns nil.
func (c gitCall) stream(ctx context.Context, each func(field string) error, args ...string) error {
	ctx, cancel := context.WithTimeout(ctx, gitTimeout)
	defer cancel()
	cmd := c.command(ctx, args)
	stderr := &bounded{limit: 4096}
	cmd.Stderr = stderr
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return err
	}
	if err = cmd.Start(); err != nil {
		return err
	}
	reader := bufio.NewReaderSize(stdout, maxField)
	var readErr error
	for {
		field, err := reader.ReadSlice(0)
		if errors.Is(err, io.EOF) {
			if len(field) > 0 {
				readErr = each(string(field))
			}
			break
		}
		if errors.Is(err, bufio.ErrBufferFull) {
			readErr = errors.New("git " + args[0] + ": output field too long")
			break
		}
		if err != nil {
			readErr = err
			break
		}
		if readErr = each(string(field[:len(field)-1])); readErr != nil {
			break
		}
	}
	if readErr != nil {
		// Stop Git rather than wait for output nobody reads.
		cancel()
	}
	waitErr := cmd.Wait()
	if errors.Is(readErr, errStop) {
		return nil
	}
	if readErr != nil {
		return readErr
	}
	if ctx.Err() != nil {
		return errors.New("git " + args[0] + " timed out")
	}
	if waitErr != nil {
		message := strings.TrimSpace(stderr.String())
		if message == "" {
			message = waitErr.Error()
		}
		return errors.New("git " + args[0] + ": " + message)
	}
	return nil
}

func git(ctx context.Context, dir string, args ...string) (string, error) {
	out, _, err := gitCall{dir: dir}.run(ctx, args...)
	return strings.TrimSpace(string(out)), err
}

// splitNUL splits Git's -z output, dropping the empty field after the last NUL.
func splitNUL(data []byte) []string {
	fields := strings.Split(string(data), "\x00")
	if len(fields) > 0 && fields[len(fields)-1] == "" {
		fields = fields[:len(fields)-1]
	}
	return fields
}
