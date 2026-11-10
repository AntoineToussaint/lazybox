package claude

import (
	"context"
	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestNativeBrowserLoginUsesPrivateStoreAndConfirmsSavedIdentity(t *testing.T) {
	if _, err := exec.LookPath("python3"); err != nil {
		t.Skip("Python native CLI fixture unavailable")
	}
	for _, mode := range []string{"success", "wrong-store", "cancel"} {
		t.Run(mode, func(t *testing.T) {
			home := filepath.Join(t.TempDir(), "private account")
			if err := os.Mkdir(home, 0700); err != nil {
				t.Fatal(err)
			}
			binary := filepath.Join(t.TempDir(), "claude-fixture")
			source := `#!/usr/bin/env python3
import os,sys,json,subprocess,shlex,time
home=os.environ['CLAUDE_CONFIG_DIR']
assert os.path.realpath(os.getcwd())==os.path.realpath(home)
assert not os.environ.get('DISABLE_LOGIN_COMMAND')
if sys.argv[1:]==['auth','login','--claudeai']:
 subprocess.run([os.environ['BROWSER']]+['https://claude.com/cai/oauth/authorize?state=fixture&redirect_uri=http%3A%2F%2Flocalhost%3A54321%2Fcallback'],check=True)
 if os.environ['FIXTURE_MODE']=='cancel':time.sleep(60)
 time.sleep(.2)
 open(home+'/fixture-auth','w').write('fixture-private-login')
elif sys.argv[1:]==['auth','status']:
 assert os.path.isfile(home+'/fixture-auth')
 print(json.dumps({'loggedIn':True,'authMethod':'claude.ai','configDirectory':'/wrong' if os.environ['FIXTURE_MODE']=='wrong-store' else home}))
else:raise Exception('unexpected CLI command')
`
			if err := os.WriteFile(binary, []byte(source), 0700); err != nil {
				t.Fatal(err)
			}
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			client, err := OpenAccount(ctx, binary, []string{"PATH=" + os.Getenv("PATH"), "CLAUDE_CONFIG_DIR=" + home, "DISABLE_LOGIN_COMMAND=1", "FIXTURE_MODE=" + mode}, home)
			if err != nil {
				t.Fatal(err)
			}
			defer client.Close()
			login, err := client.Login(ctx)
			if err != nil || !strings.HasPrefix(login.URL, "https://claude.com/") || login.Code != "" {
				t.Fatal("missing browser callback URL", err)
			}
			if mode == "cancel" {
				if err = client.Close(); err != nil {
					t.Fatal(err)
				}
				return
			}
			err = client.WaitLogin(ctx, login.ID)
			if (mode == "success") != (err == nil) {
				t.Fatalf("unexpected completion: %v", err)
			}
			client.Close()
			paths, _ := filepath.Glob(filepath.Join(home, ".login-*"))
			if len(paths) != 0 {
				t.Fatal("temporary OAuth URL retained")
			}
		})
	}
}
func TestInstalledClaudeBrowserHandoffWithoutAuthentication(t *testing.T) {
	if os.Getenv("LAZYBOX_V2_NATIVE_AUTH_CHECK") != "1" {
		t.Skip("explicit installed CLI pre-auth check")
	}
	binary := os.Getenv("LAZYBOX_V2_CLAUDE_BINARY")
	if binary == "" {
		t.Fatal("explicit native binary required")
	}
	home := filepath.Join(t.TempDir(), "private account")
	if err := os.Mkdir(home, 0700); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	client, err := OpenAccount(ctx, binary, []string{"PATH=" + os.Getenv("PATH"), "HOME=" + t.TempDir(), "CLAUDE_CONFIG_DIR=" + home, "DISABLE_AUTOUPDATER=1"}, home)
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close()
	login, err := client.Login(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if login.URL == "" || login.Code != "" {
		t.Fatal("browser callback not available")
	}
	// No browser is opened, no credentials submitted, and no model runs.
	if err = client.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestNativeManagedSubscriptionUsage(t *testing.T) {
	if os.Getenv("LAZYBOX_V2_NATIVE_USAGE_CHECK") != "1" {
		t.Skip("explicit read-only saved subscription usage check")
	}
	profiles, err := accounts.LoadProfiles(os.Getenv("LAZYBOX_V2_USAGE_STATE"))
	if err != nil {
		t.Fatal(err)
	}
	count := 0
	for _, profile := range profiles {
		if profile.Provider != "claude" {
			continue
		}
		count++
		env, err := accounts.Resolve(profile, os.Environ())
		if err != nil {
			t.Fatal(err)
		}
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		observation, err := ReadUsage(ctx, os.Getenv("LAZYBOX_V2_WORKER"), "", env, profile.Home)
		cancel()
		if err != nil {
			t.Fatalf("account %d: %v", count, err)
		}
		t.Logf("account %d: plan=%s state=%s windows=%+v", count, observation.Plan, observation.State, observation.Windows)
	}
	if count == 0 {
		t.Fatal("no managed Claude accounts")
	}
}
