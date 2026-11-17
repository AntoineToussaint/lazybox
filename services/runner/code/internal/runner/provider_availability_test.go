package runner

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
)

// modelsFixture is a fake `codex app-server` that lists models only when it
// runs without any account: a scratch CODEX_HOME holding nothing, and no API
// key in its environment. Anything beyond the listing is refused.
const modelsFixture = `#!/usr/bin/env python3
import sys,json,os
assert sys.argv[1:] == ['app-server','--stdio'], sys.argv
home=os.environ['CODEX_HOME']
assert home != os.environ.get('FIXTURE_REAL_HOME'), 'listed in the real CODEX_HOME'
assert os.listdir(home) == [], 'the scratch CODEX_HOME holds something'
assert 'OPENAI_API_KEY' not in os.environ and 'CODEX_API_KEY' not in os.environ, 'an account key reached the listing'
open(os.environ['FIXTURE_SEEN'],'w').write(home)
def send(m):print(json.dumps(m),flush=True)
for line in sys.stdin:
 m=json.loads(line)
 if 'id' not in m:continue
 if m['method']=='initialize':send({'id':m['id'],'result':{}});continue
 if m['method']=='model/list':send({'id':m['id'],'result':{'data':[{'id':'gpt-6.1-sol','model':'gpt-6.1-sol','displayName':'GPT-6.1-Sol','description':'Workhorse','hidden':False}],'nextCursor':None}});continue
 raise RuntimeError('Unexpected request: '+m['method'])
`

// The provider report lists Codex's models in a scratch CODEX_HOME, without
// the person's sign-in or API key, and removes it afterwards.
func TestCodexModelsAreListedWithoutAnyAccount(t *testing.T) {
	if _, err := exec.LookPath("python3"); err != nil {
		t.Skip("Python JSON-RPC fixture unavailable")
	}
	binary := filepath.Join(t.TempDir(), "codex")
	if err := os.WriteFile(binary, []byte(modelsFixture), 0o700); err != nil {
		t.Fatal(err)
	}
	real, seen := t.TempDir(), filepath.Join(t.TempDir(), "seen")
	t.Setenv("CODEX_HOME", real)
	t.Setenv("FIXTURE_REAL_HOME", real)
	t.Setenv("FIXTURE_SEEN", seen)
	t.Setenv("OPENAI_API_KEY", "sk-test")
	t.Setenv("CODEX_API_KEY", "ck-test")
	models, err := programModels(context.Background(), "codex", binary)
	if err != nil || len(models) != 1 || models[0].Value != "gpt-6.1-sol" || models[0].DisplayName != "GPT-6.1-Sol" {
		t.Fatalf("models %+v %v", models, err)
	}
	home, err := os.ReadFile(seen)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = os.Stat(string(home)); !os.IsNotExist(err) {
		t.Fatalf("scratch CODEX_HOME %s left behind: %v", home, err)
	}
	if _, err = programModels(context.Background(), "claude", binary); err == nil {
		t.Fatal("Claude Code listed models outside a session")
	}
}
