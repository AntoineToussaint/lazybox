package transport

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/codex"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

type fakeNativeAccount struct {
	authorize  chan struct{}
	closed     atomic.Bool
	loginError error
	// startError fails Login itself (Codex could not start its sign-in).
	startError error
	method     atomic.Value
	cancelled  atomic.Value
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

func (f *fakeNativeAccount) Login(_ context.Context, method string) (codex.Login, error) {
	f.method.Store(method)
	if f.startError != nil {
		return codex.Login{}, f.startError
	}
	if method == accounts.DeviceSignIn {
		return codex.Login{ID: "native-login", URL: "https://auth.openai.com/codex/device", Code: "FIXTURE-CODE"}, nil
	}
	return codex.Login{ID: "native-login", URL: "https://auth.openai.com/oauth/authorize?state=fixture"}, nil
}
func (f *fakeNativeAccount) CancelLogin(_ context.Context, id string) error {
	f.cancelled.Store(id)
	return nil
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
		return &pb.AddSubscriptionRequest{RequestKey: "stable-request", Provider: "codex", Label: "Work", SignInMethod: "device"}
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
	if _, err = s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "stable-request", Provider: "codex", Label: "changed", SignInMethod: "device"}); err == nil {
		t.Fatal("request identity reused")
	}
	refreshed, err := s.RefreshSubscription(context.Background(), &pb.RefreshSubscriptionRequest{ProfileId: first.ProfileId})
	if err != nil || len(refreshed.Availability.Windows) != 2 {
		t.Fatal(refreshed, err)
	}
	renamed, err := s.RenameSubscription(context.Background(), &pb.RenameSubscriptionRequest{ProfileId: first.ProfileId, Label: "  Team  "})
	if err != nil || renamed.Id != first.ProfileId || renamed.Label != "Team" || len(renamed.Availability.Windows) != 2 {
		t.Fatal("rename lost account identity or usage", renamed, err)
	}
	saved, err := accounts.LoadProfiles(s.Bench.Config.SubscriptionState)
	if err != nil || len(saved) != 1 || saved[0].ID != first.ProfileId || saved[0].Label != "Team" || saved[0].EnrollmentLabel == nil || *saved[0].EnrollmentLabel != "Work" {
		t.Fatal("rename was not persisted with the original enrollment label", saved, err)
	}
	retry, err = s.AddSubscription(context.Background(), request())
	if err != nil || retry.Id != first.Id || retry.Label != "Team" {
		t.Fatal("enrollment retry lost the renamed account", retry, err)
	}
	info, err = s.Info(context.Background(), &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	for _, p := range info.Profiles {
		if p.Id == first.ProfileId && p.Label != "Team" {
			t.Fatal("Info returned the old name")
		}
	}
	if _, err := s.RenameSubscription(context.Background(), &pb.RenameSubscriptionRequest{ProfileId: first.ProfileId, Label: " "}); status.Code(err) != codes.InvalidArgument {
		t.Fatalf("blank name: %v", err)
	}
	if _, err := s.RenameSubscription(context.Background(), &pb.RenameSubscriptionRequest{ProfileId: "codex-default", Label: "Team"}); status.Code(err) != codes.NotFound {
		t.Fatalf("unmanaged account: %v", err)
	}

}

func TestRenameSubscriptionIsOwnerOnly(t *testing.T) {
	method := pb.RunnerService_RenameSubscription_FullMethodName
	_, agentAllowed := agentRPCs[method]
	_, terminalAllowed := terminalRPCs[method]
	_, deviceAllowed := deviceRPCs[method]
	if agentAllowed || terminalAllowed || deviceAllowed {
		t.Fatalf("%s allows a scoped credential", method)
	}
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	_, _, token := f.open(t)
	request := &pb.RenameSubscriptionRequest{ProfileId: "codex-default", Label: "Work"}
	if _, err := f.client.RenameSubscription(bearer(ctx, token), request); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("agent credential got %v, want PermissionDenied", err)
	}
	if _, err := f.client.RenameSubscription(ctx, request); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("no credential got %v, want Unauthenticated", err)
	}
}

// Lazybox does not store Claude.ai credentials (CLAUDE_COMPLIANCE.md): a
// subscription token sent from Settings is refused, wiped from the request,
// and nothing is registered.
func TestSettingsRefusesAClaudeSubscriptionToken(t *testing.T) {
	s, a := collectionService(t)
	request := &pb.AddSubscriptionRequest{RequestKey: "claude-personal", Provider: "claude", Label: "Personal", SubscriptionToken: "fixture-private-subscription"}
	if _, err := s.AddSubscription(context.Background(), request); err == nil || !strings.Contains(err.Error(), "does not store Claude subscription tokens") {
		t.Fatalf("token imported: %v", err)
	}
	if request.SubscriptionToken != "" {
		t.Fatal("secret retained on RPC request object")
	}
	if profiles, err := accounts.LoadProfiles(s.Bench.Config.State); err != nil || len(profiles) != 0 || a.Session != nil {
		t.Fatal(profiles, err)
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
	// Codex's own sign-in is stopped too (account/login/cancel), freeing its callback port.
	if native.cancelled.Load() != "native-login" {
		t.Fatal("the provider's sign-in was not cancelled")
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
	authorized := make(chan struct{})
	close(authorized)
	bench.AccountFactory = func(context.Context, accounts.Profile) (runner.NativeAccount, error) {
		return &fakeNativeAccount{authorize: authorized}, nil
	}
	service := &Service{Bench: bench}
	flow, err := service.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "lost-ack", Provider: "codex", Label: "Work"})
	if err != nil {
		t.Fatal(err)
	}
	enrollmentState(t, service, flow.Id, "added")
	if _, err := service.RenameSubscription(context.Background(), &pb.RenameSubscriptionRequest{ProfileId: flow.ProfileId, Label: "Renamed Work"}); err != nil {
		t.Fatal(err)
	}

	if err = bench.Close(); err != nil {
		t.Fatal(err)
	}
	restored, err := runner.New(runner.Config{State: state})
	if err != nil {
		t.Fatal(err)
	}
	defer restored.Close()
	service.Bench = restored
	receipt, err := service.GetSubscriptionEnrollment(context.Background(), &pb.SubscriptionEnrollmentRequest{RequestKey: "lost-ack", Provider: "codex"})
	if err != nil || receipt.Id != flow.Id || receipt.State != "added" || receipt.SignInUrl != "" || receipt.UserCode != "" {
		t.Fatal(receipt, err)
	}
	retry, err := service.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "lost-ack", Provider: "codex", Label: "Work"})
	if err != nil || retry.Id != flow.Id || retry.Label != "Renamed Work" {
		t.Fatal("renaming must preserve the original enrollment request across restart", retry, err)
	}

	missing, err := service.GetSubscriptionEnrollment(context.Background(), &pb.SubscriptionEnrollmentRequest{RequestKey: "never-accepted", Provider: "codex"})
	if err != nil || missing.State != "expired" || missing.Error != runner.ExpiredSignIn || strings.Contains(missing.Error, "tracked") {
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
	signedIn := make(chan struct{})
	close(signedIn)
	work.AccountFactory = func(context.Context, accounts.Profile) (runner.NativeAccount, error) {
		return &fakeNativeAccount{authorize: signedIn}, nil
	}
	flow, err := ws.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "shared-first", Provider: "codex", Label: "Saved account"})
	if err != nil {
		t.Fatal(err)
	}
	enrollmentState(t, ws, flow.Id, "added")
	info, err := ds.Info(context.Background(), &pb.InfoRequest{})
	if err != nil || !info.SharedSubscriptions || len(info.Profiles) != 3 || !info.Profiles[2].Managed {
		t.Fatal("other Runner did not discover enrollment", info, err)
	}
	if _, err := ws.RenameSubscription(context.Background(), &pb.RenameSubscriptionRequest{ProfileId: flow.ProfileId, Label: "Shared Work"}); err != nil {
		t.Fatal(err)
	}
	info, err = ds.Info(context.Background(), &pb.InfoRequest{})
	if err != nil || len(info.Profiles) != 3 || info.Profiles[2].Id != flow.ProfileId || info.Profiles[2].Label != "Shared Work" {
		t.Fatal("other Runner kept the old label", info, err)
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
	allowClaudeSignIn(t, s)
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
	flow, err := s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "device-disabled", Provider: "codex", Label: "Work", SignInMethod: "device"})
	if err != nil {
		t.Fatal(err)
	}
	failed := enrollmentState(t, s, flow.Id, "failed")
	if failed.Error != codex.ErrDeviceAuthDisabled.Error() || failed.ErrorCode != runner.ErrorDeviceSignInDisabled || failed.SignInMethod != "device" || failed.UserCode != "" || failed.SignInUrl != "" {
		t.Fatalf("wrong failed enrollment: %+v", failed)
	}
	for _, p := range s.Bench.Profiles() {
		if p.ID == flow.ProfileId {
			t.Fatal("failed login registered account")
		}
	}
	// The way out the client offers: the same account, in the browser.
	browser := &fakeNativeAccount{authorize: make(chan struct{})}
	s.Bench.AccountFactory = func(context.Context, accounts.Profile) (runner.NativeAccount, error) { return browser, nil }
	retry, err := s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "device-disabled-browser", Provider: "codex", Label: "Work", SignInMethod: "browser"})
	if err != nil {
		t.Fatal(err)
	}
	waiting := enrollmentState(t, s, retry.Id, "awaiting_login")
	if waiting.SignInMethod != "browser" || waiting.UserCode != "" || !strings.HasPrefix(waiting.SignInUrl, "https://auth.openai.com/oauth/") || browser.method.Load() != "browser" {
		t.Fatalf("browser retry: %+v", waiting)
	}
	close(browser.authorize)
	enrollmentState(t, s, retry.Id, "added")
}

// Codex signs in through the browser unless a code is asked for: no device
// code is involved, and the page calls back to Codex on the Runner's computer.
func TestCodexSignsInWithTheBrowserByDefault(t *testing.T) {
	s, _ := collectionService(t)
	native := &fakeNativeAccount{authorize: make(chan struct{})}
	s.Bench.AccountFactory = func(_ context.Context, p accounts.Profile) (runner.NativeAccount, error) {
		if p.Isolation != "environment" || p.Home != filepath.Join(s.Bench.Config.State, "accounts", p.ID, "codex") {
			t.Error("browser sign-in escaped the profile's own CODEX_HOME", p.Home)
		}
		return native, nil
	}
	flow, err := s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "browser-codex", Provider: "codex", Label: "Work"})
	if err != nil {
		t.Fatal(err)
	}
	if flow.SignInMethod != "browser" {
		t.Fatalf("default method: %+v", flow)
	}
	waiting := enrollmentState(t, s, flow.Id, "awaiting_login")
	if waiting.SignInUrl == "" || waiting.UserCode != "" || native.method.Load() != "browser" {
		t.Fatalf("browser sign-in: %+v", waiting)
	}
	// The same request asked again with the method spelled out is the same request.
	if again, err := s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "browser-codex", Provider: "codex", Label: "Work", SignInMethod: "browser"}); err != nil || again.Id != flow.Id {
		t.Fatal("explicit default is a different request", err)
	}
	// ... but asking for a code under the same request key is not.
	if _, err := s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "browser-codex", Provider: "codex", Label: "Work", SignInMethod: "device"}); err == nil {
		t.Fatal("request key reused for another sign-in method")
	}
	close(native.authorize)
	enrollmentState(t, s, flow.Id, "added")
	for _, bad := range []*pb.AddSubscriptionRequest{
		{RequestKey: "claude-code", Provider: "claude", Label: "Personal", SignInMethod: "device"},
		{RequestKey: "codex-other", Provider: "codex", Label: "Other", SignInMethod: "carrier-pigeon"},
	} {
		if _, err := s.AddSubscription(context.Background(), bad); err == nil {
			t.Fatalf("accepted %+v", bad)
		}
	}
}

func TestCodexBrowserSignInSaysWhenAnotherSignInHoldsTheCallback(t *testing.T) {
	s, _ := collectionService(t)
	native := &fakeNativeAccount{startError: codex.ErrSignInInProgress}
	s.Bench.AccountFactory = func(context.Context, accounts.Profile) (runner.NativeAccount, error) { return native, nil }
	flow, err := s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "port-busy", Provider: "codex", Label: "Work"})
	if err != nil {
		t.Fatal(err)
	}
	failed := enrollmentState(t, s, flow.Id, "failed")
	if failed.ErrorCode != runner.ErrorSignInInProgress || failed.Error != codex.ErrSignInInProgress.Error() || failed.SignInUrl != "" {
		t.Fatalf("port busy: %+v", failed)
	}
	if !strings.Contains(failed.Error, "Another Codex sign-in is in progress") {
		t.Fatal(failed.Error)
	}
	// Any other failure to start stays private and offers nothing specific.
	other := &fakeNativeAccount{startError: errors.New("adapter account/login/start: secret detail")}
	s.Bench.AccountFactory = func(context.Context, accounts.Profile) (runner.NativeAccount, error) { return other, nil }
	flow, err = s.AddSubscription(context.Background(), &pb.AddSubscriptionRequest{RequestKey: "other-failure", Provider: "codex", Label: "Work"})
	if err != nil {
		t.Fatal(err)
	}
	failed = enrollmentState(t, s, flow.Id, "failed")
	if failed.ErrorCode != "" || strings.Contains(failed.Error, "secret") {
		t.Fatalf("private failure: %+v", failed)
	}
}

// allowClaudeSignIn turns Claude sign-in in Lazybox on, as the person does in
// Settings → Agents & workspaces → Claude (off by default; CLAUDE_COMPLIANCE.md).
func allowClaudeSignIn(t *testing.T, s *Service) {
	t.Helper()
	settings, err := s.GetRunnerSettings(context.Background(), &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	settings.ClaudeSubscriptionSignIn = true
	if _, err = s.SaveRunnerSettings(context.Background(), settings); err != nil {
		t.Fatal(err)
	}
}
