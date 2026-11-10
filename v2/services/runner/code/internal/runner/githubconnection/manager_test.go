package githubconnection

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/sources/github"
)

type fakeAPI struct {
	github.API
	mu                sync.Mutex
	outcomes          []github.Token
	refreshes, begins int
	userID            int64
	refreshError      error
	userError         error
}

func (f *fakeAPI) Begin(context.Context, string) (github.Device, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.begins++
	return github.Device{DeviceCode: "device-secret", UserCode: "ABCD-1234", VerificationURI: "https://github.com/login/device", ExpiresIn: 900, Interval: 1}, nil
}
func (f *fakeAPI) Exchange(context.Context, string, string) (github.Token, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if len(f.outcomes) == 0 {
		return github.Token{Error: "authorization_pending"}, nil
	}
	out := f.outcomes[0]
	f.outcomes = f.outcomes[1:]
	return out, nil
}
func (f *fakeAPI) User(context.Context, string) (github.User, error) {
	id := f.userID
	if id == 0 {
		id = 42
	}
	return github.User{ID: id, Login: "fixture"}, f.userError
}
func (f *fakeAPI) Refresh(context.Context, string, string) (github.Token, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.refreshes++
	return token("new"), f.refreshError
}
func token(suffix string) github.Token {
	return github.Token{AccessToken: "ghu_" + suffix, RefreshToken: "ghr_" + suffix, ExpiresIn: 28800, RefreshExpiresIn: 100000}
}
func managerTest(t *testing.T, api *fakeAPI) (*Manager, *FileStore) {
	t.Helper()
	store, err := NewFileStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	m, err := New(store, api)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(m.Close)
	m.wait = func(ctx context.Context, _ time.Duration) bool { return wait(ctx, time.Millisecond) }
	if _, err = m.Configure(App{ClientID: "Iv.fixture", Slug: "fixture-app"}); err != nil {
		t.Fatal(err)
	}
	return m, store
}
func awaitFlow(t *testing.T, m *Manager, want string) Enrollment {
	t.Helper()
	deadline := time.Now().Add(time.Second)
	for time.Now().Before(deadline) {
		f, _ := m.Enrollment("")
		if f.State == want {
			return f
		}
		time.Sleep(time.Millisecond)
	}
	f, _ := m.Enrollment("")
	t.Fatalf("wanted %s, got %+v", want, f)
	return f
}
func TestEnrollmentPersistenceAndRefreshIsolation(t *testing.T) {
	api := &fakeAPI{outcomes: []github.Token{{Error: "authorization_pending"}, {Error: "slow_down"}, token("first")}}
	m, store := managerTest(t, api)
	a, err := m.Begin(context.Background(), "same")
	if err != nil {
		t.Fatal(err)
	}
	b, err := m.Begin(context.Background(), "same")
	if err != nil || a.ID != b.ID {
		t.Fatal("duplicate enrollment", err)
	}
	f := awaitFlow(t, m, "connected")
	if f.UserCode != "" || f.SignInURL != "" {
		t.Fatal("completed flow retained code")
	}
	wire, _ := json.Marshal(m.Status())
	if strings.Contains(string(wire), "ghu_") || strings.Contains(string(wire), "ghr_") {
		t.Fatal("public state leaked token")
	}
	info, err := os.Stat(store.path)
	if err != nil || info.Mode().Perm() != 0600 {
		t.Fatal("credential mode", err)
	}
	m.Close()
	restored, err := New(store, api)
	if err != nil {
		t.Fatal(err)
	}
	defer restored.Close()
	if restored.Status().Login != "fixture" {
		t.Fatal("lost identity")
	}
	restored.mu.Lock()
	restored.saved.ExpiresAt = 1
	restored.mu.Unlock()
	var wg sync.WaitGroup
	for j := 0; j < 8; j++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if err := restored.WithCredential(context.Background(), func(value string) error {
				if value != "ghu_new" {
					t.Error("wrong token")
				}
				return nil
			}); err != nil {
				t.Error(err)
			}
		}()
	}
	wg.Wait()
	if api.refreshes != 1 {
		t.Fatalf("refresh raced: %d", api.refreshes)
	}
	if _, err = restored.Configure(App{ClientID: "Iv.other", Slug: "other"}); err == nil {
		t.Fatal("overwrote active registration")
	}
	if err = restored.Disconnect(); err != nil {
		t.Fatal(err)
	}
	data, _ := store.Load()
	if strings.Contains(string(data), "ghu_") || strings.Contains(string(data), "ghr_") {
		t.Fatal("disconnect retained auth")
	}
	if restored.Status().State != "not_connected" {
		t.Fatal(restored.Status())
	}
}
func TestCancellationDenialAndWrongAccount(t *testing.T) {
	m, _ := managerTest(t, &fakeAPI{})
	flow, err := m.Begin(context.Background(), "cancel")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = m.Cancel("unknown"); err == nil {
		t.Fatal("cancelled wrong flow")
	}
	cancelled, err := m.Cancel(flow.ID)
	if err != nil || cancelled.State != "cancelled" || cancelled.UserCode != "" {
		t.Fatal(cancelled, err)
	}
	api := &fakeAPI{outcomes: []github.Token{{Error: "access_denied"}}}
	denied, _ := managerTest(t, api)
	denied.Begin(context.Background(), "denied")
	awaitFlow(t, denied, "denied")
	wrong, _ := managerTest(t, &fakeAPI{userID: 99, outcomes: []github.Token{token("wrong")}})
	wrong.saved.User = github.User{ID: 42, Login: "original"}
	wrong.Begin(context.Background(), "wrong")
	awaitFlow(t, wrong, "failed")
	if wrong.Status().Login != "original" {
		t.Fatal("replaced original identity")
	}
}

type failingStore struct{ Store }

func (failingStore) Save([]byte) error { return errors.New("fake disk failure") }
func TestRefreshFailureAndRevocationFailClosed(t *testing.T) {
	m, _ := managerTest(t, &fakeAPI{})
	m.saved.Token = token("old")
	m.saved.ExpiresAt = 1
	m.saved.RefreshExpiresAt = time.Now().Add(time.Hour).Unix()
	m.store = failingStore{m.store}
	used := false
	err := m.WithCredential(context.Background(), func(string) error { used = true; return nil })
	if err == nil || used || m.Status().State != "needs_attention" {
		t.Fatal("refresh save failure did not block use")
	}
	_, err = m.credential(context.Background())
	if !errors.Is(err, github.ErrUnauthorized) {
		t.Fatal(err)
	}
	other, _ := managerTest(t, &fakeAPI{})
	other.saved.Token = token("valid")
	other.saved.ExpiresAt = time.Now().Add(time.Hour).Unix()
	err = other.WithCredential(context.Background(), func(string) error { return github.ErrUnauthorized })
	if !errors.Is(err, github.ErrUnauthorized) || other.Status().State != "needs_attention" {
		t.Fatal(err)
	}
}
func TestPrivateStoreRejectsSymlinksPermissionsAndCorruption(t *testing.T) {
	if _, err := NewFileStore("relative"); err == nil {
		t.Fatal("accepted relative root")
	}
	root := t.TempDir()
	store, _ := NewFileStore(root)
	target := filepath.Join(root, "target")
	os.WriteFile(target, []byte("secret"), 0600)
	os.Symlink(target, store.path)
	if _, err := store.Load(); err == nil {
		t.Fatal("read symlink")
	}
	os.Remove(store.path)
	os.WriteFile(store.path, []byte(`{}`), 0644)
	if _, err := store.Load(); err == nil {
		t.Fatal("read public credentials")
	}
	os.Chmod(store.path, 0600)
	if _, err := New(store, &fakeAPI{}); err == nil {
		t.Fatal("accepted corrupt store")
	}
}

func TestDisconnectBeforeSetupDoesNotCreateInvalidState(t *testing.T) {
	store, err := NewFileStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	m, err := New(store, &fakeAPI{})
	if err != nil {
		t.Fatal(err)
	}
	if err = m.Disconnect(); err != nil {
		t.Fatal(err)
	}
	m.Close()
	if err = m.Disconnect(); err == nil {
		t.Fatal("modified closed connection")
	}
	reopened, err := New(store, &fakeAPI{})
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if reopened.Status().State != "setup_required" {
		t.Fatal("unexpected state")
	}
}

func TestRefreshUnauthorizedAndStaleOperationIsolation(t *testing.T) {
	m, _ := managerTest(t, &fakeAPI{refreshError: github.ErrUnauthorized})
	m.saved.Token = token("old")
	m.saved.ExpiresAt = 1
	m.saved.RefreshExpiresAt = time.Now().Add(time.Hour).Unix()
	if err := m.WithCredential(context.Background(), func(string) error { t.Fatal("used rejected credential"); return nil }); !errors.Is(err, github.ErrUnauthorized) {
		t.Fatal(err)
	}
	if m.Status().State != "needs_attention" {
		t.Fatal("refresh rejection was hidden")
	}
	other, _ := managerTest(t, &fakeAPI{})
	other.saved.Token = token("old")
	other.saved.ExpiresAt = time.Now().Add(time.Hour).Unix()
	// A request finishing after a disconnect must not mark the new state as bad.
	err := other.WithCredential(context.Background(), func(string) error {
		if err := other.Disconnect(); err != nil {
			t.Fatal(err)
		}
		return github.ErrUnauthorized
	})
	if !errors.Is(err, github.ErrUnauthorized) || other.Status().State != "not_connected" {
		t.Fatal(err, other.Status().State)
	}
}

func TestPersonalTokenWithoutAppPersistsRevokesAndDisconnects(t *testing.T) {
	store, err := NewFileStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	api := &fakeAPI{}
	m, err := New(store, api)
	if err != nil {
		t.Fatal(err)
	}
	defer m.Close()
	ctx := context.Background()
	state, err := m.ConnectPersonalToken(ctx, "ghp_fixture_secret")
	if err != nil || state.State != "connected" || state.AuthMethod != "personal_token" || state.App.ClientID != "" {
		t.Fatal(state, err)
	}
	public, _ := json.Marshal(state)
	if strings.Contains(string(public), "ghp_") {
		t.Fatal("public state leaked credential")
	}
	if _, err := m.Begin(ctx, "app-login"); err == nil {
		t.Fatal("replaced token connection with app flow")
	}
	m.Close()
	m, err = New(store, api)
	if err != nil {
		t.Fatal(err)
	}
	defer m.Close()
	if err := m.WithAuthorization(ctx, func(token, method string) error {
		if token != "ghp_fixture_secret" || method != "personal_token" {
			t.Fatal("lost authorization mode")
		}
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	if api.refreshes != 0 {
		t.Fatal("attempted OAuth refresh for personal token")
	}
	if err := m.WithCredential(ctx, func(string) error { return github.ErrUnauthorized }); !errors.Is(err, github.ErrUnauthorized) {
		t.Fatal(err)
	}
	if m.Status().State != "needs_attention" {
		t.Fatal("revoked token not surfaced")
	}
	if _, err := m.ConnectPersonalToken(ctx, "github_pat_replacement"); err != nil {
		t.Fatal(err)
	}
	if err := m.Disconnect(); err != nil {
		t.Fatal(err)
	}
	data, err := store.Load()
	if err != nil || strings.Contains(string(data), "replacement") {
		t.Fatal("disconnect retained credential", err)
	}
	m.Close()
	reopened, err := New(store, api)
	if err != nil {
		t.Fatal("empty disconnected record cannot reopen", err)
	}
	defer reopened.Close()
	if reopened.Status().State != "setup_required" {
		t.Fatal(reopened.Status())
	}
}

func TestPersonalTokenRejectsReplacementAndPreservesSavedCredentialOnFailure(t *testing.T) {
	api := &fakeAPI{}
	m, store := managerTest(t, api)
	ctx := context.Background()
	for _, bad := range []string{"", "ghu_app_token", "ghp_a\nsecret", "not-a-token", strings.Repeat("x", 4097)} {
		if _, err := m.ConnectPersonalToken(ctx, bad); err == nil {
			t.Fatal("accepted invalid credential")
		}
	}
	if _, err := m.ConnectPersonalToken(ctx, "ghp_original_secret"); err != nil {
		t.Fatal(err)
	}
	before, _ := store.Load()
	api.userError = github.ErrUnauthorized
	if _, err := m.ConnectPersonalToken(ctx, "ghp_rejected_secret"); err == nil {
		t.Fatal("accepted rejected token")
	}
	api.userError = nil
	api.userID = 99
	if _, err := m.ConnectPersonalToken(ctx, "ghp_other_account"); err == nil {
		t.Fatal("silently switched account")
	}
	api.userID = 0
	m.store = failingStore{store}
	if _, err := m.ConnectPersonalToken(ctx, "ghp_disk_failure"); err == nil {
		t.Fatal("ignored persistence failure")
	}
	after, _ := store.Load()
	if string(before) != string(after) || m.saved.Token.AccessToken != "ghp_original_secret" {
		t.Fatal("failed connection replaced original")
	}
	m.store = store
	if err := m.Disconnect(); err != nil {
		t.Fatal(err)
	}
	flow, err := m.Begin(ctx, "app-flow")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := m.ConnectPersonalToken(ctx, "ghp_during_app_flow"); err == nil {
		t.Fatal("replaced pending authorization")
	}
	m.Cancel(flow.ID)
	m.saved.Token = token("existing-app")
	if _, err := m.ConnectPersonalToken(ctx, "ghp_replaces_app"); err == nil {
		t.Fatal("overwrote app connection")
	}
	m.Close()
	if _, err := m.ConnectPersonalToken(ctx, "ghp_closed_manager"); err == nil {
		t.Fatal("used closed manager")
	}
}
