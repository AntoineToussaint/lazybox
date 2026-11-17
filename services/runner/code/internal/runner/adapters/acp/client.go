package acp

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// The client half of ACP, served by Lazybox: the workspace's files, and
// terminals that are managed commands in the conversation's console.

// fileLimit bounds one file read or write through the protocol.
const fileLimit = 16 << 20

// readOnly reports whether the conversation may not change files.
func (s *session) readOnly() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.state.Permission == "read-only" || s.state.Workflow == "plan" || agent.IsAssistant(s.purpose)
}

// within reports whether path is root or below it.
func within(root, path string) bool {
	rel, err := filepath.Rel(root, path)
	return err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator)) && !filepath.IsAbs(rel)
}

// resolve is path with every symlink of its existing part resolved, so a
// link cannot lead out of the workspace. The part that does not exist yet
// (a file about to be written) is kept as it is.
func resolve(path string) (string, error) {
	existing, rest := filepath.Clean(path), ""
	for {
		resolved, err := filepath.EvalSymlinks(existing)
		if err == nil {
			return filepath.Join(resolved, rest), nil
		}
		if !errors.Is(err, os.ErrNotExist) {
			return "", err
		}
		parent := filepath.Dir(existing)
		if parent == existing {
			return "", err
		}
		rest = filepath.Join(filepath.Base(existing), rest)
		existing = parent
	}
}

// workspacePath checks that the agent's path is in the conversation's
// workspace (or, for a read, a file the person attached) and resolves it.
func (s *session) workspacePath(path string, write bool) (string, error) {
	if !filepath.IsAbs(path) {
		return "", clientError{invalidParams, "paths must be absolute: " + path}
	}
	root, err := filepath.EvalSymlinks(s.cwd)
	if err != nil {
		return "", clientError{internalError, "the workspace cannot be resolved"}
	}
	resolved, err := resolve(path)
	if err != nil {
		return "", clientError{internalError, err.Error()}
	}
	if within(root, resolved) {
		if write {
			for _, part := range strings.Split(resolved[len(root):], string(filepath.Separator)) {
				if part == ".git" {
					return "", clientError{invalidParams, "Lazybox does not let agents write inside .git: " + path}
				}
			}
		}
		return resolved, nil
	}
	if !write {
		s.mu.Lock()
		readable := append([]string(nil), s.state.Readable...)
		s.mu.Unlock()
		for _, allowed := range readable {
			if filepath.Clean(allowed) == filepath.Clean(path) {
				return resolved, nil
			}
		}
	}
	return "", clientError{invalidParams, "outside this conversation's workspace: " + path}
}

func (s *session) readFile(raw json.RawMessage) (any, error) {
	var p struct {
		Path  string `json:"path"`
		Line  *int   `json:"line"`
		Limit *int   `json:"limit"`
	}
	if json.Unmarshal(raw, &p) != nil || p.Path == "" {
		return nil, clientError{invalidParams, "fs/read_text_file needs a path"}
	}
	path, err := s.workspacePath(p.Path, false)
	if err != nil {
		return nil, err
	}
	f, err := os.Open(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil, clientError{resourceNotFound, "no such file: " + p.Path}
	}
	if err != nil {
		return nil, err
	}
	defer f.Close()
	if info, err := f.Stat(); err != nil || info.IsDir() {
		return nil, clientError{invalidParams, "not a file: " + p.Path}
	}
	head := make([]byte, 8<<10)
	n, _ := io.ReadFull(f, head)
	if bytes.IndexByte(head[:n], 0) >= 0 {
		return nil, clientError{invalidParams, "not a text file: " + p.Path}
	}
	if _, err = f.Seek(0, io.SeekStart); err != nil {
		return nil, err
	}
	if p.Line == nil && p.Limit == nil {
		data, err := io.ReadAll(io.LimitReader(f, fileLimit+1))
		if err != nil {
			return nil, err
		}
		if len(data) > fileLimit {
			return nil, clientError{invalidParams, "the file is larger than 16 MiB; read it with line and limit: " + p.Path}
		}
		return map[string]string{"content": string(data)}, nil
	}
	first, limit := 1, -1
	if p.Line != nil && *p.Line > 0 {
		first = *p.Line
	}
	if p.Limit != nil && *p.Limit >= 0 {
		limit = *p.Limit
	}
	var out strings.Builder
	r := bufio.NewReader(f)
	for line := 1; limit != 0; line++ {
		text, err := r.ReadString('\n')
		if line >= first && text != "" {
			if out.Len()+len(text) > fileLimit {
				return nil, clientError{invalidParams, "the lines asked for are larger than 16 MiB"}
			}
			out.WriteString(text)
			if limit > 0 {
				limit--
			}
		}
		if err != nil {
			break
		}
	}
	return map[string]string{"content": out.String()}, nil
}

func (s *session) writeFile(raw json.RawMessage) (any, error) {
	var p struct {
		Path    string  `json:"path"`
		Content *string `json:"content"`
	}
	if json.Unmarshal(raw, &p) != nil || p.Path == "" || p.Content == nil {
		return nil, clientError{invalidParams, "fs/write_text_file needs a path and content"}
	}
	if s.readOnly() {
		return nil, clientError{invalidParams, "this conversation is read-only: Lazybox writes no files for it"}
	}
	if len(*p.Content) > fileLimit {
		return nil, clientError{invalidParams, "content larger than 16 MiB"}
	}
	path, err := s.workspacePath(p.Path, true)
	if err != nil {
		return nil, err
	}
	mode := os.FileMode(0o644)
	if info, err := os.Stat(path); err == nil {
		if info.IsDir() {
			return nil, clientError{invalidParams, "a directory: " + p.Path}
		}
		mode = info.Mode().Perm()
	}
	if err = os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return nil, err
	}
	tmp, err := os.CreateTemp(filepath.Dir(path), ".lazybox-write-*")
	if err != nil {
		return nil, err
	}
	defer os.Remove(tmp.Name())
	if _, err = tmp.WriteString(*p.Content); err == nil {
		err = tmp.Chmod(mode)
	}
	if closeErr := tmp.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return nil, err
	}
	if err = os.Rename(tmp.Name(), path); err != nil {
		return nil, err
	}
	return map[string]any{}, nil
}

// command calls the Runner's managed command tool for this conversation.
func (s *session) command(ctx context.Context, args map[string]any) (json.RawMessage, error) {
	if s.tools == nil {
		return nil, clientError{invalidParams, "this conversation cannot run commands"}
	}
	data, _ := json.Marshal(args)
	out, err := s.tools(ctx, agent.CommandTool, data)
	if err != nil {
		return nil, err
	}
	return json.RawMessage(out), nil
}

type commandStatus struct {
	ID       string `json:"id"`
	Done     bool   `json:"done"`
	ExitCode *int   `json:"exitCode"`
	Signal   string `json:"signal"`
}

func exitStatus(c commandStatus) map[string]any {
	out := map[string]any{"exitCode": c.ExitCode}
	if c.Signal != "" {
		out["signal"] = c.Signal
	} else {
		out["signal"] = nil
	}
	return out
}

// shellChars mark a command line that only a shell can run.
const shellChars = " \t|&;<>()$`\\\"'*?~{}"

// createTerminal starts the agent's command as a managed command of this
// conversation: it is in the console, outlives the Runner, and the person
// can read or stop it. A read-only conversation refuses (the Runner does).
func (s *session) createTerminal(raw json.RawMessage) (any, error) {
	var p struct {
		Command string   `json:"command"`
		Args    []string `json:"args"`
		Env     []struct {
			Name  string `json:"name"`
			Value string `json:"value"`
		} `json:"env"`
		Cwd   *string `json:"cwd"`
		Limit *int64  `json:"outputByteLimit"`
	}
	if json.Unmarshal(raw, &p) != nil || strings.TrimSpace(p.Command) == "" {
		return nil, clientError{invalidParams, "terminal/create needs a command"}
	}
	argv := append([]string{p.Command}, p.Args...)
	line := strings.Join(argv, " ")
	if len(p.Args) == 0 && strings.ContainsAny(strings.TrimSpace(p.Command), shellChars) {
		argv = []string{"/bin/sh", "-c", p.Command}
		line = p.Command
	}
	args := map[string]any{"action": "start", "argv": argv, "title": line}
	if p.Cwd != nil && *p.Cwd != "" {
		args["cwd"] = *p.Cwd
	}
	if len(p.Env) > 0 {
		env := []string{}
		for _, e := range p.Env {
			env = append(env, e.Name+"="+e.Value)
		}
		args["env"] = env
	}
	out, err := s.command(s.ctx, args)
	if err != nil {
		return nil, err
	}
	var c commandStatus
	if json.Unmarshal(out, &c) != nil || c.ID == "" {
		return nil, clientError{internalError, "the Runner did not start the command"}
	}
	limit := 1 << 20
	if p.Limit != nil && *p.Limit > 0 && *p.Limit < int64(limit) {
		limit = int(*p.Limit)
	}
	s.mu.Lock()
	s.terminals[c.ID] = limit
	s.mu.Unlock()
	return map[string]string{"terminalId": c.ID}, nil
}

func terminalID(raw json.RawMessage) (string, error) {
	var p struct {
		ID string `json:"terminalId"`
	}
	if json.Unmarshal(raw, &p) != nil || p.ID == "" {
		return "", clientError{invalidParams, "a terminalId is required"}
	}
	return p.ID, nil
}

// tail is a command's latest output, at most limit bytes, cut on a UTF-8
// boundary, and whether earlier output was left out.
func (s *session) tail(ctx context.Context, id string, limit int) (string, bool, commandStatus, error) {
	out, err := s.command(ctx, map[string]any{"action": "tail", "id": id, "limit": limit})
	if err != nil {
		return "", false, commandStatus{}, err
	}
	var t struct {
		Output    string        `json:"output"`
		Truncated bool          `json:"truncated"`
		Status    commandStatus `json:"status"`
	}
	if err = json.Unmarshal(out, &t); err != nil {
		return "", false, commandStatus{}, err
	}
	return t.Output, t.Truncated, t.Status, nil
}

func (s *session) terminalOutput(raw json.RawMessage) (any, error) {
	id, err := terminalID(raw)
	if err != nil {
		return nil, err
	}
	s.mu.Lock()
	limit, ok := s.terminals[id]
	s.mu.Unlock()
	if !ok {
		limit = 1 << 20
	}
	output, truncated, status, err := s.tail(s.ctx, id, limit)
	if err != nil {
		return nil, err
	}
	result := map[string]any{"output": output, "truncated": truncated}
	if status.Done {
		result["exitStatus"] = exitStatus(status)
	}
	return result, nil
}

func (s *session) waitTerminal(raw json.RawMessage) (any, error) {
	id, err := terminalID(raw)
	if err != nil {
		return nil, err
	}
	out, err := s.command(s.ctx, map[string]any{"action": "wait", "id": id})
	if err != nil {
		return nil, err
	}
	var c commandStatus
	if err = json.Unmarshal(out, &c); err != nil {
		return nil, err
	}
	return exitStatus(c), nil
}

func (s *session) killTerminal(raw json.RawMessage) (any, error) {
	id, err := terminalID(raw)
	if err != nil {
		return nil, err
	}
	if _, err = s.command(s.ctx, map[string]any{"action": "cancel", "id": id}); err != nil {
		return nil, err
	}
	return map[string]any{}, nil
}

// releaseTerminal stops the command if it still runs. It stays in the
// console, with its output, for the person.
func (s *session) releaseTerminal(raw json.RawMessage) (any, error) {
	id, err := terminalID(raw)
	if err != nil {
		return nil, err
	}
	_, _, status, err := s.tail(s.ctx, id, 1)
	if err != nil {
		return nil, err
	}
	if !status.Done {
		if _, err = s.command(s.ctx, map[string]any{"action": "cancel", "id": id}); err != nil {
			return nil, err
		}
	}
	s.mu.Lock()
	delete(s.terminals, id)
	s.mu.Unlock()
	return map[string]any{}, nil
}

// commandResult is a finished command's output and exit code, for the line
// of the tool call that ran it.
func (s *session) commandResult(id string) (string, *int, bool) {
	ctx, cancel := context.WithTimeout(s.ctx, 2*time.Second)
	defer cancel()
	output, truncated, status, err := s.tail(ctx, id, 16<<10)
	if err != nil {
		return "", nil, false
	}
	if truncated {
		output = "…\n" + output
	}
	if !status.Done {
		return output, nil, true
	}
	return output, status.ExitCode, true
}
