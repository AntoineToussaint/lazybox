package codex

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"
)

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
	login, err := native.Login(ctx)
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
	failedLogin, err := failed.Login(ctx)
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
