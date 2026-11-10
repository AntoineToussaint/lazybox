package claude

import (
	"context"
	"encoding/json"
	"errors"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/adapters/jsonl"
)

// AccountClient delegates OAuth, callback validation and credential storage to
// Claude Code. BROWSER hands its native URL to the client; we do not construct
// OAuth requests, exchange codes or read credentials from the system Keychain.
type AccountClient struct {
	ctx                   context.Context
	cancel                context.CancelFunc
	binary, home, scratch string
	env                   []string
	mu                    sync.Mutex
	command               *exec.Cmd
	done                  chan struct{}
	err                   error
	closeOnce             sync.Once
}

func OpenAccount(ctx context.Context, binary string, env []string, home string) (*AccountClient, error) {
	if binary == "" {
		binary = "claude"
	}
	if !filepath.IsAbs(home) {
		return nil, errors.New("private Claude directory required")
	}
	scratch, err := os.MkdirTemp(home, ".login-")
	if err != nil {
		return nil, err
	}
	// The native CLI invokes BROWSER with the browser URL (localhost callback),
	// whereas stdout contains a different URL for manually pasting an auth code.
	helper := filepath.Join(scratch, "browser")
	if err = os.WriteFile(helper, []byte("#!/bin/sh\numask 077\nprintf '%s' \"$1\" > \"$LAZYBOX_LOGIN_URL_FILE\"\n"), 0700); err != nil {
		os.RemoveAll(scratch)
		return nil, err
	}
	filtered := []string{}
	for _, value := range env {
		key, _, _ := strings.Cut(value, "=")
		if key != "BROWSER" && key != "LAZYBOX_LOGIN_URL_FILE" && key != "DISABLE_LOGIN_COMMAND" {
			filtered = append(filtered, value)
		}
	}
	// Claude invokes BROWSER as an executable path, including paths with spaces.
	browser := helper
	filtered = append(filtered, "BROWSER="+browser, "LAZYBOX_LOGIN_URL_FILE="+filepath.Join(scratch, "url"))
	life, cancel := context.WithCancel(ctx)
	return &AccountClient{ctx: life, cancel: cancel, binary: binary, home: home, scratch: scratch, env: filtered}, nil
}

func (a *AccountClient) Login(ctx context.Context) (accounts.Login, error) {
	a.mu.Lock()
	if a.command != nil || a.ctx.Err() != nil {
		a.mu.Unlock()
		return accounts.Login{}, errors.New("login already started or closed")
	}
	cmd := exec.CommandContext(a.ctx, a.binary, "auth", "login", "--claudeai")
	cmd.Env = a.env
	cmd.Dir = a.home
	stdin, err := cmd.StdinPipe()
	if err != nil {
		a.mu.Unlock()
		return accounts.Login{}, err
	}
	if err = cmd.Start(); err != nil {
		stdin.Close()
		a.mu.Unlock()
		return accounts.Login{}, errors.New("Claude login could not start")
	}
	a.command = cmd
	a.done = make(chan struct{})
	done := a.done
	go func() {
		err := cmd.Wait()
		stdin.Close()
		a.mu.Lock()
		a.err = err
		a.mu.Unlock()
		close(done)
	}()
	a.mu.Unlock()
	ticker := time.NewTicker(50 * time.Millisecond)
	defer ticker.Stop()
	deadline, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	for {
		if data, err := os.ReadFile(filepath.Join(a.scratch, "url")); err == nil && len(data) > 0 {
			uri, err := url.Parse(string(data))
			if err != nil || uri.Scheme != "https" || uri.User != nil || uri.Port() != "" || (uri.Hostname() != "claude.com" && uri.Hostname() != "claude.ai") {
				return accounts.Login{}, errors.New("Claude returned an unsupported sign-in URL")
			}
			callback, err := url.Parse(uri.Query().Get("redirect_uri"))
			if err != nil || callback.Scheme != "http" || (callback.Hostname() != "localhost" && callback.Hostname() != "127.0.0.1") || callback.Port() == "" {
				return accounts.Login{}, errors.New("Claude did not provide its local callback")
			}
			os.Remove(filepath.Join(a.scratch, "url"))
			return accounts.Login{ID: "claude-native", URL: uri.String()}, nil
		}
		select {
		case <-deadline.Done():
			return accounts.Login{}, deadline.Err()
		case <-a.ctx.Done():
			return accounts.Login{}, a.ctx.Err()
		case <-done:
			return accounts.Login{}, errors.New("Claude exited before sign-in was ready")
		case <-ticker.C:
		}
	}
}
func (a *AccountClient) WaitLogin(ctx context.Context, _ string) error {
	a.mu.Lock()
	done := a.done
	a.mu.Unlock()
	if done == nil {
		return errors.New("login has not started")
	}
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-done:
	}
	a.mu.Lock()
	err := a.err
	a.mu.Unlock()
	if err != nil {
		return errors.New("Claude sign-in did not complete")
	}
	// Verify the persisted login from a fresh process, including its exact store.
	cmd := exec.CommandContext(ctx, a.binary, "auth", "status")
	cmd.Env = a.env
	cmd.Dir = a.home
	output, err := cmd.Output()
	if err != nil {
		return errors.New("Claude could not confirm saved authentication")
	}
	var status struct {
		LoggedIn                    bool
		AuthMethod, ConfigDirectory string
	}
	if json.Unmarshal(output, &status) != nil || !status.LoggedIn || status.AuthMethod != "claude.ai" {
		return errors.New("Claude did not confirm a saved subscription login")
	}
	actual, err := filepath.EvalSymlinks(status.ConfigDirectory)
	expected, e := filepath.EvalSymlinks(a.home)
	if err != nil || e != nil || actual != expected {
		return errors.New("Claude authentication directory did not match the private account")
	}
	return nil
}
func (a *AccountClient) Usage(context.Context) (*agent.AccountObservation, error) {
	return nil, errors.New("Claude reports usage through session events")
}
func (a *AccountClient) Close() error {
	a.closeOnce.Do(func() {
		a.cancel()
		a.mu.Lock()
		done := a.done
		a.mu.Unlock()
		if done != nil {
			<-done
		}
		os.RemoveAll(a.scratch)
	})
	return nil
}

// ReadUsage queries the pinned native SDK without submitting a prompt or creating
// an Engine execution. Provider credentials stay inside the isolated worker.
func ReadUsage(ctx context.Context, worker, binary string, env []string, cwd string) (*agent.AccountObservation, error) {
	if worker == "" {
		return nil, errors.New("Claude SDK worker is not configured")
	}
	p, err := jsonl.Start(ctx, "node", []string{worker}, env, cwd)
	if err != nil {
		return nil, err
	}
	defer p.Close()
	raw, err := p.Call(ctx, "account_usage", map[string]string{"cwd": cwd, "binary": binary})
	if err != nil {
		return nil, err
	}
	var observation agent.AccountObservation
	if err = json.Unmarshal(raw, &observation); err != nil {
		return nil, err
	}
	return &observation, nil
}
