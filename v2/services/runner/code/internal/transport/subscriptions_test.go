package transport

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/adapters/codex"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
)

type fakeNativeAccount struct {
	authorize  chan struct{}
	closed     atomic.Bool
	loginError error
}

func TestAccountWireIncludesProviderEmail(t *testing.T) {
	wire := wireAccount(&agent.AccountObservation{State: "available", Email: "provider@example.invalid"})
	if wire.Email != "provider@example.invalid" {
		t.Fatal("provider identity omitted")
	}
	if wireAccount(&agent.AccountObservation{State: "unknown"}).Email != "" {
		t.Fatal("invented missing identity")
	}
}

func (f *fakeNativeAccount) Login(context.Context) (codex.Login, error) {
	return codex.Login{ID: "native-login", URL: "https://auth.openai.com/codex/device", Code: "FIXTURE-CODE"}, nil
}
func (f *fakeNativeAccount) WaitLogin(ctx context.Context, _ string) error {
	select {
	case <-f.authorize:
		return f.loginError
	case <-ctx.Done():
		return ctx.Err()
	}
}
func (f *fakeNativeAccount) Usage(context.Context) (*agent.AccountObservation, error) {
	return &agent.AccountObservation{State: "available", Plan: "plus", Windows: []agent.UsageWindow{{Name: "codex:primary", UsedPercent: 45, DurationMinutes: 300, ResetAt: time.Now().Add(time.Hour).Unix()}, {Name: "codex:secondary", UsedPercent: 60, DurationMinutes: 10080}}}, nil
}
func (f *fakeNativeAccount) Close() error { f.closed.Store(true); return nil }
func enrollmentState(t *testing.T, s *Service, id, want string) *pb.SubscriptionEnrollment {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		value, err := s.GetSubscriptionEnrollment(context.Background(), &pb.SubscriptionEnrollmentRequest{EnrollmentId: id})
		if err != nil {
			t.Fatal(err)
		}
		if value.State == want {
			return value
		}
		if time.Now().After(deadline) {
			t.Fatalf("wanted %s: %+v", want, value)
		}
		time.Sleep(time.Millisecond * 5)
	}
}
func TestSettingsCodexSignInRegistersLiveAndRetainsUsageWithoutStartingAgent(t *testing.T) {
	s, a := collectionService(t)
	native := &fakeNativeAccount{authorize: make(chan struct{})}
	s.Bench.AccountFactory = func(ctx context.Context, p accounts.Profile) (runner.NativeAccount, error) {
		if p.Isolation != "environment" || !strings.HasPrefix(p.Home, s.Bench.Config.State+string(os.PathSeparator)) {
			t.Error("login escaped isolated context")
		}
		return native, nil
	}
	request := func() *pb.AddSubscriptionRequest {
		return &pb.AddSubscriptionRequest{RequestKey: "stable-request", Provider: "codex", Label: "Work"}
	}
	first, err := s.AddSubscription(context.Background(), request())
	if err != nil {
		t.Fatal(err)
	}
	retry, err := s.AddSubscription(context.Background(), request())
	if err != nil || retry.Id != first.Id {
		t.Fatal("duplicate enrollment", err)
	}
	waiting := enrollmentState(t, s, first.Id, "awaiting_login")
	if waiting.UserCode != "FIXTURE-CODE" || waiting.SignInUrl == "" {
		t.Fatal("native auth context missing")
	}
	info, err := s.Info(context.Background(), &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if len(info.PendingEnrollments) != 1 || info.PendingEnrollments[0].SignInUrl != "" || info.PendingEnrollments[0].UserCode != "" {
		t.Fatal("one-time auth data leaked into ordinary Info")
	}
	close(native.authorize)
	enrollmentState(t, s, first.Id, "added")
	deadline := time.Now().Add(5 * time.Second)
	for {
		info, _ = s.Info(context.Background(), &pb.InfoRequest{})
		var added *pb.Profile
		for _, p := range info.Profiles {
			if p.Id == first.ProfileId {
				added = p
			}
		}
		if added != nil && added.Availability != nil && len(added.Availability.Windows) == 2 {
			if added.Label != "Work" || added.Availability.Windows[0].UsedPercent != 45 || added.Availability.ObservedAtUnixMillis == 0 {
				t.Fatalf("bad usage: %+v", added)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("new account/usage not visible without restart")
		}
		time.Sleep(time.Millisecond * 5)
	}
	if a.Session != nil || len(s.Bench.List()) != 0 {
		t.Fatal("account sign-in started inference")
	}
	metadata, err := os.ReadFile(filepath.Join(s.Bench.Config.State, "accounts", "profiles.json"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(metadata), "FIXTURE-CODE") || strings.Contains(string(metadata), "auth.openai.com") {
		t.Fatal("one-time auth state persisted in account metadata")
	}
	if _, err = s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "stable-request", Provider: "codex", Label: "changed"}); err == nil {
		t.Fatal("request identity reused")
	}
	refreshed, err := s.RefreshSubscription(context.Background(), &pb.RefreshSubscriptionRequest{ProfileId: first.ProfileId})
	if err != nil || len(refreshed.Availability.Windows) != 2 {
		t.Fatal(refreshed, err)
	}
}
func TestSettingsClaudeTokenIsPrivateAndNotSavedInViewMetadata(t *testing.T) {
	s, a := collectionService(t)
	request := &pb.AddSubscriptionRequest{RequestKey: "claude-personal", Provider: "claude", Label: "Personal", SubscriptionToken: "fixture-private-subscription"}
	flow, err := s.AddSubscription(context.Background(), request)
	if err != nil {
		t.Fatal(err)
	}
	if request.SubscriptionToken != "" {
		t.Fatal("secret retained on RPC request object")
	}
	enrollmentState(t, s, flow.Id, "added")
	info, err := s.Info(context.Background(), &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	encoded, _ := json.Marshal(info)
	if strings.Contains(string(encoded), "fixture-private-subscription") {
		t.Fatal("token leaked into Info")
	}
	profiles, err := accounts.LoadProfiles(s.Bench.Config.State)
	if err != nil || len(profiles) != 1 {
		t.Fatal(profiles, err)
	}
	mode, err := os.Stat(profiles[0].TokenFile)
	if err != nil || mode.Mode().Perm() != 0600 {
		t.Fatal("private token mode", err)
	}
	if profiles[0].Label != "Personal" || a.Session != nil {
		t.Fatal("wrong label or implicit inference")
	}
	s.Bench.AccountFactory = func(context.Context, accounts.Profile) (runner.NativeAccount, error) {
		return &fakeNativeAccount{}, nil
	}
	if _, err = s.RefreshSubscription(context.Background(), &pb.RefreshSubscriptionRequest{ProfileId: flow.ProfileId}); err != nil {
		t.Fatal(err)
	}
}
func TestSettingsCancellationDoesNotRegisterOrChangeCurrentAccounts(t *testing.T) {
	s, _ := collectionService(t)
	native := &fakeNativeAccount{authorize: make(chan struct{})}
	s.Bench.AccountFactory = func(context.Context, accounts.Profile) (runner.NativeAccount, error) { return native, nil }
	flow, err := s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "cancel-request", Provider: "codex", Label: "Cancelled"})
	if err != nil {
		t.Fatal(err)
	}
	enrollmentState(t, s, flow.Id, "awaiting_login")
	if _, err = s.CancelSubscriptionEnrollment(context.Background(), &pb.SubscriptionEnrollmentRequest{EnrollmentId: flow.Id}); err != nil {
		t.Fatal(err)
	}
	enrollmentState(t, s, flow.Id, "cancelled")
	if len(s.Bench.Profiles()) != 2 {
		t.Fatal("cancelled subscription registered")
	}
	deadline := time.Now().Add(time.Second)
	for !native.closed.Load() {
		if time.Now().After(deadline) {
			t.Fatal("native login not closed")
		}
		time.Sleep(time.Millisecond)
	}
}

func TestEnrollmentReceiptRecoversAfterRunnerRestartWithoutTokenReplay(t *testing.T) {
	state := t.TempDir()
	bench, err := runner.New(runner.Config{State: state})
	if err != nil {
		t.Fatal(err)
	}
	service := &Service{Bench: bench}
	flow, err := service.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "lost-ack", Provider: "claude", Label: "Work", SubscriptionToken: "fixture-token-once"})
	if err != nil {
		t.Fatal(err)
	}
	enrollmentState(t, service, flow.Id, "added")
	if err = bench.Close(); err != nil {
		t.Fatal(err)
	}
	restored, err := runner.New(runner.Config{State: state})
	if err != nil {
		t.Fatal(err)
	}
	defer restored.Close()
	service.Bench = restored
	receipt, err := service.GetSubscriptionEnrollment(context.Background(), &pb.SubscriptionEnrollmentRequest{RequestKey: "lost-ack", Provider: "claude"})
	if err != nil || receipt.Id != flow.Id || receipt.State != "added" || receipt.SignInUrl != "" || receipt.UserCode != "" {
		t.Fatal(receipt, err)
	}
	missing, err := service.GetSubscriptionEnrollment(context.Background(), &pb.SubscriptionEnrollmentRequest{RequestKey: "never-accepted", Provider: "codex"})
	if err != nil || missing.State != "expired" {
		t.Fatal(missing, err)
	}
}

func TestSharedSubscriptionsAcrossRunnersAndRestart(t *testing.T) {
	shared, workState, devState := t.TempDir(), t.TempDir(), t.TempDir()
	work, err := runner.New(runner.Config{State: workState, SubscriptionState: shared})
	if err != nil {
		t.Fatal(err)
	}
	defer work.Close()
	dev, err := runner.New(runner.Config{State: devState, SubscriptionState: shared})
	if err != nil {
		t.Fatal(err)
	}
	defer func() { dev.Close() }()
	ws, ds := &Service{Bench: work}, &Service{Bench: dev}
	flow, err := ws.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "shared-claude", Provider: "claude", Label: "Saved account", SubscriptionToken: "fixture-shared-token"})
	if err != nil {
		t.Fatal(err)
	}
	enrollmentState(t, ws, flow.Id, "added")
	info, err := ds.Info(context.Background(), &pb.InfoRequest{})
	if err != nil || !info.SharedSubscriptions || len(info.Profiles) != 3 || !info.Profiles[2].Managed {
		t.Fatal("other Runner did not discover enrollment", info, err)
	}
	data, _ := json.Marshal(info)
	if strings.Contains(string(data), "fixture-shared-token") || strings.Contains(string(data), shared) {
		t.Fatal("private credential or storage path leaked")
	}
	// Enrollment in the other direction uses the same private provider home.
	authorized := make(chan struct{})
	close(authorized)
	dev.AccountFactory = func(_ context.Context, p accounts.Profile) (runner.NativeAccount, error) {
		if !strings.HasPrefix(p.Home, filepath.Join(shared, "accounts")+string(os.PathSeparator)) {
			t.Error("login used Runner-local storage", p.Home)
		}
		return &fakeNativeAccount{authorize: authorized}, nil
	}
	codex, err := ds.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "shared-codex", Provider: "codex", Label: "Shared Codex"})
	if err != nil {
		t.Fatal(err)
	}
	enrollmentState(t, ds, codex.Id, "added")
	info, err = ws.Info(context.Background(), &pb.InfoRequest{})
	if err != nil || len(info.Profiles) != 4 {
		t.Fatal("Working did not discover Dev enrollment", info, err)
	}
	saved, err := accounts.LoadProfiles(shared)
	if err != nil || len(saved) != 2 {
		t.Fatal(saved, err)
	}
	for _, state := range []string{workState, devState} {
		local, err := accounts.LoadProfiles(state)
		if err != nil || len(local) != 0 {
			t.Fatal("credentials copied into channel", local, err)
		}
	}
	if err = dev.Close(); err != nil {
		t.Fatal(err)
	}
	dev, err = runner.New(runner.Config{State: devState, SubscriptionState: shared})
	if err != nil {
		t.Fatal(err)
	}
	ds.Bench = dev
	info, err = ds.Info(context.Background(), &pb.InfoRequest{})
	if err != nil || len(info.Profiles) != 4 {
		t.Fatal("shared subscriptions lost on restart", info, err)
	}
	after, _ := accounts.LoadProfiles(shared)
	for i := range saved {
		if saved[i].Home != after[i].Home || saved[i].TokenFile != after[i].TokenFile {
			t.Fatal("restart relocated native credential store")
		}
	}
	if _, err = runner.New(runner.Config{State: t.TempDir(), SubscriptionState: "relative"}); err == nil {
		t.Fatal("accepted relative shared store")
	}
}

func TestSettingsCannotRefreshOrManageLocalCliAuthentication(t *testing.T) {
	service, _ := collectionService(t)
	calls := 0
	service.Bench.AccountFactory = func(context.Context, accounts.Profile) (runner.NativeAccount, error) { calls++; return nil, nil }
	for _, id := range []string{"claude-default", "codex-default"} {
		if _, err := service.RefreshSubscription(context.Background(), &pb.RefreshSubscriptionRequest{ProfileId: id}); err == nil {
			t.Fatal("Settings accessed local CLI authentication")
		}
	}
	if calls != 0 {
		t.Fatal("native account process started against local CLI auth")
	}
	info, err := service.Info(context.Background(), &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	for _, profile := range info.Profiles {
		if profile.Managed {
			t.Fatal("local CLI profile presented as a managed subscription")
		}
	}
}

func TestClaudeBrowserEnrollmentRegistersOnlyAfterNativeConfirmation(t *testing.T) {
	s, _ := collectionService(t)
	native := &fakeNativeAccount{authorize: make(chan struct{})}
	s.Bench.AccountFactory = func(ctx context.Context, p accounts.Profile) (runner.NativeAccount, error) {
		if p.Provider != "claude" || !p.NativeLogin || !p.Managed || p.Home != filepath.Join(s.Bench.Config.State, "accounts", p.ID, "claude") {
			t.Error("Claude login lacks a private native credential context")
		}
		return native, nil
	}
	flow, err := s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "browser-claude", Provider: "claude", Label: "Personal"})
	if err != nil {
		t.Fatal(err)
	}
	enrollmentState(t, s, flow.Id, "awaiting_login")
	if len(s.Bench.Profiles()) != 2 {
		t.Fatal("unconfirmed account registered")
	}
	close(native.authorize)
	enrollmentState(t, s, flow.Id, "added")
	saved, err := accounts.LoadProfiles(s.Bench.Config.State)
	if err != nil || len(saved) != 1 || !saved[0].NativeLogin || saved[0].TokenFile != "" {
		t.Fatal("native account was not retained", err)
	}
	if len(s.Bench.List()) != 0 {
		t.Fatal("enrollment started an agent")
	}
}

func TestCodexDisabledDeviceSignInExplainsPrerequisite(t *testing.T) {
	s, _ := collectionService(t)
	native := &fakeNativeAccount{authorize: make(chan struct{}), loginError: codex.ErrDeviceAuthDisabled}
	close(native.authorize)
	s.Bench.AccountFactory = func(context.Context, accounts.Profile) (runner.NativeAccount, error) { return native, nil }
	flow, err := s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "device-disabled", Provider: "codex", Label: "Work"})
	if err != nil {
		t.Fatal(err)
	}
	failed := enrollmentState(t, s, flow.Id, "failed")
	if failed.Error != codex.ErrDeviceAuthDisabled.Error() || failed.UserCode != "" || failed.SignInUrl != "" {
		t.Fatalf("wrong failed enrollment: %+v", failed)
	}
	for _, p := range s.Bench.Profiles() {
		if p.ID == flow.ProfileId {
			t.Fatal("failed login registered account")
		}
	}
}
