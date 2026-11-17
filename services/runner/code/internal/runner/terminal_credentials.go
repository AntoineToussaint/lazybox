package runner

import (
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

// A workspace terminal is the person's own shell, but anything started in it
// (an agent CLI run by hand, a script) runs with whatever the shell holds. So
// the shell never gets the owner's bearer. Each terminal gets a credential
// bound to its own id, which the transport accepts only for publishing
// workspace runs (`lazybox log`, `lazybox run`) to the terminal's workspace.
// Like an agent credential it is an HMAC under the Runner's key, so it needs
// no storage and survives a Runner restart; it stops working when the
// terminal closes or its shell exits, and with the workspace.

// terminalTokenPrefix marks a terminal credential; no other token has it.
const terminalTokenPrefix = "lbx-terminal."

func (b *Bench) terminalEndpointDir() string {
	return filepath.Join(b.Config.State, "terminal-endpoints")
}

// TerminalEndpoint is the file a terminal's LAZYBOX_V2_ENDPOINT names.
func (b *Bench) TerminalEndpoint(id string) string {
	return filepath.Join(b.terminalEndpointDir(), id+".json")
}

func (b *Bench) terminalMAC(id string) []byte {
	mac := hmac.New(sha256.New, b.agentKey)
	// A different label from agent credentials: one can never pass as the other.
	mac.Write([]byte("lazybox v2 terminal credential\x00"))
	mac.Write([]byte(id))
	return mac.Sum(nil)
}

func (b *Bench) terminalToken(id string) string {
	return terminalTokenPrefix + id + "." + base64.RawURLEncoding.EncodeToString(b.terminalMAC(id))
}

// TerminalScope returns the terminal a credential was issued to and that
// terminal's workspace. It fails for a forged credential, for a terminal that
// was closed or whose shell exited, and for a workspace being deleted.
func (b *Bench) TerminalScope(token string) (terminal, workspace string, err error) {
	rest, ok := strings.CutPrefix(token, terminalTokenPrefix)
	if !ok {
		return "", "", errors.New("not a terminal credential")
	}
	dot := strings.LastIndexByte(rest, '.')
	if dot <= 0 || b.Terminals == nil {
		return "", "", errors.New("malformed terminal credential")
	}
	id, encoded := rest[:dot], rest[dot+1:]
	presented, err := base64.RawURLEncoding.DecodeString(encoded)
	if err != nil || !validExecutionID(id) || !hmac.Equal(presented, b.terminalMAC(id)) {
		return "", "", errors.New("invalid terminal credential")
	}
	t, err := b.Terminals.Get(id)
	if err != nil {
		return "", "", errors.New("terminal credential revoked")
	}
	state := t.Status()
	if state.Done || state.Spec.CollectionID == "" {
		return "", "", errors.New("terminal credential revoked")
	}
	if b.Collections == nil {
		return "", "", errors.New("workspaces unavailable")
	}
	c, err := b.Collections.Get(state.Spec.CollectionID)
	if err != nil || c.State == "deleting" || c.State == "deleted" {
		return "", "", errors.New("terminal credential revoked")
	}
	return id, c.ID, nil
}

// CommandTerminal returns the terminal that registered a workspace run, or ""
// for any other command.
func (b *Bench) CommandTerminal(id string) (string, error) {
	c, err := b.Commands.Get(id)
	if err != nil {
		return "", err
	}
	return c.Status().Spec.TerminalID, nil
}

// terminalCredential gives a terminal what the lazybox CLI needs to publish
// without flags: the terminal's own endpoint file and a directory for
// full-output files. It writes the endpoint file, so it runs once the
// terminal has its id and before its shell starts.
func (b *Bench) terminalCredential(collectionID string) func(string) ([]string, error) {
	return func(id string) ([]string, error) {
		if err := b.writeTerminalEndpoint(id, collectionID); err != nil {
			return nil, err
		}
		return []string{"LAZYBOX_V2_ENDPOINT=" + b.TerminalEndpoint(id), "LAZYBOX_V2_OUTPUT_DIR=" + b.workspaceOutputDir(collectionID)}, nil
	}
}

// workspaceOutputDir holds full-output files of a workspace's terminal runs;
// it is deleted with the workspace and bounded with every other output.
func (b *Bench) workspaceOutputDir(collectionID string) string {
	return filepath.Join(b.Config.OutputRoot, "workspace-"+collectionID)
}

func (b *Bench) writeTerminalEndpoint(id, collectionID string) error {
	if !validExecutionID(id) {
		return errors.New("invalid terminal identity")
	}
	dir := b.terminalEndpointDir()
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	data, err := json.Marshal(map[string]string{"url": b.Config.Address, "token": b.terminalToken(id), "collectionId": collectionID})
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(dir, ".endpoint-*")
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
	return os.Rename(f.Name(), b.TerminalEndpoint(id))
}

// refreshTerminalEndpoints runs when the Runner opens: every re-attached
// shell's endpoint file is rewritten with this Runner's URL, and the file of
// any terminal that is gone is removed. Best effort: a shell that cannot
// publish still works as a shell.
func (b *Bench) refreshTerminalEndpoints() {
	live := map[string]bool{}
	for _, t := range b.Terminals.List("") {
		if !t.Done && validExecutionID(t.ID) {
			live[t.ID] = true
			_ = b.writeTerminalEndpoint(t.ID, t.Spec.CollectionID)
		}
	}
	entries, err := os.ReadDir(b.terminalEndpointDir())
	if err != nil {
		return
	}
	for _, entry := range entries {
		if id, ok := strings.CutSuffix(entry.Name(), ".json"); !ok || !live[id] {
			_ = os.Remove(filepath.Join(b.terminalEndpointDir(), entry.Name()))
		}
	}
}

// CloseTerminal hangs a terminal up and revokes its credential: the endpoint
// file is removed and runs it left open are finished, so they neither stay
// "running" nor hold a command slot.
func (b *Bench) CloseTerminal(ctx context.Context, id string) error {
	_, err := b.Terminals.Discard(ctx, id)
	if err != nil {
		return err
	}
	return b.forgetTerminal(id)
}

func (b *Bench) forgetTerminal(id string) error {
	var errs []error
	if validExecutionID(id) {
		if err := os.Remove(b.TerminalEndpoint(id)); err != nil && !errors.Is(err, fs.ErrNotExist) {
			errs = append(errs, err)
		}
	}
	for _, state := range b.Commands.List("") {
		if state.Spec.TerminalID != id || state.Done {
			continue
		}
		if c, err := b.Commands.Get(state.ID); err == nil {
			errs = append(errs, c.FinishPipe(errors.New("the terminal was closed before the output ended")))
		}
	}
	return errors.Join(errs...)
}

// RegisterWorkspacePipe opens a workspace run: output published from a
// terminal of collectionID, shown in every conversation of that workspace.
// shareWith, when set, names the conversation (id or title) whose message box
// receives the run's compact view once it finishes.
func (b *Bench) RegisterWorkspacePipe(ctx context.Context, collectionID, terminalID, title, shareWith string) (*commands.Command, error) {
	if b.Collections == nil {
		return nil, errors.New("workspaces unavailable")
	}
	c, err := b.Collections.Get(collectionID)
	if err != nil {
		return nil, err
	}
	if c.State == "deleting" || c.State == "deleted" {
		return nil, errors.New("workspace is being deleted")
	}
	conversation := ""
	if strings.TrimSpace(shareWith) != "" {
		if conversation, err = b.ResolveConversation(ctx, collectionID, shareWith); err != nil {
			return nil, err
		}
	}
	return b.Commands.Register(commands.Spec{CollectionID: collectionID, TerminalID: terminalID, ShareWith: conversation, Title: title, Cwd: c.Root})
}

// ResolveConversation finds a conversation of a workspace by id or by title:
// an exact title first (ignoring case), else the one title containing ref.
// The error lists the workspace's conversations, so a person can pick one.
func (b *Bench) ResolveConversation(ctx context.Context, collectionID, ref string) (string, error) {
	ref = strings.TrimSpace(ref)
	var all []conversationRef
	for offset := 0; ; offset += 100 {
		records, err := b.History.Query(ctx, sessionstore.Query{Limit: 100, Offset: offset, Workspace: &collectionID})
		if err != nil {
			return "", err
		}
		for _, r := range records {
			if agent.IsAssistant(r.Purpose) {
				continue
			}
			title := r.DisplayTitle
			if title == "" {
				title = r.Title
			}
			conversation := r.ConversationID
			if conversation == "" {
				conversation = r.ID
			}
			all = append(all, conversationRef{id: conversation, execution: r.ID, title: title})
		}
		if len(records) < 100 {
			break
		}
	}
	for _, c := range all {
		if c.id == ref || c.execution == ref {
			return c.id, nil
		}
	}
	var exact, partial []conversationRef
	for _, c := range all {
		switch {
		case strings.EqualFold(c.title, ref):
			exact = append(exact, c)
		case ref != "" && strings.Contains(strings.ToLower(c.title), strings.ToLower(ref)):
			partial = append(partial, c)
		}
	}
	for _, matches := range [][]conversationRef{exact, partial} {
		if len(matches) == 1 {
			return matches[0].id, nil
		}
		if len(matches) > 1 {
			return "", fmt.Errorf("%q matches %d conversations in this workspace: %s", ref, len(matches), listConversations(matches))
		}
	}
	if len(all) == 0 {
		return "", errors.New("this workspace has no conversation yet; start one, then share with it")
	}
	return "", fmt.Errorf("no conversation %q in this workspace; choose one of: %s", ref, listConversations(all))
}

type conversationRef struct{ id, execution, title string }

func listConversations(refs []conversationRef) string {
	names := []string{}
	for i, c := range refs {
		if i == 10 {
			names = append(names, fmt.Sprintf("and %d more", len(refs)-10))
			break
		}
		names = append(names, fmt.Sprintf("%q (%s)", c.title, c.id))
	}
	return strings.Join(names, ", ")
}

// ClaimCommandShare takes a shared run for one message box, once.
func (b *Bench) ClaimCommandShare(id string) (bool, error) {
	return b.Commands.ClaimShare(id)
}

// removeWorkspaceOutput deletes a workspace's terminal-run output files.
func (b *Bench) removeWorkspaceOutput(collectionID string) error {
	if !validExecutionID(collectionID) {
		return nil
	}
	return os.RemoveAll(b.workspaceOutputDir(collectionID))
}
