package codex

import (
	"context"
	"encoding/json"
	"errors"
	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/adapters/jsonl"
	"net/url"
	"strings"
	"time"
)

// AccountClient uses native account RPCs without starting a thread or inference.
type AccountClient struct{ p *jsonl.Process }
type Login = accounts.Login

// ErrDeviceAuthDisabled identifies the provider's required account setting.
var ErrDeviceAuthDisabled = errors.New("Enable device-code sign-in in ChatGPT Settings → Security, then try adding Codex again.")

func loginError(message string) error {
	if strings.Contains(message, "Enable device code sign-in") && strings.Contains(message, "ChatGPT Security Settings") {
		return ErrDeviceAuthDisabled
	}
	return errors.New("Codex sign-in did not complete")
}

func OpenAccount(ctx context.Context, binary string, env []string, cwd string, isolated bool) (*AccountClient, error) {
	if binary == "" {
		binary = "codex"
	}
	args := []string{"app-server", "--stdio"}
	if isolated {
		args = append(args, "-c", `cli_auth_credentials_store="file"`)
	}
	p, err := jsonl.Start(ctx, binary, args, env, cwd)
	if err != nil {
		return nil, err
	}
	setup, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	if _, err = p.Call(setup, "initialize", map[string]any{"clientInfo": map[string]string{"name": "lazybox_subscriptions", "version": "0.1.0"}}); err != nil {
		p.Close()
		return nil, err
	}
	if err = p.Notify("initialized", map[string]any{}); err != nil {
		p.Close()
		return nil, err
	}
	return &AccountClient{p: p}, nil
}
func (a *AccountClient) Login(ctx context.Context) (Login, error) {
	raw, err := a.p.Call(ctx, "account/login/start", map[string]string{"type": "chatgptDeviceCode"})
	if err != nil {
		return Login{}, loginError(err.Error())
	}
	var response struct{ LoginID, VerificationURL, UserCode string }
	if err = json.Unmarshal(raw, &response); err != nil {
		return Login{}, err
	}
	uri, err := url.Parse(response.VerificationURL)
	if err != nil || uri.Scheme != "https" || uri.Host == "" || uri.User != nil || response.LoginID == "" || response.UserCode == "" {
		return Login{}, errors.New("native Codex omitted a valid device sign-in URL/code")
	}
	return Login{ID: response.LoginID, URL: response.VerificationURL, Code: response.UserCode}, nil
}
func (a *AccountClient) WaitLogin(ctx context.Context, id string) error {
	for {
		event, err := a.p.Receive(ctx)
		if err != nil {
			return err
		}
		if event.Method != "account/login/completed" {
			continue
		}
		var response struct {
			LoginID string
			Success bool
			Error   string
		}
		if err = json.Unmarshal(event.Params, &response); err != nil {
			return err
		}
		if response.LoginID != "" && response.LoginID != id {
			continue
		}
		if !response.Success {
			return loginError(response.Error)
		}
		return nil
	}
}
func (a *AccountClient) Usage(ctx context.Context) (*agent.AccountObservation, error) {
	account, err := a.p.Call(ctx, "account/read", map[string]bool{"refreshToken": false})
	if err != nil {
		return nil, err
	}
	var identity struct {
		Account            json.RawMessage
		RequiresOpenaiAuth bool
	}
	if err = json.Unmarshal(account, &identity); err != nil {
		return nil, err
	}
	if identity.RequiresOpenaiAuth && (len(identity.Account) == 0 || string(identity.Account) == "null") {
		return &agent.AccountObservation{State: "auth_required", Reason: "native_account_read"}, nil
	}
	raw, err := a.p.Call(ctx, "account/rateLimits/read", map[string]any{})
	if err != nil {
		return nil, err
	}
	var accountIdentity struct{ Email string }
	if err := json.Unmarshal(identity.Account, &accountIdentity); err != nil && len(identity.Account) > 0 {
		return nil, err
	}
	observation := rateLimits(raw)
	observation.Email = strings.TrimSpace(accountIdentity.Email)
	return observation, nil
}
func (a *AccountClient) Close() error { return a.p.Close() }
