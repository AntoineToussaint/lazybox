// Package githubconnection owns one isolated GitHub identity on a Runner.
// Provider wire behavior and credential persistence are independent boundaries.
package githubconnection

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
)

type App struct {
	ClientID string `json:"clientId"`
	Slug     string `json:"slug"`
}
type Status struct {
	App                 App
	AuthMethod          string
	State, Login, Error string
	UserID              int64
	ExpiresAt           int64
}
type Enrollment struct {
	ID, State, UserCode, SignInURL, Error string
	ExpiresAt                             int64
}
type stored struct {
	Version                     int          `json:"version"`
	AuthMethod                  string       `json:"authMethod,omitempty"`
	App                         App          `json:"app"`
	User                        github.User  `json:"user"`
	Token                       github.Token `json:"token"`
	ExpiresAt, RefreshExpiresAt int64
}

// Store is deliberately separate from generic session history and metadata.
type Store interface {
	Load() ([]byte, error)
	Save([]byte) error
}
type FileStore struct{ path string }

func NewFileStore(root string) (*FileStore, error) {
	if !filepath.IsAbs(root) {
		return nil, errors.New("absolute Runner connection root required")
	}
	if err := os.MkdirAll(root, 0700); err != nil {
		return nil, err
	}
	return &FileStore{path: filepath.Join(root, "github.json")}, nil
}
func (s *FileStore) Load() ([]byte, error) {
	info, err := os.Lstat(s.path)
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() || info.Mode().Perm()&0077 != 0 || info.Size() > 64<<10 {
		return nil, errors.New("GitHub credential file must be a private regular file")
	}
	return os.ReadFile(s.path)
}
func (s *FileStore) Save(data []byte) error {
	f, err := os.CreateTemp(filepath.Dir(s.path), ".github-")
	if err != nil {
		return err
	}
	name := f.Name()
	defer os.Remove(name)
	if err = f.Chmod(0600); err == nil {
		_, err = f.Write(data)
	}
	if err == nil {
		err = f.Sync()
	}
	err = errors.Join(err, f.Close())
	if err != nil {
		return err
	}
	if err = os.Rename(name, s.path); err != nil {
		return err
	}
	dir, err := os.Open(filepath.Dir(s.path))
	if err != nil {
		return err
	}
	defer dir.Close()
	return dir.Sync()
}

type Manager struct {
	mu         sync.Mutex
	api        github.API
	store      Store
	saved      stored
	problem    string
	flow       Enrollment
	flowKey    string
	flowCancel context.CancelFunc
	ctx        context.Context
	cancel     context.CancelFunc
	wg         sync.WaitGroup
	closed     bool
	now        func() time.Time
	wait       func(context.Context, time.Duration) bool
	// refreshing is the token refresh in flight, if any: one at a time,
	// every caller that needs it waits for the same one.
	refreshing *refreshCall
}

// refreshCall is one refresh of an expiring GitHub App token. It runs on the
// manager's own context, not a caller's: GitHub rotates both tokens on a
// refresh, so a refresh abandoned halfway would lose the new pair and leave
// the old refresh token spent.
type refreshCall struct {
	done  chan struct{}
	token string
	err   error
}

// refreshTimeout bounds one refresh.
const refreshTimeout = 30 * time.Second

var clientIDPattern = regexp.MustCompile(`^[A-Za-z0-9._-]{3,128}$`)
var slugPattern = regexp.MustCompile(`^[a-z0-9][a-z0-9-]{0,99}$`)

func New(store Store, api github.API) (*Manager, error) {
	ctx, cancel := context.WithCancel(context.Background())
	m := &Manager{api: api, store: store, ctx: ctx, cancel: cancel, now: time.Now, wait: wait}
	data, err := store.Load()
	if errors.Is(err, os.ErrNotExist) {
		m.saved.Version = 1
		return m, nil
	}
	if err != nil {
		cancel()
		return nil, errors.New("cannot read private GitHub connection")
	}
	if json.Unmarshal(data, &m.saved) != nil || !validStored(m.saved) {
		cancel()
		return nil, errors.New("invalid GitHub connection record; preserve it for inspection")
	}
	return m, nil
}
func validStored(s stored) bool {
	if s.Version != 1 || (s.App != (App{}) && !validApp(s.App)) {
		return false
	}
	switch s.AuthMethod {
	case "personal_token":
		return validPersonalToken(s.Token.AccessToken) && s.User.ID > 0 && s.User.Login != "" && s.Token.RefreshToken == ""
	case "", "github_app":
		return validApp(s.App) || (s.AuthMethod == "" && s.App == (App{}) && s.Token == (github.Token{}) && s.User == (github.User{}))
	default:
		return false
	}
}
func validPersonalToken(token string) bool {
	return len(token) >= 10 && len(token) <= 4096 && strings.TrimSpace(token) == token &&
		!strings.ContainsAny(token, "\r\n\t ") && (strings.HasPrefix(token, "ghp_") || strings.HasPrefix(token, "github_pat_"))
}

// ConnectPersonalToken validates the identity before atomically storing an
// explicitly supplied credential. It never resolves ambient CLI credentials.
func (m *Manager) ConnectPersonalToken(ctx context.Context, token string) (Status, error) {
	if !validPersonalToken(token) {
		return Status{}, errors.New("enter a GitHub personal access token (classic or fine-grained)")
	}
	// The identity is read without the lock (a slow GitHub must not hold up
	// every credential use); the checks below are made again after it.
	if err := m.personalTokenAllowed(); err != nil {
		return Status{}, err
	}
	user, err := m.api.User(ctx, token)
	if err != nil {
		return Status{}, err
	}
	if user.ID <= 0 || user.Login == "" {
		return Status{}, errors.New("GitHub omitted the authenticated identity")
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if err := m.personalTokenAllowedLocked(); err != nil {
		return Status{}, err
	}
	if m.saved.User.ID != 0 && m.saved.User.ID != user.ID {
		return Status{}, errors.New("this token belongs to a different GitHub account; disconnect the existing connection first")
	}
	if err := m.persist(stored{Version: 1, App: m.saved.App, AuthMethod: "personal_token", User: user, Token: github.Token{AccessToken: token}}); err != nil {
		return Status{}, err
	}
	m.problem = ""
	m.flow = Enrollment{}
	m.flowKey = ""
	return m.status(), nil
}
func (m *Manager) personalTokenAllowed() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.personalTokenAllowedLocked()
}

func (m *Manager) personalTokenAllowedLocked() error {
	if m.closed {
		return errors.New("GitHub connection is closed")
	}
	if m.flow.State == "awaiting_login" || (m.saved.Token.AccessToken != "" && m.saved.AuthMethod != "personal_token") {
		return errors.New("disconnect the existing GitHub connection before connecting a personal token")
	}
	return nil
}
func validApp(app App) bool {
	return clientIDPattern.MatchString(app.ClientID) && slugPattern.MatchString(app.Slug)
}
func wait(ctx context.Context, d time.Duration) bool {
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-timer.C:
		return true
	}
}
func (m *Manager) persist(next stored) error {
	data, err := json.Marshal(next)
	if err == nil {
		err = m.store.Save(data)
	}
	if err != nil {
		return errors.New("could not save private GitHub connection")
	}
	m.saved = next
	return nil
}
func (m *Manager) Configure(app App) (Status, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return Status{}, errors.New("GitHub connection is closed")
	}
	if !validApp(app) {
		return Status{}, errors.New("GitHub App client ID and app slug required")
	}
	if m.saved.App == app {
		return m.status(), nil
	}
	if m.saved.Token.AccessToken != "" || m.flow.State == "awaiting_login" {
		return Status{}, errors.New("disconnect the existing GitHub connection before changing its App")
	}
	err := m.persist(stored{Version: 1, App: app})
	return m.status(), err
}
func (m *Manager) status() Status {
	s := Status{App: m.saved.App, AuthMethod: m.saved.AuthMethod, State: "not_connected", Login: m.saved.User.Login, UserID: m.saved.User.ID, ExpiresAt: m.saved.ExpiresAt, Error: m.problem}
	if m.saved.Token.AccessToken != "" {
		s.State = "connected"
		if s.AuthMethod == "" {
			s.AuthMethod = "github_app"
		}
	} else if !validApp(m.saved.App) {
		s.State = "setup_required"
	}
	if m.problem != "" {
		s.State = "needs_attention"
	}
	return s
}
func (m *Manager) Status() Status { m.mu.Lock(); defer m.mu.Unlock(); return m.status() }
func (m *Manager) Begin(ctx context.Context, key string) (Enrollment, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return Enrollment{}, errors.New("GitHub connection is closed")
	}
	if key == "" || len(key) > 128 {
		return Enrollment{}, errors.New("bounded sign-in request key required")
	}
	if m.saved.AuthMethod == "personal_token" {
		return Enrollment{}, errors.New("disconnect the personal-token connection before signing in with an App")
	}
	if m.flowKey == key || m.flow.State == "awaiting_login" {
		return m.flow, nil
	}
	if !validApp(m.saved.App) {
		return Enrollment{}, errors.New("configure the Lazybox GitHub App first")
	}
	d, err := m.api.Begin(ctx, m.saved.App.ClientID)
	if err != nil {
		return Enrollment{}, err
	}
	m.flowKey = key
	m.flow = Enrollment{ID: rand.Text(), State: "awaiting_login", UserCode: d.UserCode, SignInURL: d.VerificationURI, ExpiresAt: m.now().Unix() + d.ExpiresIn}
	flowCtx, cancel := context.WithCancel(m.ctx)
	m.flowCancel = cancel
	m.wg.Add(1)
	go m.poll(flowCtx, m.flow.ID, m.saved.App.ClientID, d)
	return m.flow, nil
}
func (m *Manager) Enrollment(id string) (Enrollment, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.flow.ID == "" || id != "" && id != m.flow.ID {
		return Enrollment{}, os.ErrNotExist
	}
	return m.flow, nil
}
func (m *Manager) Cancel(id string) (Enrollment, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if id != m.flow.ID || id == "" {
		return Enrollment{}, os.ErrNotExist
	}
	if m.flow.State == "awaiting_login" {
		m.flowCancel()
		m.flow.State = "cancelled"
		m.flow.UserCode = ""
		m.flow.SignInURL = ""
	}
	return m.flow, nil
}
func (m *Manager) poll(ctx context.Context, id, client string, d github.Device) {
	defer m.wg.Done()
	interval := time.Duration(d.Interval) * time.Second
	if interval < time.Second {
		interval = time.Second
	}
	deadline := m.now().Add(time.Duration(d.ExpiresIn) * time.Second)
	for m.wait(ctx, interval) {
		if !m.now().Before(deadline) {
			m.finish(id, "expired", "GitHub sign-in expired. Start again.")
			return
		}
		token, err := m.api.Exchange(ctx, client, d.DeviceCode)
		if ctx.Err() != nil {
			return
		}
		if err != nil {
			m.finish(id, "failed", err.Error())
			return
		}
		switch token.Error {
		case "authorization_pending":
			continue
		case "slow_down":
			interval += 5 * time.Second
			continue
		case "access_denied":
			m.finish(id, "denied", "GitHub authorization was declined.")
			return
		case "expired_token":
			m.finish(id, "expired", "GitHub sign-in expired. Start again.")
			return
		case "":
		default:
			m.finish(id, "failed", "GitHub could not authorize this App. Check its Device Flow configuration.")
			return
		}
		if err = validToken(token); err != nil {
			m.finish(id, "failed", err.Error())
			return
		}
		user, err := m.api.User(ctx, token.AccessToken)
		if err != nil {
			m.finish(id, "failed", err.Error())
			return
		}
		m.mu.Lock()
		if m.flow.ID != id || m.flow.State != "awaiting_login" || m.closed {
			m.mu.Unlock()
			return
		}
		if m.saved.User.ID != 0 && m.saved.User.ID != user.ID {
			m.mu.Unlock()
			m.finish(id, "failed", "Signed in with a different GitHub account. Disconnect the existing connection first.")
			return
		}
		next := m.withToken(token)
		next.User = user
		err = m.persist(next)
		if err == nil {
			m.problem = ""
			m.flow.State = "connected"
		} else {
			m.flow.State = "failed"
			m.flow.Error = err.Error()
		}
		m.flow.UserCode = ""
		m.flow.SignInURL = ""
		m.mu.Unlock()
		return
	}
}
func validToken(token github.Token) error {
	if len(token.AccessToken) < 5 || token.AccessToken[:4] != "ghu_" || token.RefreshToken == "" || token.ExpiresIn <= 0 || token.RefreshExpiresIn <= 0 {
		return errors.New("GitHub did not return an expiring GitHub App authorization; check the App configuration")
	}
	return nil
}
func (m *Manager) withToken(token github.Token) stored {
	next := m.saved
	next.Token = token
	next.AuthMethod = "github_app"
	next.ExpiresAt = m.now().Unix() + token.ExpiresIn
	next.RefreshExpiresAt = m.now().Unix() + token.RefreshExpiresIn
	return next
}
func (m *Manager) finish(id, state, problem string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.flow.ID == id && m.flow.State == "awaiting_login" {
		m.flow.State = state
		m.flow.Error = problem
		m.flow.UserCode = ""
		m.flow.SignInURL = ""
	}
}
func (m *Manager) credential(ctx context.Context) (string, error) {
	token, _, err := m.authorization(ctx)
	return token, err
}

// authorization is the credential to use now and its discovery mode. An
// expiring App token is refreshed first, by one refresh at a time on the
// manager's own context (refreshCall), without holding the lock across the
// network; a caller that gives up waiting does not abandon the refresh.
func (m *Manager) authorization(ctx context.Context) (string, string, error) {
	m.mu.Lock()
	if m.closed {
		m.mu.Unlock()
		return "", "", errors.New("GitHub connection is closed")
	}
	if m.problem != "" || m.saved.Token.AccessToken == "" {
		m.mu.Unlock()
		return "", "", github.ErrUnauthorized
	}
	method := m.saved.AuthMethod
	if method == "" {
		method = "github_app"
	}
	if m.saved.AuthMethod == "personal_token" || m.now().Unix()+60 < m.saved.ExpiresAt {
		token := m.saved.Token.AccessToken
		m.mu.Unlock()
		return token, method, nil
	}
	if m.now().Unix() >= m.saved.RefreshExpiresAt {
		m.problem = github.ErrUnauthorized.Error()
		m.mu.Unlock()
		return "", "", github.ErrUnauthorized
	}
	call := m.refreshing
	if call == nil {
		call = &refreshCall{done: make(chan struct{})}
		m.refreshing = call
		m.wg.Add(1)
		go m.refresh(call, m.saved.App.ClientID, m.saved.Token.RefreshToken)
	}
	m.mu.Unlock()
	select {
	case <-call.done:
	case <-ctx.Done():
		return "", "", ctx.Err()
	}
	if call.err != nil {
		return "", "", call.err
	}
	return call.token, "github_app", nil
}

// refresh exchanges the refresh token for a new pair and saves it, whoever
// is still waiting. A result for a connection that changed meanwhile
// (disconnected, reconnected, closed) is dropped.
func (m *Manager) refresh(call *refreshCall, client, refreshToken string) {
	defer m.wg.Done()
	defer close(call.done)
	ctx, cancel := context.WithTimeout(m.ctx, refreshTimeout)
	token, err := m.api.Refresh(ctx, client, refreshToken)
	cancel()
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.refreshing == call {
		m.refreshing = nil
	}
	switch {
	case m.closed:
		call.err = errors.New("GitHub connection is closed")
		return
	case m.saved.Token.RefreshToken != refreshToken || m.saved.App.ClientID != client:
		call.err = github.ErrUnauthorized
		return
	}
	if err != nil {
		if errors.Is(err, github.ErrUnauthorized) {
			m.problem = err.Error()
		}
		call.err = err
		return
	}
	if token.Error != "" || validToken(token) != nil {
		m.problem = github.ErrUnauthorized.Error()
		call.err = github.ErrUnauthorized
		return
	}
	// Refresh rotates both tokens. A persistence failure must not reuse the old pair.
	if err = m.persist(m.withToken(token)); err != nil {
		m.problem = "GitHub refresh could not be saved; reconnect this Lazybox account"
		call.err = err
		return
	}
	call.token = m.saved.Token.AccessToken
}

// WithCredential limits secret exposure to a provider or repository operation.
func (m *Manager) WithCredential(ctx context.Context, operation func(string) error) error {
	return m.WithAuthorization(ctx, func(token, _ string) error { return operation(token) })
}

// WithAuthorization captures the credential and its discovery mode together,
// so an in-flight operation cannot combine different connection identities.
func (m *Manager) WithAuthorization(ctx context.Context, operation func(token, method string) error) error {
	token, method, err := m.authorization(ctx)
	if err != nil {
		return err
	}
	err = operation(token, method)
	if errors.Is(err, github.ErrUnauthorized) {
		m.mu.Lock()
		if m.saved.Token.AccessToken == token {
			m.problem = err.Error()
		}
		m.mu.Unlock()
	}
	return err
}
func (m *Manager) Disconnect() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return errors.New("GitHub connection is closed")
	}
	if !validApp(m.saved.App) && m.saved.Token.AccessToken == "" {
		return nil
	}
	if m.flowCancel != nil {
		m.flowCancel()
	}
	m.flow = Enrollment{}
	m.flowKey = ""
	next := stored{Version: 1, App: m.saved.App}
	// A disconnected, unconfigured Runner is a valid empty record too.
	if err := m.persist(next); err != nil {
		return err
	}
	m.problem = ""
	return nil
}
func (m *Manager) Close() { m.mu.Lock(); m.closed = true; m.cancel(); m.mu.Unlock(); m.wg.Wait() }
