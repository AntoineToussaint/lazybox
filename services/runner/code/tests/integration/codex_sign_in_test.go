package integration

import (
	"context"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/codex"
)

// TestRealCodexBrowserSignInStartsAndCancels starts the installed Codex's
// browser sign-in in a scratch CODEX_HOME and cancels it: the page is
// OpenAI's, the callback is Codex's own loopback listener, no device code is
// involved, and nothing is saved. No sign-in completes and no browser opens.
func TestRealCodexBrowserSignInStartsAndCancels(t *testing.T) {
	if os.Getenv("LAZYBOX_V2_REAL") != "1" {
		t.Skip("requires explicit live-provider opt-in")
	}
	if selected := os.Getenv("LAZYBOX_V2_REAL_PROVIDER"); selected != "" && selected != "codex" {
		t.Skip("different provider selected")
	}
	binary, err := exec.LookPath("codex")
	if err != nil {
		t.Skip("codex is not installed")
	}
	home := t.TempDir()
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()
	native, err := codex.OpenAccount(ctx, binary, []string{"PATH=" + os.Getenv("PATH"), "HOME=" + os.Getenv("HOME"), "CODEX_HOME=" + home}, t.TempDir(), true)
	if err != nil {
		t.Fatal(err)
	}
	defer native.Close()
	login, err := native.Login(ctx, accounts.BrowserSignIn)
	if err != nil {
		t.Fatal(err)
	}
	page, err := url.Parse(login.URL)
	if err != nil || page.Host != "auth.openai.com" || login.Code != "" || login.ID == "" {
		t.Fatalf("browser sign-in: %q code=%q id=%q", login.URL, login.Code, login.ID)
	}
	callback, err := url.Parse(page.Query().Get("redirect_uri"))
	if err != nil || callback.Hostname() != "127.0.0.1" || !strings.HasSuffix(callback.Path, "/auth/callback") {
		t.Fatalf("callback is not Codex's loopback listener: %q", page.Query().Get("redirect_uri"))
	}
	if err = native.CancelLogin(ctx, login.ID); err != nil {
		t.Fatal(err)
	}
	if err = native.WaitLogin(ctx, login.ID); err == nil {
		t.Fatal("a cancelled sign-in completed")
	}
	if _, err = os.Stat(filepath.Join(home, "auth.json")); err == nil {
		t.Fatal("a cancelled sign-in saved credentials")
	}
}
