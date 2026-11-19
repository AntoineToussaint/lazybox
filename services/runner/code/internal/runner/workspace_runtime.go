package runner

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/terminals"
	wr "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/workspaceruntime"
)

// The workspace runtime (WORKSPACE_RUNTIME.md): each repository's committed
// .lazybox/workspace.yaml runs as managed commands, so its output is in the
// console and its processes outlive a Runner restart like any command.

// StopGrace is how long an action, setup or archive gets between SIGTERM to
// its process group and SIGKILL.
const StopGrace = 5 * time.Second

// ErrScriptsNeedApproval refuses to run a repository's scripts the person has
// not approved in this exact version.
var ErrScriptsNeedApproval = errors.New("this repository's workspace scripts need your approval first")

type runRecord struct {
	CommandID string `json:"commandId"`
	StartedAt int64  `json:"startedAt"`
	// Stopped: a person or Lazybox asked it to stop.
	Stopped bool `json:"stopped,omitempty"`
}

// runtimeRecord is what the runtime started in one workspace: the latest run
// of each action (by key), setup and archive (by member), and the files copied.
type runtimeRecord struct {
	Actions map[string]runRecord     `json:"actions,omitempty"`
	Setup   map[string]runRecord     `json:"setup,omitempty"`
	Archive map[string]runRecord     `json:"archive,omitempty"`
	Files   map[string]wr.CopyResult `json:"files,omitempty"`
}

type workspaceRuntime struct {
	dir       string
	ports     *wr.Ports
	approvals *wr.Approvals
	settings  *wr.SettingsStore
	// control serialises starts and stops, so two clicks never start an
	// action twice and a non-concurrent action is never running in two places.
	control sync.Mutex
	mu      sync.Mutex
	records map[string]*runtimeRecord
}

func openWorkspaceRuntime(dir string) (*workspaceRuntime, error) {
	if err := os.MkdirAll(filepath.Join(dir, "workspaces"), 0o700); err != nil {
		return nil, err
	}
	rt := &workspaceRuntime{dir: dir, records: map[string]*runtimeRecord{}}
	var err error
	if rt.ports, err = wr.OpenPorts(filepath.Join(dir, "ports.json")); err != nil {
		return nil, err
	}
	if rt.approvals, err = wr.OpenApprovals(filepath.Join(dir, "approvals.json")); err != nil {
		return nil, err
	}
	if rt.settings, err = wr.OpenSettings(filepath.Join(dir, "settings.json")); err != nil {
		return nil, err
	}
	return rt, nil
}

var safeID = regexp.MustCompile(`^[A-Za-z0-9_.-]{1,128}$`)

func (rt *workspaceRuntime) recordPath(id string) string {
	return filepath.Join(rt.dir, "workspaces", id+".json")
}

// record returns a copy of a workspace's record.
func (rt *workspaceRuntime) record(id string) runtimeRecord {
	rt.mu.Lock()
	defer rt.mu.Unlock()
	return clone(rt.loadLocked(id))
}

func (rt *workspaceRuntime) loadLocked(id string) *runtimeRecord {
	if r := rt.records[id]; r != nil {
		return r
	}
	r := &runtimeRecord{}
	if safeID.MatchString(id) {
		_ = wr.LoadJSON(rt.recordPath(id), r)
	}
	rt.records[id] = r
	return r
}

func clone(r *runtimeRecord) runtimeRecord {
	out := runtimeRecord{Actions: map[string]runRecord{}, Setup: map[string]runRecord{}, Archive: map[string]runRecord{}, Files: map[string]wr.CopyResult{}}
	for k, v := range r.Actions {
		out.Actions[k] = v
	}
	for k, v := range r.Setup {
		out.Setup[k] = v
	}
	for k, v := range r.Archive {
		out.Archive[k] = v
	}
	for k, v := range r.Files {
		out.Files[k] = v
	}
	return out
}

// update changes a workspace's record and saves it.
func (rt *workspaceRuntime) update(id string, change func(*runtimeRecord)) error {
	if !safeID.MatchString(id) {
		return errors.New("invalid workspace id")
	}
	rt.mu.Lock()
	defer rt.mu.Unlock()
	next := clone(rt.loadLocked(id))
	change(&next)
	if err := wr.SaveJSON(rt.recordPath(id), next); err != nil {
		return err
	}
	rt.records[id] = &next
	return nil
}

func (rt *workspaceRuntime) forget(id string) error {
	rt.mu.Lock()
	defer rt.mu.Unlock()
	delete(rt.records, id)
	if err := os.Remove(rt.recordPath(id)); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	return rt.ports.Release(id)
}

// WorkspaceMember is one workspace member's runtime: its committed configuration, its
// approval, the person's overrides and what runs as a result.
type WorkspaceMember struct {
	Name, RepositoryID string
	// Worktree is the member's folder; Source the checkout it was made from.
	Worktree, Source string
	// Path is ConfigPath when the repository commits one.
	Path     string
	Content  string
	Config   wr.Config
	Error    string
	Approval string
	Changed  []string
	Override wr.Override
	// OverrideError: the person's own settings for this repository do not
	// parse, so none of them apply.
	OverrideError string
	Effective     wr.Effective
	// Waiting are the repository's actions that cannot run until approved.
	Waiting []wr.Action
}

// approved reports whether nothing of the repository's needs approval.
func (m WorkspaceMember) approved() bool { return m.Approval == "approved" || m.Approval == "none" }

func (b *Bench) runtimeMembers(c collections.Collection) []WorkspaceMember {
	out := []WorkspaceMember{}
	for _, cm := range c.Members {
		m := WorkspaceMember{Name: cm.Name, RepositoryID: cm.RepositoryID, Worktree: cm.Path, Approval: "none"}
		if m.Name == "" {
			m.Name = filepath.Base(cm.Path)
		}
		if repo, ok := b.Collections.Repository(cm.RepositoryID); ok {
			m.Source = repo.Path
		}
		m.Override = b.runtime.settings.Override(m.RepositoryID)
		mine, overrideErr := m.Override.Parsed()
		content, readErr := readConfig(cm.Path)
		repo := wr.Config{}
		switch {
		case errors.Is(readErr, os.ErrNotExist):
		case readErr != nil:
			m.Path, m.Error, m.Approval = wr.ConfigPath, readErr.Error(), "required"
		default:
			m.Path, m.Content = wr.ConfigPath, string(content)
			parsed, err := wr.Parse(content)
			if err != nil {
				m.Error, m.Approval = err.Error(), "required"
				break
			}
			m.Config = parsed
			if !parsed.Empty() {
				m.Approval, m.Changed = b.runtime.approvals.Status(m.RepositoryID, parsed)
			}
			if m.Approval == "approved" || m.Approval == "none" {
				repo = parsed
			} else {
				m.Waiting = parsed.Actions
			}
		}
		if overrideErr != nil {
			// Saves are validated, so only a hand-edited file gets here: none
			// of your entries apply; the repository's still run.
			m.OverrideError = "Your settings for this repository are not used: " + overrideErr.Error()
		}
		m.Effective = wr.Merge(repo, mine)
		out = append(out, m)
	}
	return out
}

// readConfig reads a member's committed configuration, confined to its worktree.
func readConfig(worktree string) ([]byte, error) {
	root, err := os.OpenRoot(worktree)
	if err != nil {
		return nil, err
	}
	defer root.Close()
	f, err := root.Open(wr.ConfigPath)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	if info, err := f.Stat(); err != nil || !info.Mode().IsRegular() {
		return nil, errors.New(wr.ConfigPath + " is not a regular file")
	}
	data, err := io.ReadAll(io.LimitReader(f, wr.MaxConfigBytes+1))
	if err == nil && len(data) > wr.MaxConfigBytes {
		err = fmt.Errorf("%s is larger than %d KiB", wr.ConfigPath, wr.MaxConfigBytes>>10)
	}
	return data, err
}

// RunState is one run's state for people: "idle", "running", "stopping",
// "succeeded", "failed" or "stopped".
type RunState struct {
	State     string
	CommandID string
	ExitCode  *int
	Signal    string
	Error     string
	StartedAt int64
	LastLine  string
}

func (b *Bench) runState(ctx context.Context, r runRecord, ok bool) RunState {
	if !ok || r.CommandID == "" {
		return RunState{State: "idle"}
	}
	s := RunState{CommandID: r.CommandID, StartedAt: r.StartedAt}
	c, err := b.Commands.Get(r.CommandID)
	if err != nil {
		s.State = "idle"
		return s
	}
	status := c.Status()
	s.ExitCode, s.Signal, s.Error = status.ExitCode, status.Signal, status.Error
	s.LastLine = lastLine(c.Tail(ctx, 4096))
	switch {
	case !status.Done && r.Stopped:
		s.State = "stopping"
	case !status.Done:
		s.State = "running"
	case r.Stopped:
		s.State = "stopped"
	case status.Error == "" && status.ExitCode != nil && *status.ExitCode == 0:
		s.State = "succeeded"
	default:
		s.State = "failed"
	}
	return s
}

var ansi = regexp.MustCompile("\x1b\\[[0-9;?]*[ -/]*[@-~]|\x1b\\][^\x07\x1b]*(\x07|\x1b\\\\)")

// lastLine is the output's last non-empty line, without terminal escapes.
func lastLine(out []byte) string {
	lines := bytes.Split(bytes.ReplaceAll(out, []byte("\r"), []byte("\n")), []byte("\n"))
	for i := len(lines) - 1; i >= 0; i-- {
		line := strings.TrimSpace(ansi.ReplaceAllString(string(lines[i]), ""))
		if line != "" {
			if len(line) > 200 {
				line = line[:200] + "…"
			}
			return line
		}
	}
	return ""
}

// ActionView is one action as the person sees it.
type ActionView struct {
	Key, Member, Name, Description, Run, Cwd, URL, Source, Blocked string
	Default, Concurrent                                            bool
	Port                                                           int
	State                                                          RunState
	RunningElsewhere                                               []string
}

// SetupView is one member's setup.
type SetupView struct {
	Member, Run, Source string
	PendingApproval     bool
	State               RunState
	Files               wr.CopyResult
}

// RuntimeView is a workspace's runtime for GetWorkspaceRuntime.
type RuntimeView struct {
	CollectionID  string
	PortBase      int
	PortsError    string
	Members       []WorkspaceMember
	Actions       []ActionView
	Setups        []SetupView
	DefaultAction string
}

func (b *Bench) workspace(id string) (collections.Collection, error) {
	if b.Collections == nil || b.runtime == nil {
		return collections.Collection{}, errors.New("workspaces unavailable")
	}
	c, err := b.Collections.Get(id)
	if err != nil {
		return c, err
	}
	if c.State == "deleting" || c.State == "deleted" {
		return c, errors.New("workspace is being deleted")
	}
	return c, nil
}

// portBlock is the workspace's block, allocated the first time it is needed.
func (b *Bench) portBlock(id string) (int, error) {
	s := b.runtime.settings.Get()
	return b.runtime.ports.Block(id, s.PortStart, s.PortEnd)
}

// WorkspaceRuntime describes a workspace's runtime: what each repository
// commits and whether it is approved, its actions and their state, setup, and
// the port block.
func (b *Bench) WorkspaceRuntime(ctx context.Context, id string) (RuntimeView, error) {
	c, err := b.workspace(id)
	if err != nil {
		return RuntimeView{}, err
	}
	v := RuntimeView{CollectionID: id, Members: b.runtimeMembers(c)}
	if v.PortBase, err = b.portBlock(id); err != nil {
		v.PortsError = err.Error()
	}
	rec := b.runtime.record(id)
	running := b.runningActions(ctx)
	settingsDefault := ""
	for _, m := range v.Members {
		if m.Override.DefaultAction != "" && settingsDefault == "" {
			settingsDefault = m.Name + "/" + m.Override.DefaultAction
		}
		setupRun, setupSource := "", ""
		if m.Effective.Setup != nil {
			setupRun, setupSource = m.Effective.Setup.Run, m.Effective.Setup.Source
		}
		pending := !m.approved() && m.Config.Setup != nil
		if setupRun == "" && pending {
			setupRun, setupSource = m.Config.Setup.Run, wr.FromRepository
		}
		if setupRun != "" || len(m.Effective.Files) > 0 || pending {
			r, ok := rec.Setup[m.Name]
			v.Setups = append(v.Setups, SetupView{Member: m.Name, Run: setupRun, Source: setupSource, PendingApproval: pending, State: b.runState(ctx, r, ok), Files: rec.Files[m.Name]})
		}
		blocked := ""
		switch {
		case m.Error != "":
			blocked = "Fix " + m.Name + "'s " + wr.ConfigPath + " first"
		case !m.approved():
			blocked = "Approve " + m.Name + "'s workspace scripts first"
		}
		add := func(a wr.Action, source, blocked string) {
			key := m.Name + "/" + a.Name
			r, ok := rec.Actions[key]
			view := ActionView{Key: key, Member: m.Name, Name: a.Name, Description: a.Description, Run: a.Run, Cwd: a.Cwd, Source: source, Blocked: blocked, Default: a.Default, Concurrent: a.Concurrent, State: b.runState(ctx, r, ok)}
			if v.PortBase > 0 {
				view.Port = v.PortBase + a.Port
				if a.URL != "" {
					view.URL, _ = wr.CheckURL(wr.Expand(a.URL, wr.Variables(v.PortBase, a.Port)))
				}
			}
			if !a.Concurrent {
				for _, other := range running {
					if other.collection != id && other.repository == m.RepositoryID && other.name == a.Name {
						view.RunningElsewhere = append(view.RunningElsewhere, other.workspaceName)
					}
				}
			}
			v.Actions = append(v.Actions, view)
		}
		for _, a := range m.Effective.Actions {
			add(a.Action, a.Source, "")
		}
		for _, a := range m.Waiting {
			if _, overridden := m.Effective.ActionNamed(a.Name); !overridden {
				add(a, wr.FromRepository, blocked)
			}
		}
	}
	v.DefaultAction = defaultAction(v.Actions, settingsDefault)
	return v, nil
}

func defaultAction(actions []ActionView, preferred string) string {
	for _, a := range actions {
		if a.Key == preferred {
			return a.Key
		}
	}
	for _, a := range actions {
		if a.Default {
			return a.Key
		}
	}
	if len(actions) > 0 {
		return actions[0].Key
	}
	return ""
}

type runningAction struct {
	collection, workspaceName, repository, name, commandID, key string
}

// runningActions lists every running action on this Runner, in every
// workspace, with the repository it belongs to.
func (b *Bench) runningActions(ctx context.Context) []runningAction {
	out := []runningAction{}
	if b.Collections == nil {
		return out
	}
	for _, c := range b.Collections.List() {
		rec := b.runtime.record(c.ID)
		for key, r := range rec.Actions {
			cmd, err := b.Commands.Get(r.CommandID)
			if err != nil {
				continue
			}
			s := cmd.Status()
			if s.Done || ctx.Err() != nil {
				continue
			}
			_, name, _ := strings.Cut(key, "/")
			out = append(out, runningAction{collection: c.ID, workspaceName: c.Name, repository: s.Spec.Repository, name: name, commandID: r.CommandID, key: key})
		}
	}
	return out
}

// ApproveWorkspaceScripts records that the person approved exactly the
// configuration they reviewed (hash) for one member's repository, and runs
// its setup when asked and it has not succeeded yet.
func (b *Bench) ApproveWorkspaceScripts(ctx context.Context, id, memberName, hash string, runSetup bool) error {
	c, err := b.workspace(id)
	if err != nil {
		return err
	}
	m, err := b.findMember(c, memberName)
	if err != nil {
		return err
	}
	if m.Path == "" {
		return errors.New(memberName + " has no " + wr.ConfigPath + " to approve")
	}
	if m.Error != "" && m.Config.Empty() {
		return errors.New("fix " + wr.ConfigPath + " before approving it: " + m.Error)
	}
	if hash == "" || m.Config.Hash() != hash {
		return fmt.Errorf("%s: %w", wr.ConfigPath, ErrStale)
	}
	if err = b.runtime.approvals.Approve(m.RepositoryID, m.Config); err != nil {
		return err
	}
	if !runSetup {
		return nil
	}
	rec := b.runtime.record(id)
	if r, ok := rec.Setup[m.Name]; ok && b.runState(ctx, r, ok).State == "succeeded" {
		return nil
	}
	_, err = b.RunWorkspaceSetup(ctx, id, memberName)
	return err
}

// ErrStale refuses an approval of a version other than the one on disk.
var ErrStale = errors.New("the scripts changed while you were reviewing them; review them again")

func (b *Bench) findMember(c collections.Collection, name string) (WorkspaceMember, error) {
	for _, m := range b.runtimeMembers(c) {
		if m.Name == name {
			return m, nil
		}
	}
	return WorkspaceMember{}, fmt.Errorf("%w: %s is not in this workspace", os.ErrNotExist, name)
}

// runtimeEnv is the environment of every runtime script: the person's own
// (as their terminal gets it), the workspace's variables, its ports and the
// configuration's variables, which may use $LAZYBOX_PORT.
func (b *Bench) runtimeEnv(c collections.Collection, m WorkspaceMember, base, pick int, extra map[string]string) []string {
	vars := wr.Variables(base, pick)
	vars["LAZYBOX_WORKSPACE_ID"] = c.ID
	vars["LAZYBOX_WORKSPACE_NAME"] = c.Name
	vars["LAZYBOX_WORKSPACE_ROOT"] = c.Root
	vars["LAZYBOX_REPOSITORY"] = m.Name
	vars["LAZYBOX_WORKTREE"] = m.Worktree
	if home, _ := b.Collections.Home(m.RepositoryID); home != "" {
		vars["LAZYBOX_REPOSITORY_HOME"] = home
	}
	if root := os.Getenv("LAZYBOX_V2_ENVIRONMENTS"); root != "" {
		vars["LAZYBOX_ENVIRONMENTS"] = root
	}
	if m.Source != "" {
		vars["LAZYBOX_ROOT_PATH"] = m.Source
	}
	env := []string{}
	for _, entry := range terminals.Environment(os.Environ(), c.ID) {
		name, _, _ := strings.Cut(entry, "=")
		// Output goes to a console, not a terminal.
		if name != "TERM" && name != "COLORTERM" && !strings.HasPrefix(name, "LAZYBOX_") {
			env = append(env, entry)
		}
	}
	env = append(env, "TERM=dumb")
	for k, v := range vars {
		env = append(env, k+"="+v)
	}
	for _, layer := range []map[string]string{m.Effective.Env, extra} {
		for k, v := range layer {
			env = append(env, k+"="+wr.Expand(v, vars))
		}
	}
	return env
}

// runtimeCommand starts one runtime script as a managed command in the
// person's login shell, in its directory inside the member's worktree.
func (b *Bench) runtimeCommand(c collections.Collection, m WorkspaceMember, kind, entry, title, script, cwd string, env []string) (*commands.Command, error) {
	dir, err := terminals.Resolve(m.Worktree, cwd)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", title, err)
	}
	shell, err := terminals.Shell(os.Getenv("SHELL"))
	if err != nil {
		return nil, err
	}
	return b.Commands.Start(commands.Spec{CollectionID: c.ID, Kind: kind, Action: entry, Repository: m.RepositoryID, StopGraceMs: StopGrace.Milliseconds(), Title: title, Cwd: dir, Argv: []string{shell, "-l", "-c", script}, Env: env})
}

// RunWorkspaceSetup copies the member's listed files from its source checkout
// and runs its setup script, approved or written by the person.
func (b *Bench) RunWorkspaceSetup(ctx context.Context, id, memberName string) (RunState, error) {
	b.runtime.control.Lock()
	defer b.runtime.control.Unlock()
	c, err := b.workspace(id)
	if err != nil {
		return RunState{}, err
	}
	m, err := b.findMember(c, memberName)
	if err != nil {
		return RunState{}, err
	}
	return b.setupLocked(ctx, c, m)
}

func (b *Bench) setupLocked(ctx context.Context, c collections.Collection, m WorkspaceMember) (RunState, error) {
	if !m.approved() && m.Config.Setup != nil && (m.Effective.Setup == nil || m.Effective.Setup.Source != wr.FromSettings) {
		return RunState{}, ErrScriptsNeedApproval
	}
	rec := b.runtime.record(c.ID)
	if r, ok := rec.Setup[m.Name]; ok && b.runState(ctx, r, ok).State == "running" {
		return b.runState(ctx, r, ok), nil
	}
	if len(m.Effective.Files) > 0 && m.Source != "" {
		copied, copyErr := wr.CopyFiles(m.Source, m.Worktree, m.Effective.Files)
		if err := b.runtime.update(c.ID, func(r *runtimeRecord) { r.Files[m.Name] = copied }); err != nil {
			return RunState{}, err
		}
		if copyErr != nil {
			return RunState{}, fmt.Errorf("copying files into %s: %w", m.Name, copyErr)
		}
	}
	if m.Effective.Setup == nil {
		return RunState{State: "idle"}, nil
	}
	base, _ := b.portBlock(c.ID)
	s := m.Effective.Setup
	cmd, err := b.runtimeCommand(c, m, "setup", m.Name, "Setup · "+m.Name, s.Run, s.Cwd, b.runtimeEnv(c, m, base, 0, s.Env))
	if err != nil {
		return RunState{}, err
	}
	record := runRecord{CommandID: cmd.Status().ID, StartedAt: time.Now().UnixMilli()}
	if err = b.runtime.update(c.ID, func(r *runtimeRecord) { r.Setup[m.Name] = record }); err != nil {
		return RunState{}, err
	}
	if s.Timeout > 0 {
		go b.stopAfter(cmd, time.Duration(s.Timeout)*time.Second)
	}
	return b.runState(ctx, record, true), nil
}

// stopAfter stops a script still running after its timeout.
func (b *Bench) stopAfter(cmd *commands.Command, d time.Duration) {
	ctx, cancel := context.WithTimeout(b.ctx, d)
	defer cancel()
	if _, err := cmd.Wait(ctx); err != nil && b.ctx.Err() == nil {
		_ = cmd.Cancel()
	}
}

// WorkspaceCreated runs the runtime for members just added to a workspace:
// files are copied and setup starts, for approved or person-written scripts.
// Scripts awaiting approval wait for it. Progress reports each step, for the
// workspace dialog; setup itself runs on, in the Actions panel.
func (b *Bench) WorkspaceCreated(ctx context.Context, id string, members []string, progress func(string) error) error {
	if b.runtime == nil || b.Collections == nil {
		return nil
	}
	b.runtime.control.Lock()
	defer b.runtime.control.Unlock()
	c, err := b.workspace(id)
	if err != nil {
		return err
	}
	want := map[string]bool{}
	for _, name := range members {
		want[name] = true
	}
	rec := b.runtime.record(id)
	for _, m := range b.runtimeMembers(c) {
		if len(members) > 0 && !want[m.Name] {
			continue
		}
		if _, done := rec.Setup[m.Name]; done {
			continue // A retried request: setup already started once.
		}
		say := func(message string) {
			if progress != nil {
				_ = progress(message)
			}
		}
		switch {
		case m.Error != "":
			say(m.Name + ": " + wr.ConfigPath + " has a problem; nothing from it runs. " + firstLine(m.Error))
			continue
		case !m.approved():
			say(m.Name + " has workspace scripts to review before they run")
			continue
		case m.Override.SkipSetup:
			continue
		case m.Effective.Setup == nil && len(m.Effective.Files) == 0:
			continue
		}
		if m.Effective.Setup != nil {
			say("Starting setup for " + m.Name)
		} else {
			say("Copying files into " + m.Name)
		}
		if _, err := b.setupLocked(ctx, c, m); err != nil {
			say(m.Name + ": " + err.Error())
		}
	}
	return nil
}

func firstLine(s string) string {
	line, _, _ := strings.Cut(s, "\n")
	return line
}

// StoppedElsewhere is a non-concurrent action stopped in another workspace.
type StoppedElsewhere struct{ CollectionID, WorkspaceName, Action string }

// StartWorkspaceAction starts an action (key "<member>/<name>"). A running
// action is left running unless restart is set. A non-concurrent action is
// first stopped in every other workspace of the same repository.
func (b *Bench) StartWorkspaceAction(ctx context.Context, id, key string, restart bool) ([]StoppedElsewhere, error) {
	b.runtime.control.Lock()
	defer b.runtime.control.Unlock()
	c, err := b.workspace(id)
	if err != nil {
		return nil, err
	}
	view, err := b.WorkspaceRuntime(ctx, id)
	if err != nil {
		return nil, err
	}
	var target *ActionView
	for i := range view.Actions {
		if view.Actions[i].Key == key {
			target = &view.Actions[i]
		}
	}
	if target == nil {
		return nil, fmt.Errorf("%w: no action %s in this workspace", os.ErrNotExist, key)
	}
	if target.Blocked != "" {
		return nil, errors.New(target.Blocked)
	}
	for _, s := range view.Setups {
		if s.Member == target.Member && s.State.State == "running" {
			return nil, fmt.Errorf("%s is still setting up; start %s when setup finishes", s.Member, target.Name)
		}
	}
	var m WorkspaceMember
	for _, candidate := range view.Members {
		if candidate.Name == target.Member {
			m = candidate
		}
	}
	action, ok := m.Effective.ActionNamed(target.Name)
	if !ok {
		return nil, fmt.Errorf("%w: no action %s in this workspace", os.ErrNotExist, key)
	}
	if target.State.State == "running" || target.State.State == "stopping" {
		if !restart && target.State.State == "running" {
			return nil, nil
		}
		if err = b.stopAndWait(ctx, id, key, target.State.CommandID); err != nil {
			return nil, err
		}
	}
	stopped := []StoppedElsewhere{}
	if !action.Concurrent {
		for _, other := range b.runningActions(ctx) {
			if other.collection == id || other.repository != m.RepositoryID || other.name != action.Name {
				continue
			}
			if err = b.stopAndWait(ctx, other.collection, other.key, other.commandID); err != nil {
				return stopped, fmt.Errorf("could not stop %s in %s: %w", action.Name, other.workspaceName, err)
			}
			stopped = append(stopped, StoppedElsewhere{CollectionID: other.collection, WorkspaceName: other.workspaceName, Action: other.key})
		}
	}
	if view.PortBase > 0 && action.URL != "" {
		port := view.PortBase + action.Port
		if b.runtime.ports.InUse(port) {
			return stopped, fmt.Errorf("port %d (this workspace's $LAZYBOX_PORT for %s) is in use by another program; stop it (lsof -i :%d shows which) and start %s again", port, action.Name, port, action.Name)
		}
	}
	env := b.runtimeEnv(c, m, view.PortBase, action.Port, action.Env)
	cmd, err := b.runtimeCommand(c, m, "action", key, m.Name+" · "+action.Name, action.Run, action.Cwd, env)
	if err != nil {
		return stopped, err
	}
	record := runRecord{CommandID: cmd.Status().ID, StartedAt: time.Now().UnixMilli()}
	return stopped, b.runtime.update(id, func(r *runtimeRecord) { r.Actions[key] = record })
}

// StopWorkspaceAction asks a running action to stop: SIGTERM to its process
// group, SIGKILL after StopGrace. It returns at once; the action reads
// "stopping" until it has exited.
func (b *Bench) StopWorkspaceAction(ctx context.Context, id, key string) error {
	b.runtime.control.Lock()
	defer b.runtime.control.Unlock()
	if _, err := b.workspace(id); err != nil {
		return err
	}
	r, ok := b.runtime.record(id).Actions[key]
	if !ok {
		return nil
	}
	return b.requestStop(id, key, r)
}

func (b *Bench) requestStop(id, key string, r runRecord) error {
	cmd, err := b.Commands.Get(r.CommandID)
	if err != nil || cmd.Status().Done {
		return nil
	}
	if err = b.runtime.update(id, func(rec *runtimeRecord) {
		current := rec.Actions[key]
		if current.CommandID == r.CommandID {
			current.Stopped = true
			rec.Actions[key] = current
		}
	}); err != nil {
		return err
	}
	return cmd.Cancel()
}

// stopAndWait stops an action and waits until its process group is gone.
func (b *Bench) stopAndWait(ctx context.Context, id, key, commandID string) error {
	if err := b.requestStop(id, key, runRecord{CommandID: commandID}); err != nil {
		return err
	}
	cmd, err := b.Commands.Get(commandID)
	if err != nil {
		return nil
	}
	wait, cancel := context.WithTimeout(ctx, StopGrace+5*time.Second)
	defer cancel()
	state, err := cmd.Wait(wait)
	if err != nil {
		return err
	}
	if state.CleanupUnconfirmed() {
		return errors.New("its processes could not be confirmed stopped")
	}
	return nil
}

// workspaceDeleting stops the workspace's actions and setup, then runs each
// member's archive script (approved or written by the person), waiting for
// it up to its timeout. A failing archive script does not keep the
// workspace: its output stays in the command journal.
func (b *Bench) workspaceDeleting(ctx context.Context, c collections.Collection) error {
	if b.runtime == nil {
		return nil
	}
	b.runtime.control.Lock()
	defer b.runtime.control.Unlock()
	rec := b.runtime.record(c.ID)
	for key, r := range rec.Actions {
		if err := b.stopAndWait(ctx, c.ID, key, r.CommandID); err != nil {
			return fmt.Errorf("could not stop %s: %w", key, err)
		}
	}
	for _, r := range rec.Setup {
		if cmd, err := b.Commands.Get(r.CommandID); err == nil && !cmd.Status().Done {
			_ = cmd.Cancel()
			wait, cancel := context.WithTimeout(ctx, StopGrace+5*time.Second)
			_, _ = cmd.Wait(wait)
			cancel()
		}
	}
	base, _ := b.runtime.ports.Lookup(c.ID)
	for _, m := range b.runtimeMembers(c) {
		if m.Effective.Archive == nil {
			continue
		}
		if r, done := rec.Archive[m.Name]; done && b.runState(ctx, r, true).State != "running" {
			continue // A retried deletion: archive already ran.
		}
		s := m.Effective.Archive
		cmd, err := b.runtimeCommand(c, m, "archive", m.Name, "Archive · "+m.Name, s.Run, s.Cwd, b.runtimeEnv(c, m, base, 0, s.Env))
		if err != nil {
			continue
		}
		record := runRecord{CommandID: cmd.Status().ID, StartedAt: time.Now().UnixMilli()}
		if err = b.runtime.update(c.ID, func(r *runtimeRecord) { r.Archive[m.Name] = record }); err != nil {
			return err
		}
		timeout := time.Duration(s.Timeout) * time.Second
		if timeout == 0 {
			timeout = wr.DefaultArchiveTimeout * time.Second
		}
		wait, cancel := context.WithTimeout(ctx, timeout)
		_, err = cmd.Wait(wait)
		cancel()
		if err != nil {
			_ = cmd.Cancel()
			wait, cancel = context.WithTimeout(context.WithoutCancel(ctx), StopGrace+5*time.Second)
			_, _ = cmd.Wait(wait)
			cancel()
		}
	}
	return nil
}

// RuntimeSettings returns the person's runtime settings.
func (b *Bench) RuntimeSettings() wr.Settings { return b.runtime.settings.Get() }

// SaveRuntimeSettings saves them, refusing a stale revision.
func (b *Bench) SaveRuntimeSettings(next wr.Settings) (wr.Settings, error) {
	return b.runtime.settings.Save(next)
}

// agentActionView is what the lazybox_command tool tells an agent about one
// action: enough to start it and to reach what it serves.
type agentActionView struct {
	Name     string `json:"name"`
	State    string `json:"state"`
	URL      string `json:"url,omitempty"`
	Port     int    `json:"port,omitempty"`
	Default  bool   `json:"default,omitempty"`
	Blocked  string `json:"blocked,omitempty"`
	Command  string `json:"commandId,omitempty"`
	LastLine string `json:"lastLine,omitempty"`
}

// agentAction lists the workspace's actions or starts one for an agent's
// lazybox_command tool. Starting needs what starting a managed command
// needs: a coding session whose permission is not read-only or planning (in
// Ask mode the person approves the tool call first). Approving scripts and
// stopping actions stay the person's.
func (b *Bench) agentAction(ctx context.Context, e *Execution, action, name string, restart bool) (any, error) {
	if e.CollectionID == "" {
		return nil, errors.New("this conversation has no workspace, so it has no actions")
	}
	if action == "start_action" && (agent.IsAssistant(e.Purpose) || e.Options.Permission == "read-only" || e.Options.Workflow == "plan") {
		return nil, errors.New("starting a workspace action is disabled for read-only executions")
	}
	view, err := b.WorkspaceRuntime(ctx, e.CollectionID)
	if err != nil {
		return nil, err
	}
	list := func(v RuntimeView) []agentActionView {
		out := []agentActionView{}
		for _, a := range v.Actions {
			out = append(out, agentActionView{Name: a.Key, State: a.State.State, URL: a.URL, Port: a.Port, Default: a.Key == v.DefaultAction, Blocked: a.Blocked, Command: a.State.CommandID, LastLine: a.State.LastLine})
		}
		return out
	}
	if action == "actions" {
		return list(view), nil
	}
	key := ""
	matches := []string{}
	for _, a := range view.Actions {
		if a.Key == name || a.Name == name {
			matches = append(matches, a.Key)
		}
	}
	switch {
	case name == "" && view.DefaultAction != "":
		key = view.DefaultAction
	case len(matches) == 1:
		key = matches[0]
	case len(matches) > 1:
		return nil, fmt.Errorf("%q names several actions (%s); use one of those names", name, strings.Join(matches, ", "))
	default:
		return nil, fmt.Errorf("%w: no action %q in this workspace; action \"actions\" lists them", os.ErrNotExist, name)
	}
	stopped, err := b.StartWorkspaceAction(ctx, e.CollectionID, key, restart)
	if err != nil {
		return nil, err
	}
	after, err := b.WorkspaceRuntime(ctx, e.CollectionID)
	if err != nil {
		return nil, err
	}
	for _, a := range list(after) {
		if a.Name == key {
			result := map[string]any{"action": a}
			if len(stopped) > 0 {
				result["stoppedElsewhere"] = stopped
			}
			return result, nil
		}
	}
	return nil, fmt.Errorf("%w: %s", os.ErrNotExist, key)
}
