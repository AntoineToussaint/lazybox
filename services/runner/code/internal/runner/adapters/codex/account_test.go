package codex

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
)

// browserFixture is a fake `codex app-server` speaking the browser sign-in
// RPCs as codex-cli 0.169.0 does: account/login/start {"type":"chatgpt"}
// answers an authUrl (no code), a notification may come before the answer,
// completion is account/login/completed, and account/login/cancel ends the
// login with success false. On success it writes auth.json into CODEX_HOME,
// as Codex's file credential store does.
const browserFixture = `#!/usr/bin/env python3
import sys,json,os
assert sys.argv[1:] == ['app-server','--stdio','-c','cli_auth_credentials_store="file"']
def send(m):print(json.dumps(m),flush=True)
for line in sys.stdin:
 m=json.loads(line)
 if 'id' not in m:continue
 method=m['method']
 if method=='initialize':send({'id':m['id'],'result':{}});continue
 if method=='account/login/start':
  assert m['params']=={'type':'chatgpt'}, m['params']
  send({'method':'remoteControl/status/changed','params':{'status':'disabled'}})
  if os.environ.get('FIXTURE_PORT_BUSY'):
   send({'id':m['id'],'error':{'code':-32603,'message':'failed to start login server: Port 127.0.0.1:1457 is already in use'}});continue
  send({'id':m['id'],'result':{'type':'chatgpt','loginId':'browser-1','authUrl':'https://auth.openai.com/oauth/authorize?response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback'}})
  if not os.environ.get('FIXTURE_HOLD'):
   open(os.path.join(os.environ['CODEX_HOME'],'auth.json'),'w').write('{"tokens":{}}')
   send({'method':'account/login/completed','params':{'loginId':'browser-1','success':True,'error':None}})
  continue
 if method=='account/login/cancel':
  assert m['params']=={'loginId':'browser-1'}, m['params']
  send({'id':m['id'],'result':{'status':'canceled'}})
  send({'method':'account/login/completed','params':{'loginId':'browser-1','success':False,'error':'Login server error: Login was not completed'}})
  continue
 raise RuntimeError('Unexpected inference or account mutation: '+method)
`

func openBrowserFixture(t *testing.T, ctx context.Context, home string, env ...string) *AccountClient {
	t.Helper()
	if _, err := exec.LookPath("python3"); err != nil {
		t.Skip("Python JSON-RPC fixture unavailable")
	}
	binary := filepath.Join(t.TempDir(), "browser-fixture")
	if err := os.WriteFile(binary, []byte(browserFixture), 0700); err != nil {
		t.Fatal(err)
	}
	native, err := OpenAccount(ctx, binary, append([]string{"PATH=" + os.Getenv("PATH"), "CODEX_HOME=" + home}, env...), t.TempDir(), true)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { native.Close() })
	return native
}

func TestBrowserSignInOpensAPageWithoutACodeAndSavesInTheProfile(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	home := t.TempDir()
	native := openBrowserFixture(t, ctx, home)
	login, err := native.Login(ctx, accounts.BrowserSignIn)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(login.URL, "https://auth.openai.com/oauth/authorize?") || login.Code != "" || login.ID != "browser-1" {
		t.Fatalf("browser sign-in: %+v", login)
	}
	if err = native.WaitLogin(ctx, login.ID); err != nil {
		t.Fatal(err)
	}
	if _, err = os.Stat(filepath.Join(home, "auth.json")); err != nil {
		t.Fatal("credentials not saved in the profile's own CODEX_HOME:", err)
	}
	// The browser is also the default when no method is named.
	again := openBrowserFixture(t, ctx, t.TempDir())
	if login, err = again.Login(ctx, ""); err != nil || login.Code != "" || login.URL == "" {
		t.Fatalf("default sign-in: %+v %v", login, err)
	}
}

func TestBrowserSignInCancelStopsCodexLogin(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	home := t.TempDir()
	native := openBrowserFixture(t, ctx, home, "FIXTURE_HOLD=1")
	login, err := native.Login(ctx, accounts.BrowserSignIn)
	if err != nil {
		t.Fatal(err)
	}
	if err = native.CancelLogin(ctx, login.ID); err != nil {
		t.Fatal(err)
	}
	if err = native.WaitLogin(ctx, login.ID); err == nil || errors.Is(err, ErrDeviceAuthDisabled) || strings.Contains(err.Error(), "Login server") {
		t.Fatalf("a cancelled sign-in must fail privately: %v", err)
	}
	if _, err = os.Stat(filepath.Join(home, "auth.json")); err == nil {
		t.Fatal("a cancelled sign-in saved credentials")
	}
}

func TestBrowserSignInSaysWhenAnotherSignInHoldsTheCallbackPort(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	native := openBrowserFixture(t, ctx, t.TempDir(), "FIXTURE_PORT_BUSY=1")
	if _, err := native.Login(ctx, accounts.BrowserSignIn); !errors.Is(err, ErrSignInInProgress) {
		t.Fatalf("port busy: %v", err)
	}
	if !strings.HasPrefix(ErrSignInInProgress.Error(), "Another Codex sign-in is in progress on this ") {
		t.Fatal(ErrSignInInProgress)
	}
}

func TestNativeAccountRPCsDoNotStartThreadsOrUseGlobalCredentialStore(t *testing.T) {
	if _, err := exec.LookPath("python3"); err != nil {
		t.Skip("Python JSON-RPC fixture unavailable")
	}
	binary := filepath.Join(t.TempDir(), "account-fixture")
	source := `#!/usr/bin/env python3
import sys,json,os
assert sys.argv[1:] == ['app-server','--stdio','-c','cli_auth_credentials_store="file"']
for line in sys.stdin:
 m=json.loads(line)
 if 'id' not in m:continue
 method=m['method'];result={}
 if method=='account/login/start':
  assert m['params']['type']=='chatgptDeviceCode'
  result={'type':'chatgptDeviceCode','loginId':'login-1','verificationUrl':'https://auth.openai.com/codex/device','userCode':'FIXTURE-CODE'}
 elif method=='account/read':
  result={'account':None if os.environ.get('FIXTURE_LOGGED_OUT') else {'type':'chatgpt','email':'codex-fixture@example.invalid'},'requiresOpenaiAuth':True}
 elif method=='account/rateLimits/read':
  result={'rateLimits':{'planType':'plus','primary':{'usedPercent':45,'windowDurationMins':300,'resetsAt':1234567}}}
 elif method!='initialize':
  raise RuntimeError('Unexpected inference or account mutation: '+method)
 print(json.dumps({'id':m['id'],'result':result}),flush=True)
 if method=='account/login/start':print(json.dumps({'method':'account/login/completed','params':{'loginId':'login-1','success':not bool(os.environ.get('FIXTURE_LOGIN_ERROR')), 'error':os.environ.get('FIXTURE_LOGIN_ERROR')}}),flush=True)
`
	if err := os.WriteFile(binary, []byte(source), 0700); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	native, err := OpenAccount(ctx, binary, []string{"PATH=" + os.Getenv("PATH"), "CODEX_HOME=" + t.TempDir()}, t.TempDir(), true)
	if err != nil {
		t.Fatal(err)
	}
	defer native.Close()
	login, err := native.Login(ctx, accounts.DeviceSignIn)
	if err != nil || login.Code != "FIXTURE-CODE" {
		t.Fatal(login, err)
	}
	if err = native.WaitLogin(ctx, login.ID); err != nil {
		t.Fatal(err)
	}

	failed, err := OpenAccount(ctx, binary, []string{"PATH=" + os.Getenv("PATH"), "FIXTURE_LOGIN_ERROR=Enable device code sign-in in ChatGPT Security Settings"}, t.TempDir(), true)
	if err != nil {
		t.Fatal(err)
	}
	defer failed.Close()
	failedLogin, err := failed.Login(ctx, accounts.DeviceSignIn)
	if err != nil {
		t.Fatal(err)
	}
	if err := failed.WaitLogin(ctx, failedLogin.ID); !errors.Is(err, ErrDeviceAuthDisabled) {
		t.Fatalf("wrong sign-in failure: %v", err)
	}
	quota, err := native.Usage(ctx)
	if err != nil || len(quota.Windows) != 1 || quota.Windows[0].UsedPercent != 45 || quota.Plan != "plus" || quota.Email != "codex-fixture@example.invalid" {
		t.Fatalf("%+v %v", quota, err)
	}
	loggedOut, err := OpenAccount(ctx, binary, []string{"PATH=" + os.Getenv("PATH"), "FIXTURE_LOGGED_OUT=1"}, t.TempDir(), true)
	if err != nil {
		t.Fatal(err)
	}
	defer loggedOut.Close()
	state, err := loggedOut.Usage(ctx)
	if err != nil || state.State != "auth_required" {
		t.Fatal(state, err)
	}
}

func TestDeviceSignInSettingErrorIsActionableAndUnknownErrorsArePrivate(t *testing.T) {
	message := `Enable device code sign-in for Codex, Excel, PowerPoint, and Word in ChatGPT Security Settings, then run "codex login --device-auth" again.`
	if !errors.Is(loginError(message), ErrDeviceAuthDisabled) {
		t.Fatal("missing account prerequisite")
	}
	if loginError("secret-url?code=private").Error() != "Codex sign-in did not complete" {
		t.Fatal("provider details leaked")
	}
}
