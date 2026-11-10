"""Verify Codefly/Buf output without relying on files already being in Git."""
from hashlib import sha256
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parent.parent

def snapshot():
    code = root / "services/runner/code"
    paths = [code / "main.go", code / "plugins/registry_gen.go"]
    paths += list((code / "pkg/adapters").glob("*_gen.go"))
    paths += list((code / "pkg/gen").rglob("*.go"))
    paths += list((root / "packages/protocol/src/gen").rglob("*.ts"))
    return {str(p.relative_to(root)): sha256(p.read_bytes()).hexdigest() for p in paths}

before = snapshot()
for command in [["codefly", "sync", "service", "runner"], ["node_modules/.bin/buf", "generate"]]:
    result = subprocess.run(command, cwd=root, capture_output=True, text=True)
    if result.returncode:
        print(result.stdout, result.stderr, file=sys.stderr)
        sys.exit(result.returncode)
after = snapshot()
changed = [p for p in before.keys() | after.keys() if before.get(p) != after.get(p)]
if changed:
    print("Generated output was stale; review regeneration:\n" + "\n".join(sorted(changed)), file=sys.stderr)
    sys.exit(1)
print("Codefly Go and Buf TypeScript generated output is current.")
