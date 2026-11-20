package codex

import (
	"context"
	"encoding/json"
	"errors"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
	"net/url"
	"runtime"
	"strings"
	"time"
)

// AccountClient uses native account RPCs without starting a thread or inference.
type AccountClient struct{ p *jsonl.Process }
type Login = accounts.Login

// ErrDeviceAuthDisabled: the ChatGPT account refuses device-code sign-in.
// Browser sign-in still works; in a Business or Enterprise workspace only an
// admin can turn device codes on.
var ErrDeviceAuthDisabled = errors.New("Device-code sign-in is off for this ChatGPT account. In a Business or Enterprise workspace, an admin turns it on.")

// ErrSignInInProgress: Codex could not listen for the browser's callback on
// loopback because another sign-in (another `codex login`) holds the port.
var ErrSignInInProgress = errors.New("Another Codex sign-in is in progress on this " + computer() + ". Finish or close it, then try again.")

func computer() string {
	if runtime.GOOS == "darwin" {
		return "Mac"
	}
	return "computer"
}

func loginError(message string) error {
	if strings.Contains(message, "Enable device code sign-in") && strings.Contains(message, "ChatGPT Security Settings") {
		return ErrDeviceAuthDisabled
	}
	// Codex's login server, when its callback port and the fallback are both
	// taken: "failed to start login server: Port 127.0.0.1:1457 is already in use".
	if strings.Contains(message, "login server") && strings.Contains(message, "already in use") {
		return ErrSignInInProgress
	}
	return errors.New("Codex sign-in did not complete")
}

func OpenAccount(ctx context.Context, binary string, env []string, cwd string, isolated bool) (*AccountClient, error) {
	binary = codexProgram(binary)
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

// Login starts a ChatGPT sign-in. A browser sign-in ("chatgpt") returns the
// provider page to open; Codex itself receives the callback on loopback and
// writes the credentials into this process's CODEX_HOME. A device sign-in
// ("chatgptDeviceCode") returns a verification page and a one-time code.
func (a *AccountClient) Login(ctx context.Context, method string) (Login, error) {
	device := method == accounts.DeviceSignIn
	if !device && method != "" && method != accounts.BrowserSignIn {
		return Login{}, errors.New("unknown sign-in method")
	}
	kind := "chatgpt"
	if device {
		kind = "chatgptDeviceCode"
	}
	raw, err := a.p.Call(ctx, "account/login/start", map[string]string{"type": kind})
	if err != nil {
		return Login{}, loginError(err.Error())
	}
	var response struct{ LoginID, AuthURL, VerificationURL, UserCode string }
	if err = json.Unmarshal(raw, &response); err != nil {
		return Login{}, err
	}
	page := response.AuthURL
	if device {
		page = response.VerificationURL
	}
	uri, err := url.Parse(page)
	if err != nil || uri.Scheme != "https" || uri.Host == "" || uri.User != nil || response.LoginID == "" || (device && response.UserCode == "") {
		return Login{}, errors.New("native Codex omitted a valid sign-in URL or code")
	}
	if !device {
		return Login{ID: response.LoginID, URL: page}, nil
	}
	return Login{ID: response.LoginID, URL: page, Code: response.UserCode}, nil
}

// CancelLogin stops a sign-in Codex is waiting on and releases its loopback
// callback port; Codex then reports the login completed without success.
func (a *AccountClient) CancelLogin(ctx context.Context, id string) error {
	_, err := a.p.Call(ctx, "account/login/cancel", map[string]string{"loginId": id})
	return err
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
