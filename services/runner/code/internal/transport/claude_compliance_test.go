package transport

import (
	"context"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// CLAUDE_COMPLIANCE.md: an Anthropic API key for Claude agents, Claude
// sign-in in Lazybox off by default, and automatic switching only through an
// extension's account policy.

// A fixture key: shaped like an Anthropic API key, valid nowhere.
const fixtureAPIKey = "sk-ant-api03-fixture-not-a-real-key-0000-wxyz"

func agentFailure(state string) agent.Event {
	return agent.Event{Type: "failure", Status: state, Account: &agent.AccountObservation{State: state, ResetAt: time.Now().Add(time.Hour).Unix()}}
}

// enableAccountPolicy installs and approves Lazybox's own Subscription
// rotation extension, as the person does in Settings → Extensions.
func enableAccountPolicy(t *testing.T, b *runner.Bench) {
	t.Helper()
	snap, err := b.Extensions().InstallExample("subscription-rotation")
	if err != nil {
		t.Fatal(err)
	}
	info, ok := snap.Find("subscription-rotation")
	if !ok {
		t.Fatal("the bundled Subscription rotation extension is missing")
	}
	if _, err = b.Extensions().Approve(info.ID, info.Manifest.Grants, info.Manifest.Hosts); err != nil {
		t.Fatal(err)
	}
}

// The saved key is what a claude-default agent runs on, the only Claude
// credential in its environment; the key never comes back on the wire, and a
// subscription token is refused.
func TestClaudeAPIKeyIsWhatTheAgentRunsOn(t *testing.T) {
	service, _ := collectionService(t)
	b := service.Bench
	adapter := rotatingFixture{opened: make(chan *rotatingNative, 4)}
	b.Adapters["claude"] = adapter
	t.Setenv("CLAUDE_CODE_OAUTH_TOKEN", "fixture-inherited-subscription")
	t.Setenv("ANTHROPIC_API_KEY", "")
	ctx := context.Background()
	env := func() string {
		t.Helper()
		r, err := service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", Cwd: t.TempDir()})
		if err != nil {
			t.Fatal(err)
		}
		if r.Session.Provider != "claude" {
			t.Fatalf("provider %q", r.Session.Provider)
		}
		return "\n" + strings.Join((<-adapter.opened).Open.Env, "\n") + "\n"
	}
	// No key: the person's own Claude Code sign-in, as inherited.
	if got := env(); strings.Contains(got, "\nANTHROPIC_API_KEY=sk-") || !strings.Contains(got, "\nCLAUDE_CODE_OAUTH_TOKEN=fixture-inherited-subscription\n") {
		t.Fatal("without a key the agent must keep the person's own sign-in")
	}
	if _, err := service.SetClaudeApiKey(ctx, &pb.SetClaudeApiKeyRequest{ApiKey: "sk-ant-oat01-fixture-subscription"}); status.Code(err) != codes.InvalidArgument {
		t.Fatalf("a subscription token was saved as a key: %v", err)
	}
	auth, err := service.SetClaudeApiKey(ctx, &pb.SetClaudeApiKeyRequest{ApiKey: "  " + fixtureAPIKey + "\n"})
	if err != nil {
		t.Fatal(err)
	}
	if !auth.ApiKeySet || auth.ApiKeyHint != "wxyz" || strings.Contains(auth.String(), fixtureAPIKey) {
		t.Fatalf("the reply: %v", auth)
	}
	got := env()
	if !strings.Contains(got, "\nANTHROPIC_API_KEY="+fixtureAPIKey+"\n") || strings.Contains(got, "CLAUDE_CODE_OAUTH_TOKEN") {
		t.Fatalf("the agent's Claude credentials: the key, and only the key")
	}
	if read, _ := service.GetClaudeAuth(ctx, &pb.InfoRequest{}); !read.ApiKeySet || strings.Contains(read.String(), fixtureAPIKey) {
		t.Fatalf("GetClaudeAuth: %v", read)
	}
	if info, err := os.Stat(accounts.APIKeyFile(b.Config.State)); err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("key file: %v %v", info, err)
	}
	// Codex is not given it.
	b.Adapters["codex"] = adapter
	r, err := service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "codex-default", Cwd: t.TempDir()})
	if err != nil || r.Session.Provider != "codex" {
		t.Fatal(err)
	}
	if codexEnv := strings.Join((<-adapter.opened).Open.Env, "\n"); strings.Contains(codexEnv, fixtureAPIKey) {
		t.Fatal("the Anthropic key reached Codex")
	}
	if auth, err = service.SetClaudeApiKey(ctx, &pb.SetClaudeApiKeyRequest{Clear: true}); err != nil || auth.ApiKeySet {
		t.Fatalf("forget: %v %v", auth, err)
	}
	if got := env(); strings.Contains(got, fixtureAPIKey) {
		t.Fatal("a forgotten key is still used")
	}
}

// A Claude subscription saved through a sign-in Lazybox offered starts only
// with Claude sign-in in Lazybox turned on; adding one is refused while off.
func TestClaudeSignInInLazyboxIsOffByDefault(t *testing.T) {
	service, _ := collectionService(t)
	b := service.Bench
	adapter := rotatingFixture{opened: make(chan *rotatingNative, 4)}
	b.Adapters["claude"] = adapter
	home := filepath.Join(b.Config.State, "accounts", "claude-work", "claude")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := accounts.StoreProfile(b.Config.State, accounts.Profile{ID: "claude-work", Label: "Work", Provider: "claude", Isolation: "environment", Home: home, NativeLogin: true}); err != nil {
		t.Fatal(err)
	}
	if err := b.ReloadProfiles(); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	if _, err := service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-work", Cwd: t.TempDir()}); status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "Claude sign-in in Lazybox is off") || len(adapter.opened) != 0 {
		t.Fatalf("a Claude subscription saved in Lazybox started with sign-in off: %v", err)
	}
	if _, err := service.AddSubscription(ctx, &pb.AddSubscriptionRequest{RequestKey: "k", Provider: "claude", Label: "Home"}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("Lazybox offered a Claude sign-in with it off: %v", err)
	}
	if _, err := service.AddSubscription(ctx, &pb.AddSubscriptionRequest{RequestKey: "t", Provider: "claude", Label: "Home", SubscriptionToken: "sk-ant-oat01-fixture"}); err == nil || !strings.Contains(err.Error(), "does not store Claude subscription tokens") {
		t.Fatalf("a subscription token was imported: %v", err)
	}
	settings, err := service.GetRunnerSettings(ctx, &pb.InfoRequest{})
	if err != nil || settings.ClaudeSubscriptionSignIn {
		t.Fatalf("on by default: %v", err)
	}
	settings.ClaudeSubscriptionSignIn = true
	if _, err = service.SaveRunnerSettings(ctx, settings); err != nil {
		t.Fatal(err)
	}
	if _, err = service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-work", Cwd: t.TempDir()}); err != nil {
		t.Fatalf("turned on, it starts: %v", err)
	}
	if open := (<-adapter.opened).Open; !strings.Contains(strings.Join(open.Env, "\n"), "CLAUDE_CONFIG_DIR="+home) {
		t.Fatal("the saved subscription's own sign-in")
	}
}

// Without an account policy a conversation never switches by itself; the
// person's explicit switch still works. Approving the bundled extension
// switches at the next limit; turning it off stops that, running
// conversations included.
func TestAutomaticSwitchingIsAnAccountPolicy(t *testing.T) {
	service, _ := collectionService(t)
	b := service.Bench
	adapter := twoSubscriptions(t, b)
	ctx := context.Background()
	limit := func(n *rotatingNative) {
		n.Events <- agentFailure("exhausted")
	}
	settings, _ := service.GetRunnerSettings(ctx, &pb.InfoRequest{})
	if len(settings.AutomaticSwitchingProviders) != 0 {
		t.Fatalf("switching on by default: %v", settings.AutomaticSwitchingProviders)
	}
	r, err := service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "a", Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	first := <-adapter.opened
	if len(r.Session.RotationProfileIds) != 0 {
		t.Fatalf("a pool without an account policy: %v", r.Session.RotationProfileIds)
	}
	limit(first)
	e, _ := b.Get(r.Session.Id)
	until(t, "the limit stops it", func() bool { return e.Engine.Snapshot().Fault == "exhausted" })
	if len(adapter.opened) != 0 || first.stopped.Load() {
		t.Fatal("switched by itself without an account policy")
	}
	// The person's choice needs no policy.
	if _, err = service.SwitchSubscription(ctx, &pb.SwitchSubscriptionRequest{SessionId: r.Session.Id, ProfileId: "b"}); err != nil {
		t.Fatal(err)
	}
	<-adapter.opened

	enableAccountPolicy(t, b)
	settings, _ = service.GetRunnerSettings(ctx, &pb.InfoRequest{})
	if !slices.Equal(settings.AutomaticSwitchingProviders, []string{"claude", "codex"}) {
		t.Fatalf("providers: %v", settings.AutomaticSwitchingProviders)
	}
	r, err = service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "a", Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	first = <-adapter.opened
	if strings.Join(r.Session.RotationProfileIds, ",") != "b" {
		t.Fatalf("pool %v", r.Session.RotationProfileIds)
	}
	limit(first)
	select {
	case second := <-adapter.opened:
		if second.Open.Profile != "b" {
			t.Fatalf("moved to %s", second.Open.Profile)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("the account policy did not switch at the limit")
	}

	// Off again: a running conversation with a pool stops at its next limit.
	r, err = service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "a", Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	first = <-adapter.opened
	if _, err = b.Extensions().SetEnabled("subscription-rotation", false); err != nil {
		t.Fatal(err)
	}
	limit(first)
	e, _ = b.Get(r.Session.Id)
	until(t, "the limit stops it", func() bool { return e.Engine.Snapshot().Fault == "exhausted" })
	if len(adapter.opened) != 0 {
		t.Fatal("an extension turned off still switched")
	}
}

// The migration: compliant defaults for new state, and an existing setup
// keeps working. Each case opens a Runner on state an earlier version left.

// openRunner opens a Runner on state, as a restart of Lazybox Dev does.
func openRunner(t *testing.T, state string) *Service {
	t.Helper()
	b, err := runner.New(runner.Config{State: state, OutputFallbackRoot: t.TempDir(), WorktreeRoot: t.TempDir(), ItemPoll: -1})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	return &Service{Bench: b}
}

// Claude subscriptions already saved in Lazybox, one signed in and one
// imported from a token, keep starting: the gate is carried over on, said
// once, and only a new token import is refused. The imported one can be
// removed.
func TestMigrationKeepsSavedClaudeAccountsWorking(t *testing.T) {
	state := t.TempDir()
	root := filepath.Join(state, "accounts")
	for _, id := range []string{"claude-work", "claude-imported"} {
		if err := os.MkdirAll(filepath.Join(root, id, "claude"), 0o700); err != nil {
			t.Fatal(err)
		}
	}
	token := filepath.Join(root, "claude-imported", "oauth-token")
	if err := os.WriteFile(token, []byte("fixture-imported-token"), 0o600); err != nil {
		t.Fatal(err)
	}
	for _, p := range []accounts.Profile{
		{ID: "claude-work", Label: "Work", Provider: "claude", Isolation: "environment", Home: filepath.Join(root, "claude-work", "claude"), NativeLogin: true},
		{ID: "claude-imported", Label: "Imported", Provider: "claude", Isolation: "environment", Home: filepath.Join(root, "claude-imported", "claude"), TokenFile: token},
	} {
		if err := accounts.StoreProfile(state, p); err != nil {
			t.Fatal(err)
		}
	}
	service := openRunner(t, state)
	adapter := rotatingFixture{opened: make(chan *rotatingNative, 4)}
	service.Bench.Adapters["claude"] = adapter
	ctx := context.Background()
	settings, err := service.GetRunnerSettings(ctx, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if !settings.ClaudeSubscriptionSignIn || !settings.ClaudeSignInCarriedOver || !slices.Contains(settings.Notices, runner.NoticeClaudeSignInKept) {
		t.Fatalf("not carried over: sign-in %v, carried %v, notices %v", settings.ClaudeSubscriptionSignIn, settings.ClaudeSignInCarriedOver, settings.Notices)
	}
	for _, id := range []string{"claude-work", "claude-imported"} {
		if _, err = service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: id, Cwd: t.TempDir()}); err != nil {
			t.Fatalf("%s no longer starts: %v", id, err)
		}
		env := strings.Join((<-adapter.opened).Open.Env, "\n")
		if id == "claude-imported" && !strings.Contains(env, "CLAUDE_CODE_OAUTH_TOKEN=fixture-imported-token") {
			t.Fatal("the imported account lost its credential")
		}
	}
	info, err := service.Info(ctx, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	for _, p := range info.Profiles {
		if p.TokenImported != (p.Id == "claude-imported") {
			t.Fatalf("%s token_imported %v", p.Id, p.TokenImported)
		}
	}
	if strings.Contains(info.String(), "fixture-imported-token") {
		t.Fatal("the token reached the wire")
	}
	if _, err = service.AddSubscription(ctx, &pb.AddSubscriptionRequest{RequestKey: "new", Provider: "claude", Label: "New", SubscriptionToken: "sk-ant-oat01-fixture"}); err == nil || !strings.Contains(err.Error(), "does not store Claude subscription tokens") {
		t.Fatalf("a new token import was accepted: %v", err)
	}
	// A conversation runs on it: removing it waits.
	if _, err = service.RemoveSubscription(ctx, &pb.RemoveSubscriptionRequest{ProfileId: "claude-imported"}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("removed under a running conversation: %v", err)
	}
	for _, e := range service.Bench.List() {
		if e.Profile.ID == "claude-imported" {
			if _, err = service.CloseSession(ctx, &pb.SessionRequest{SessionId: e.ID}); err != nil {
				t.Fatal(err)
			}
		}
	}
	info, err = service.RemoveSubscription(ctx, &pb.RemoveSubscriptionRequest{ProfileId: "claude-imported"})
	if err != nil || slices.ContainsFunc(info.Profiles, func(p *pb.Profile) bool { return p.Id == "claude-imported" }) {
		t.Fatalf("not removed: %v", err)
	}
	if _, err = os.Stat(token); !os.IsNotExist(err) {
		t.Fatal("the imported token outlived its account")
	}
	if _, err = service.RemoveSubscription(ctx, &pb.RemoveSubscriptionRequest{ProfileId: "claude-default"}); err == nil {
		t.Fatal("the person's own CLI sign-in was removed")
	}
	// Once: turned off and restarted, it stays off.
	settings, _ = service.GetRunnerSettings(ctx, &pb.InfoRequest{})
	settings.ClaudeSubscriptionSignIn, settings.Notices = false, nil
	if settings, err = service.SaveRunnerSettings(ctx, settings); err != nil || settings.ClaudeSignInCarriedOver {
		t.Fatalf("turned off: %v %v", settings, err)
	}
	if err = service.Bench.Close(); err != nil {
		t.Fatal(err)
	}
	again := openRunner(t, state)
	if settings, _ = again.GetRunnerSettings(ctx, &pb.InfoRequest{}); settings.ClaudeSubscriptionSignIn || len(settings.Notices) != 0 {
		t.Fatalf("the migration ran twice: %v", settings)
	}
}

// Fresh state: the compliant defaults, and nothing to say.
func TestMigrationFreshStateKeepsCompliantDefaults(t *testing.T) {
	service := openRunner(t, t.TempDir())
	settings, err := service.GetRunnerSettings(context.Background(), &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if settings.ClaudeSubscriptionSignIn || settings.ClaudeSignInCarriedOver || len(settings.Notices) != 0 || len(settings.AutomaticSwitchingProviders) != 0 || settings.Revision != 1 {
		t.Fatalf("fresh state: %v", settings)
	}
	if _, ok := service.Bench.Extensions().Snapshot().Find(runner.RotationExtension); ok {
		t.Fatal("Subscription rotation installed on a fresh Runner")
	}
}

// Where automatic switching was on, it stays on, now through the bundled
// Subscription rotation extension, installed and allowed, with the same
// settings (an account left out stays out), for Codex as before.
func TestMigrationKeepsAutomaticSwitchingThroughTheExtension(t *testing.T) {
	state := t.TempDir()
	root := filepath.Join(state, "accounts")
	for _, id := range []string{"work", "spare"} {
		if err := accounts.StoreProfile(state, accounts.Profile{ID: id, Provider: "codex", Isolation: "environment", Home: filepath.Join(root, id, "codex")}); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(state, "runner-settings.json"), []byte(`{"revision":3,"naming":"repositories","rotationExcluded":["spare"]}`), 0o600); err != nil {
		t.Fatal(err)
	}
	service := openRunner(t, state)
	b := service.Bench
	adapter := rotatingFixture{opened: make(chan *rotatingNative, 4)}
	b.Adapters["codex"] = adapter
	ctx := context.Background()
	info, ok := b.Extensions().Snapshot().Find(runner.RotationExtension)
	if !ok || info.State != power.ExtensionEnabled {
		t.Fatalf("Subscription rotation not installed and on: %+v", info)
	}
	settings, _ := service.GetRunnerSettings(ctx, &pb.InfoRequest{})
	if !slices.Equal(settings.AutomaticSwitchingProviders, []string{"claude", "codex"}) || !slices.Contains(settings.Notices, runner.NoticeSwitchingMoved) || strings.Join(settings.RotationExcludedProfileIds, ",") != "spare" || settings.ClaudeSubscriptionSignIn {
		t.Fatalf("after the migration: %v", settings)
	}
	r, err := service.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "codex-default", Cwd: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	if strings.Join(r.Session.RotationProfileIds, ",") != "work" {
		t.Fatalf("pool %v: the same settings as before (spare left out)", r.Session.RotationProfileIds)
	}
	first := <-adapter.opened
	first.Events <- agentFailure("exhausted")
	select {
	case second := <-adapter.opened:
		if second.Open.Profile != "work" {
			t.Fatalf("moved to %s", second.Open.Profile)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("automatic switching stopped with the migration")
	}
	// The person turns it off: a restart does not turn it back on.
	if _, err = b.Extensions().SetEnabled(runner.RotationExtension, false); err != nil {
		t.Fatal(err)
	}
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	again := openRunner(t, state)
	if info, _ = again.Bench.Extensions().Snapshot().Find(runner.RotationExtension); info.State != power.ExtensionDisabled {
		t.Fatalf("the migration ran twice: %s", info.State)
	}
}
